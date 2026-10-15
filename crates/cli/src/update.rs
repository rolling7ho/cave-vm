//! cave updating itself (D45, decision 0018). The daemon's update detector
//! records the newest cave release in `updates.json`; before a command,
//! [`start_if_newer`] installs it in the background, the way this copy was
//! installed, when no VM is running. A daemon left from another version
//! is replaced by [`daemon_is_stale`] and [`stop_daemon`] once no VM runs.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cave_vm_proto::daemon::{Reply, Request, VmState};
use serde_json::{Value, json};

use crate::client::{Connection, data_dir};
use crate::project;
use crate::style::Paint;

/// The detector's record, in the data folder.
const RECORD: &str = "updates.json";

/// When an install was last started, and for which version, so a failing
/// one is tried again only a day later.
const TRIED: &str = "update-tried.json";

/// The install's output.
const LOG: &str = "update.log";

/// How long before an install of the same version is tried again.
const RETRY: u64 = 24 * 60 * 60;

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

/// The newest stable release, as the update detector last recorded it.
fn latest_stable() -> Option<String> {
    let record = read_json(&data_dir().ok()?.join(RECORD))?;
    record.get("cave")?.as_str().map(str::to_string)
}

/// What `cave --version` prints: this version, and the newest stable
/// release the update detector has seen (it looks once a day).
pub fn version_text() -> String {
    let latest = latest_stable().map_or_else(
        || "not looked up yet; cave looks once a day while a VM runs".to_string(),
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

/// Whether any VM is running, as the daemon says.
fn any_running() -> Option<bool> {
    match Connection::call(&Request::List { project: None }).ok()? {
        Reply::List { entries } => Some(entries.iter().any(|e| e.state == Some(VmState::Running))),
        _ => None,
    }
}

/// Before a command: if the detector has seen a newer cave, starts
/// installing it in the background and says so. Best effort; it never
/// stops the command.
pub fn start_if_newer() {
    let Ok(data) = data_dir() else { return };
    let Some(newest) = latest_stable() else {
        return;
    };
    if !newer(&newest, VERSION) {
        return;
    }
    let tried = read_json(&data.join(TRIED));
    let recently = tried.as_ref().is_some_and(|t| {
        t["version"] == newest.as_str()
            && now_unix().saturating_sub(t["at"].as_u64().unwrap_or(0)) < RETRY
    });
    if recently {
        return;
    }
    let off = project::current_dir()
        .ok()
        .and_then(|dir| project::find(&dir, project::home().as_deref()))
        .and_then(|p| fs::read_to_string(p.join(project::CONFIG_FILE)).ok())
        .is_some_and(|config| turned_off(&config));
    if off {
        return;
    }
    let Some(install) = std::env::current_exe()
        .and_then(fs::canonicalize)
        .ok()
        .and_then(|exe| install_of(&exe))
    else {
        return;
    };
    if any_running() != Some(false) {
        return;
    }
    let record = json!({ "version": newest, "at": now_unix() });
    if fs::write(data.join(TRIED), record.to_string() + "\n").is_err() {
        return;
    }
    let Ok(log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join(LOG))
    else {
        return;
    };
    let Ok(err) = log.try_clone() else { return };
    // nohup keeps the install going when this terminal closes.
    let started = Command::new("/usr/bin/nohup")
        .args(["/bin/sh", "-c", &install_command(&install)])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn();
    if started.is_ok() {
        eprintln!(
            "{} cave {newest} is out; installing it in the background (log: {}). The next command uses it",
            Paint::stderr().notice("update:"),
            data.join(LOG).display()
        );
    }
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
