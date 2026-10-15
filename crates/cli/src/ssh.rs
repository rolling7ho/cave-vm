//! Remote editing (design, "Remote editing"; P5.9): sshd runs in the VM
//! and is reached over vsock through `cave ssh-proxy`, so editors that
//! expect SSH (VS Code Remote, Zed) connect to `<project>.cave` without the
//! user touching SSH.
//!
//! `cave` keeps an SSH key and one host entry per project in its data
//! directory, and `~/.ssh/config` includes those entries. The proxy starts
//! the VM if needed, puts the public key in the guest user's
//! `authorized_keys` the first time, then carries the SSH connection.

use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_proto::ErrorCode;
use cave_vm_proto::daemon::{Reply, Request, VmId};

use crate::client::{Connection, data_dir};
use crate::commands::vm_id;
use crate::error::{CliError, Result};

/// The user commands run as in the VM.
const GUEST_USER: &str = "cave";

/// Where the guest user's authorized keys are.
const AUTHORIZED_KEYS: &str = "/home/cave/.ssh/authorized_keys";

/// Adds the public key, given as `$1`, to the guest user's authorized keys.
const INSTALL_KEY: &str = "install -d -m 700 ~/.ssh && printf '%s\\n' \"$1\" >> ~/.ssh/authorized_keys \
     && chmod 600 ~/.ssh/authorized_keys";

/// The folder in the data directory with the key and the host entries.
fn ssh_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("ssh"))
}

/// The private key `cave` connects with, made with `ssh-keygen` the first
/// time.
fn ensure_key(dir: &Path) -> Result<PathBuf> {
    let key = dir.join("id_ed25519");
    if key.is_file() && key.with_extension("pub").is_file() {
        return Ok(key);
    }
    fs::create_dir_all(dir)?;
    let _ = fs::remove_file(&key);
    let _ = fs::remove_file(key.with_extension("pub"));
    let status = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "cave", "-f"])
        .arg(&key)
        .status()
        .map_err(|e| {
            CliError::new(format!("cannot run ssh-keygen: {e}"))
                .fix("macOS includes /usr/bin/ssh-keygen; run `cave doctor` to check the Mac")
        })?;
    if !status.success() {
        return Err(
            CliError::new("ssh-keygen could not make a key for remote editing")
                .fix("Run `cave doctor`"),
        );
    }
    Ok(key)
}

/// The SSH host name for `project`: its folder name, lowercased, with
/// anything but letters, digits and `-` as `-`, then `.cave`.
fn host_base(project: &Path) -> String {
    let name: String = project
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "project".into()
    } else {
        name.into()
    }
}

/// A double-quoted value for an SSH config line. `%` is doubled, since
/// SSH expands `%` tokens in `ProxyCommand`.
fn quoted(value: &Path, percent: bool) -> String {
    let text = value.to_string_lossy().replace('"', "\\\"");
    let text = if percent {
        text.replace('%', "%%")
    } else {
        text
    };
    format!("\"{text}\"")
}

/// The host entry that reaches `project`'s VM.
fn host_entry(host: &str, project: &Path, cave: &Path, key: &Path) -> String {
    format!(
        "# cave project: {project}\n\
         Host {host}\n\
         \x20 HostName {host}\n\
         \x20 User {GUEST_USER}\n\
         \x20 IdentityFile {key}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 StrictHostKeyChecking no\n\
         \x20 UserKnownHostsFile /dev/null\n\
         \x20 LogLevel ERROR\n\
         \x20 ProxyCommand {cave} ssh-proxy {project_arg}\n",
        project = project.display(),
        key = quoted(key, false),
        cave = quoted(cave, true),
        project_arg = quoted(project, true),
    )
}

/// The project a host entry file was written for.
fn entry_project(text: &str) -> Option<&str> {
    text.lines().next()?.strip_prefix("# cave project: ")
}

/// Writes `project`'s host entry, and returns its host name. A name taken
/// by another project gets `-2`, `-3` and so on.
fn write_entry(dir: &Path, project: &Path, cave: &Path, key: &Path) -> Result<String> {
    let hosts = dir.join("hosts");
    fs::create_dir_all(&hosts)?;
    let base = host_base(project);
    let wanted = project.to_string_lossy();
    for n in 1.. {
        let host = if n == 1 {
            format!("{base}.cave")
        } else {
            format!("{base}-{n}.cave")
        };
        let path = hosts.join(format!("{host}.conf"));
        let free = match fs::read_to_string(&path) {
            Ok(text) => entry_project(&text) == Some(&*wanted),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e.into()),
        };
        if free {
            fs::write(&path, host_entry(&host, project, cave, key))?;
            return Ok(host);
        }
    }
    unreachable!("a free host name")
}

/// Makes `~/.ssh/config` include the host entries in `dir`, at the top,
/// where an `Include` applies to every host. Returns whether it changed
/// the file.
fn ensure_include(home: &Path, dir: &Path) -> Result<bool> {
    let line = format!("Include {}", quoted(&dir.join("hosts/*.conf"), false));
    let ssh = home.join(".ssh");
    let config = ssh.join("config");
    let existing = match fs::read_to_string(&config) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    if !ssh.is_dir() {
        fs::create_dir_all(&ssh)?;
        set_mode(&ssh, 0o700)?;
    }
    let new = config.metadata().is_err();
    fs::write(
        &config,
        format!("# Added by cave, for remote editing\n{line}\n\n{existing}"),
    )?;
    if new {
        set_mode(&config, 0o600)?;
    }
    Ok(true)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Sets up remote editing for `project`: the key, its host entry and the
/// `Include` line. Returns the host name and whether `~/.ssh/config`
/// changed.
pub fn set_up(project: &Path) -> Result<(String, bool)> {
    let dir = ssh_dir()?;
    let key = ensure_key(&dir)?;
    let cave = std::env::current_exe()?;
    let host = write_entry(&dir, project, &cave, &key)?;
    let home = crate::project::home()
        .ok_or_else(|| CliError::new("HOME is not set").fix("Set HOME to your home folder"))?;
    let changed = ensure_include(&home, &dir)?;
    Ok((host, changed))
}

/// How long a VM that just booted gets to start sshd, which makes its
/// host keys on the first boot.
const SSHD_START_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection to sshd in `vm`, waiting for sshd to listen.
fn open_ssh(vm: &VmId) -> Result<UnixStream> {
    let deadline = Instant::now() + SSHD_START_TIMEOUT;
    loop {
        let mut connection = Connection::open()?;
        connection.send(&Request::Ssh { vm: vm.clone() })?;
        match connection.receive()? {
            Reply::Ok => return Ok(connection.into_stream()),
            Reply::Error(e) if e.code == ErrorCode::NotFound && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(250));
            }
            Reply::Error(e) => {
                return Err(CliError::from(e)
                    .fix("Check that sshd runs in the VM: cave exec systemctl status ssh"));
            }
            other => {
                return Err(CliError::unexpected(&format!("ssh: {other:?}")));
            }
        }
    }
}

/// `cave ssh-proxy <project>`: what the host entry's `ProxyCommand` runs.
/// Carries the SSH connection on stdin and stdout.
pub fn proxy(project: &Path, fork: Option<String>) -> Result<()> {
    let dir = ssh_dir()?;
    let key = ensure_key(&dir)?;
    let public = fs::read_to_string(key.with_extension("pub"))?;
    let public = public.trim();
    let vm = vm_id(project, fork.clone());

    let sandbox = cave_vm_sdk::Sandbox::open(project)?;
    let sandbox = match &fork {
        Some(name) => sandbox.fork_named(name),
        None => sandbox,
    };
    sandbox.start()?;
    let installed = sandbox
        .read_file(AUTHORIZED_KEYS)
        .map(|keys| {
            String::from_utf8_lossy(&keys)
                .lines()
                .any(|l| l.trim() == public)
        })
        .unwrap_or(false);
    if !installed {
        let command = cave_vm_sdk::Command::new("sh").args(["-c", INSTALL_KEY, "sh", public]);
        let out = sandbox.run(&command)?;
        if !out.success() {
            return Err(CliError::new(format!(
                "cannot add the remote editing key in the VM: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
            .fix(
                "Check the `cave` user's ~/.ssh folder in the VM with `cave shell`, then try again",
            ));
        }
    }

    let stream = open_ssh(&vm)?;
    let mut up = stream.try_clone()?;
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });
    // Stdout is line buffered, and SSH packets are not lines, so each
    // read is flushed at once.
    let mut down = stream;
    let mut stdout = io::stdout().lock();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match down.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
}

impl From<cave_vm_sdk::Error> for CliError {
    fn from(e: cave_vm_sdk::Error) -> CliError {
        CliError::new(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cave-ssh-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn host_names_come_from_the_folder() {
        assert_eq!(host_base(Path::new("/Users/ivan/My Project")), "my-project");
        assert_eq!(host_base(Path::new("/p/app_2")), "app-2");
        assert_eq!(host_base(Path::new("/")), "project");
    }

    #[test]
    fn the_entry_quotes_paths_and_escapes_percent_signs() {
        let entry = host_entry(
            "app.cave",
            Path::new("/Users/ivan/100% app"),
            Path::new("/opt/cave/bin/cave"),
            Path::new("/Users/ivan/Library/Application Support/cave-vm/ssh/id_ed25519"),
        );
        assert_eq!(
            entry,
            "# cave project: /Users/ivan/100% app\n\
             Host app.cave\n  HostName app.cave\n  User cave\n  \
             IdentityFile \"/Users/ivan/Library/Application Support/cave-vm/ssh/id_ed25519\"\n  \
             IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  \
             LogLevel ERROR\n  \
             ProxyCommand \"/opt/cave/bin/cave\" ssh-proxy \"/Users/ivan/100%% app\"\n"
        );
    }

    #[test]
    fn each_project_keeps_its_name_and_others_get_a_counter() {
        let dir = temp("entries");
        let (cave, key) = (Path::new("/c"), Path::new("/k"));
        let a = Path::new("/one/app");
        let b = Path::new("/two/app");
        assert_eq!(write_entry(&dir, a, cave, key).unwrap(), "app.cave");
        assert_eq!(write_entry(&dir, b, cave, key).unwrap(), "app-2.cave");
        assert_eq!(write_entry(&dir, a, cave, key).unwrap(), "app.cave");
        let text = fs::read_to_string(dir.join("hosts/app-2.cave.conf")).unwrap();
        assert_eq!(entry_project(&text), Some("/two/app"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_include_goes_first_once_and_keeps_the_rest() {
        let home = temp("include");
        let dir = Path::new("/data/ssh");
        assert!(ensure_include(&home, dir).unwrap());
        let config = home.join(".ssh/config");
        let line = "Include \"/data/ssh/hosts/*.conf\"";
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            format!("# Added by cave, for remote editing\n{line}\n\n")
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            config.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(&config, "Host gh\n  User git\n").unwrap();
        assert!(ensure_include(&home, dir).unwrap());
        assert!(!ensure_include(&home, dir).unwrap());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            format!("# Added by cave, for remote editing\n{line}\n\nHost gh\n  User git\n")
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_key_is_made_once() {
        let dir = temp("key");
        let key = ensure_key(&dir).unwrap();
        let public = fs::read_to_string(key.with_extension("pub")).unwrap();
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        ensure_key(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(key.with_extension("pub")).unwrap(),
            public
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
