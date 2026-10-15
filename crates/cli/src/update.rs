//! cave updating itself (D45, D47; decisions 0018 and 0020). When cave
//! runs in a terminal, [`offer_update`] looks up the newest stable release
//! in the background, and when one is newer than this copy it asks whether
//! to install it now, the way this copy was installed. A daemon left from
//! another version is replaced by [`daemon_is_stale`] and [`stop_daemon`]
//! once no VM runs.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::client::data_dir;
use crate::project;
use crate::style::Paint;

/// The image update detector's record, in the data folder. It also names
/// the newest cave it saw.
const RECORD: &str = "updates.json";

/// The latest release's `images.json`, as the last lookup fetched it. Its
/// `cave` key is the newest stable release.
const LATEST: &str = "latest.json";

/// Touched when a lookup starts, so they are at most [`LOOKUP_EVERY`] apart.
const LOOKED: &str = "latest.looked";

/// How long a lookup counts as fresh. Short, so a new release is offered
/// soon, but long enough that a script of cave commands looks up once.
const LOOKUP_EVERY: u64 = 10 * 60;

/// A declined update, so it is not asked about again until [`ASK_AGAIN`].
const DECLINED: &str = "update-declined.json";

/// How long after "no" cave asks about the same version again.
const ASK_AGAIN: u64 = 24 * 60 * 60;

/// The latest stable release's manifest. Pre-releases are never "latest".
const LATEST_URL: &str =
    "https://github.com/rolling7ho/cave-vm/releases/latest/download/images.json";

/// The release's install script, which installs the latest release.
const INSTALL_SCRIPT: &str =
    "https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh";

/// The Homebrew formula.
const FORMULA: &str = "rolling7ho/cave/cave-vm";

/// This cave's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How this copy was installed, and so how it updates.
#[derive(Debug, PartialEq, Eq)]
enum Install {
    /// By Homebrew, whose `brew` is this one.
    Brew(PathBuf),
    /// By `install.sh`, into this prefix (binaries in `<prefix>/bin`).
    Script(PathBuf),
}

/// What a release is before it is stable, in the order they come.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Beta,
    Rc,
    Stable,
}

/// A cave version: `x.y.z`, or `x.y.z-beta.N` or `x.y.z-rc.N` before it
/// is stable. Orders as releases come: 0.1.2-beta.1, 0.1.2-rc.1, 0.1.2.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    number: [u64; 3],
    stage: Stage,
    count: u64,
}

impl Version {
    fn parse(text: &str) -> Option<Version> {
        let text = text.trim().trim_start_matches('v');
        let (number, pre) = match text.split_once('-') {
            Some((number, pre)) => (number, Some(pre)),
            None => (text, None),
        };
        let parts: Vec<u64> = number
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        let number: [u64; 3] = parts.try_into().ok()?;
        let (stage, count) = match pre {
            None => (Stage::Stable, 0),
            Some(pre) => {
                let (stage, count) = pre.split_once('.')?;
                let stage = match stage {
                    "beta" => Stage::Beta,
                    "rc" => Stage::Rc,
                    _ => return None,
                };
                (stage, count.parse().ok()?)
            }
        };
        Some(Version {
            number,
            stage,
            count,
        })
    }

    /// As people read it: `v0.1.2`, `v0.1.2 RC 1`, `v0.1.3 Beta 1`.
    fn display(&self) -> String {
        let [major, minor, patch] = self.number;
        let base = format!("v{major}.{minor}.{patch}");
        match self.stage {
            Stage::Stable => base,
            Stage::Rc => format!("{base} RC {}", self.count),
            Stage::Beta => format!("{base} Beta {}", self.count),
        }
    }
}

/// `a` is a newer version than `b`.
fn newer(a: &str, b: &str) -> bool {
    match (Version::parse(a), Version::parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// `version` as people read it, or as it is if it is not a cave version.
fn display(version: &str) -> String {
    Version::parse(version).map_or_else(|| version.to_string(), |v| v.display())
}

/// The newest stable release cave has seen: from its own lookup, or else
/// from the image update detector's record.
fn latest_stable() -> Option<String> {
    let data = data_dir().ok()?;
    [LATEST, RECORD].iter().find_map(|file| {
        let doc = read_json(&data.join(file))?;
        doc.get("cave")?.as_str().map(str::to_string)
    })
}

/// What `cave --version` prints: this version, and the newest stable
/// release cave has seen.
pub fn version_text() -> String {
    let latest = latest_stable().map_or_else(
        || "not looked up yet; cave looks when you run it in a terminal".to_string(),
        |v| display(&v),
    );
    format!("{}\nLatest Stable: {latest}", display(VERSION))
}

/// How the `cave` at `exe` (already resolved) was installed; `None` for a
/// build folder, `cargo install` or anything else cave does not manage.
fn install_of(exe: &Path) -> Option<Install> {
    let text = exe.to_str()?;
    if let Some((prefix, _)) = text.split_once("/Cellar/cave-vm/") {
        let brew = Path::new(prefix).join("bin/brew");
        return brew.is_file().then_some(Install::Brew(brew));
    }
    if text.contains("/target/") || text.contains("/.cargo/") {
        return None;
    }
    let bin = exe.parent()?;
    let installed = exe.file_name()? == "cave"
        && bin.file_name()? == "bin"
        && ["cave-daemon", "cave-vmm"]
            .iter()
            .all(|f| bin.join(f).is_file());
    installed.then(|| Install::Script(bin.parent().unwrap_or(bin).to_path_buf()))
}

/// Single-quotes `text` for `sh`.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The shell command that installs the newest release.
fn install_command(install: &Install) -> String {
    match install {
        Install::Brew(brew) => {
            // The old version stays until `brew cleanup`, since a daemon
            // with running VMs may still be using it.
            let brew = quoted(&brew.to_string_lossy());
            format!("{brew} update && HOMEBREW_NO_INSTALL_CLEANUP=1 {brew} upgrade {FORMULA}")
        }
        Install::Script(prefix) => format!(
            "/usr/bin/curl -fsSL {INSTALL_SCRIPT} | CAVE_PREFIX={} /bin/sh",
            quoted(&prefix.to_string_lossy())
        ),
    }
}

/// The project's `cave.toml` turns updates off with `check = false` under
/// `[updates]`.
fn turned_off(config: &str) -> bool {
    let mut in_updates = false;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_updates = line == "[updates]";
        } else if in_updates
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "check"
        {
            return value.trim() == "false";
        }
    }
    false
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Seconds since `path` was last changed; `None` if it does not exist.
fn age(path: &Path) -> Option<u64> {
    let changed = fs::metadata(path).ok()?.modified().ok()?;
    let changed = changed.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now_unix().saturating_sub(changed))
}

/// Fetches the latest release's manifest into [`LATEST`] in the background,
/// unless a lookup started less than [`LOOKUP_EVERY`] ago. The command does
/// not wait for it; what it finds is offered the next time cave runs.
fn look_up_soon(data: &Path) {
    let looked = data.join(LOOKED);
    if age(&looked).is_some_and(|age| age < LOOKUP_EVERY) {
        return;
    }
    if fs::write(&looked, b"").is_err() {
        return;
    }
    // A plain GET with no identifying data, as the image detector's. nohup
    // keeps it going when the terminal closes right after the command.
    let _ = Command::new("/usr/bin/nohup")
        .args([
            "/bin/sh",
            "-c",
            r#"/usr/bin/curl -fsSL --max-time 20 -o "$1.part" "$2" && /bin/mv "$1.part" "$1""#,
            "sh",
        ])
        .arg(data.join(LATEST))
        .arg(LATEST_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The project's `cave.toml`, if cave runs in one, turns updates off.
fn updates_off() -> bool {
    project::current_dir()
        .ok()
        .and_then(|dir| project::find(&dir, project::home().as_deref()))
        .and_then(|p| fs::read_to_string(p.join(project::CONFIG_FILE)).ok())
        .is_some_and(|config| turned_off(&config))
}

/// `version` was declined less than [`ASK_AGAIN`] ago.
fn declined_recently(data: &Path, version: &str) -> bool {
    read_json(&data.join(DECLINED)).is_some_and(|d| {
        d["version"] == version
            && now_unix().saturating_sub(d["at"].as_u64().unwrap_or(0)) < ASK_AGAIN
    })
}

/// Before a command run in a terminal: looks up the newest stable release
/// in the background, and if one already seen is newer than this copy,
/// asks whether to install it now. After installing it, cave stops so the
/// command runs again on the new version. Scripts and pipes are never
/// asked and never look anything up.
pub fn offer_update() {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() || updates_off() {
        return;
    }
    let Ok(data) = data_dir() else { return };
    let _ = fs::create_dir_all(&data);
    look_up_soon(&data);
    let Some(newest) = latest_stable() else {
        return;
    };
    if !newer(&newest, VERSION) || declined_recently(&data, &newest) {
        return;
    }
    let Some(install) = std::env::current_exe()
        .and_then(fs::canonicalize)
        .ok()
        .and_then(|exe| install_of(&exe))
    else {
        return;
    };
    let paint = Paint::stderr();
    eprint!(
        "{} cave {} is out; you have {}. Update now? [Y/n] ",
        paint.notice("update:"),
        display(&newest),
        display(VERSION)
    );
    let _ = io::stderr().flush();
    let mut answer = String::new();
    // Ctrl-D, like an error, is not a yes.
    if !matches!(io::stdin().lock().read_line(&mut answer), Ok(n) if n > 0) {
        eprintln!();
        return;
    }
    if !matches!(answer.trim(), "" | "y" | "Y" | "yes" | "Yes") {
        let record = json!({ "version": newest, "at": now_unix() });
        let _ = fs::write(data.join(DECLINED), record.to_string() + "\n");
        eprintln!("Not now. cave asks again tomorrow");
        return;
    }
    let command = install_command(&install);
    eprintln!("Updating cave to {}", display(&newest));
    let installed = Command::new("/bin/sh")
        .args(["-c", &command])
        .status()
        .is_ok_and(|s| s.success());
    if installed {
        eprintln!(
            "Updated cave to {}. Run your command again to use it",
            display(&newest)
        );
        std::process::exit(0);
    }
    eprintln!(
        "{} the update did not finish; carrying on with {}. To update by hand: {command}",
        paint.warning("warning:"),
        display(VERSION)
    );
}

/// The daemon at the other end of a handshake that sent `software` is a
/// real cave daemon of another version.
pub fn daemon_is_stale(software: &str) -> bool {
    software
        .strip_prefix("cave-daemon ")
        .is_some_and(|version| version != VERSION)
}

/// Stops the daemon listening on `socket`, so the next connection starts
/// this version's. Call it only when no VM is running: the new daemon
/// stops the VMM processes it finds. Returns whether it stopped.
pub fn stop_daemon(socket: &Path) -> bool {
    let Ok(out) = Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg(socket)
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let pids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|pid| is_daemon(pid))
        .collect();
    let [pid] = pids.as_slice() else {
        return false;
    };
    let killed = Command::new("/bin/kill")
        .args(["-TERM", pid])
        .status()
        .is_ok_and(|s| s.success());
    if !killed {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_daemon(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    !is_daemon(pid)
}

/// Process `pid` is a cave daemon.
fn is_daemon(pid: &str) -> bool {
    Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", pid])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .ends_with("cave-daemon")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.1.1", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.1"));
        assert!(!newer("garbage", "0.1.0"));
    }

    #[test]
    fn betas_come_before_rcs_before_the_stable_release() {
        assert!(newer("0.1.2-rc.1", "0.1.2-beta.3"));
        assert!(newer("0.1.2-rc.2", "0.1.2-rc.1"));
        assert!(newer("0.1.2", "0.1.2-rc.2"));
        // An RC of 0.1.2 never goes back to the stable 0.1.1.
        assert!(!newer("0.1.1", "0.1.2-rc.1"));
        assert!(!newer("0.1.2-alpha.1", "0.1.1"));
    }

    #[test]
    fn versions_read_as_people_say_them() {
        assert_eq!(display("0.1.2-rc.1"), "v0.1.2 RC 1");
        assert_eq!(display("0.1.3-beta.1"), "v0.1.3 Beta 1");
        assert_eq!(display("0.1.1"), "v0.1.1");
        assert_eq!(display("odd"), "odd");
    }

    #[test]
    fn only_homebrew_and_script_copies_update() {
        let dir = std::env::temp_dir().join(format!("cave-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let brew = dir.join("homebrew/bin/brew");
        let cellar = dir.join("homebrew/Cellar/cave-vm/0.1.0/bin/cave");
        let script = dir.join("local/bin");
        for f in [&brew, &cellar] {
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, "").unwrap();
        }
        fs::create_dir_all(&script).unwrap();
        for f in ["cave", "cave-daemon", "cave-vmm"] {
            fs::write(script.join(f), "").unwrap();
        }
        assert_eq!(install_of(&cellar), Some(Install::Brew(brew)));
        assert_eq!(
            install_of(&script.join("cave")),
            Some(Install::Script(dir.join("local")))
        );
        assert_eq!(
            install_of(Path::new("/src/cave-vm/target/release/cave")),
            None
        );
        assert_eq!(install_of(&dir.join("elsewhere/bin/cave")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_install_commands() {
        assert_eq!(
            install_command(&Install::Brew("/opt/homebrew/bin/brew".into())),
            "'/opt/homebrew/bin/brew' update && HOMEBREW_NO_INSTALL_CLEANUP=1 '/opt/homebrew/bin/brew' upgrade rolling7ho/cave/cave-vm"
        );
        assert_eq!(
            install_command(&Install::Script("/Users/a b/.local".into())),
            "/usr/bin/curl -fsSL https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh | CAVE_PREFIX='/Users/a b/.local' /bin/sh"
        );
    }

    #[test]
    fn check_false_under_updates_turns_it_off() {
        assert!(turned_off(
            "[vm]\ncpus = 2\n[updates]\ncheck = false  # quiet\n"
        ));
        assert!(!turned_off("[updates]\ncheck = true\n"));
        assert!(!turned_off("[other]\ncheck = false\n"));
        assert!(!turned_off("[vm]\ncpus = 2\n"));
    }

    #[test]
    fn only_a_cave_daemon_of_another_version_is_stale() {
        assert!(daemon_is_stale("cave-daemon 0.0.9"));
        assert!(!daemon_is_stale(&format!("cave-daemon {VERSION}")));
        assert!(!daemon_is_stale("fake daemon"));
    }
}
