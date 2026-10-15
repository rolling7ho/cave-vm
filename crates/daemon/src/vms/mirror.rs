//! `sync` and `pull` for synced mode (docs/cli.md, "cave sync" and "cave
//! pull"; P5.10): the I/O around the rules in [`crate::sync`]. The Mac
//! mirror is the project folder itself (D14) and the VM's copy is
//! `/workspace`. The hashes both sides agreed on at the last sync are kept
//! in `.cave/sync.json`.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use cave_vm_proto::agent::{Exec, Message, Stream};
use cave_vm_proto::daemon::{AuditEntry, AuditEvent, Storage, VmId};
use cave_vm_proto::{ErrorCode, ErrorMessage, MAX_CHUNK, write_frame};

use super::{Slot, Vms, error};
use crate::audit;
use crate::project::STATE_DIR;
use crate::relay;
use crate::sync::{self, Exclusions, Hashes};

/// The project folder in the VM.
const WORKSPACE: &str = "/workspace";

/// The file in `.cave/` with the hashes of the last sync.
pub const STATE_FILE: &str = "sync.json";

/// Files per `shasum` run on the Mac.
const HASH_BATCH: usize = 200;

/// Lists every file in the VM's `/workspace` with its SHA-256, without
/// descending into the folders that never sync.
const VM_HASHES: &str = "cd /workspace && find . \\( -name node_modules -o -name target -o -name .git \
     -o -name .cave \\) -prune -o -type f -print0 | xargs -0 -r sha256sum";

/// Held through a sync or pull, so two never change the mirror or its
/// state at once.
static MIRROR: Mutex<()> = Mutex::new(());

/// A command's exit code, stdout and stderr.
type Ran = (Option<i32>, Vec<u8>, Vec<u8>);

/// What a sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    pub written: Vec<String>,
    pub deleted: Vec<String>,
    /// Changed on both sides, left alone.
    pub conflicts: Vec<String>,
    /// Changed only on the Mac, waiting for `pull`.
    pub pending: Vec<String>,
}

/// What a pull did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pulled {
    /// The snapshot taken first, if anything was applied.
    pub snapshot: Option<String>,
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
}

fn io_error(what: &str, e: impl std::fmt::Display) -> ErrorMessage {
    error(ErrorCode::Io, format!("cannot {what}: {e}"))
}

impl Vms {
    /// Checks that `id` is a synced project's own VM.
    fn mirrored(&self, id: &VmId) -> Result<PathBuf, ErrorMessage> {
        if id.fork.is_some() {
            return Err(error(ErrorCode::BadRequest, "forks have no Mac mirror")
                .with_fix("Sync the project's own VM, without --fork"));
        }
        if self.source.policy(id)?.storage != Storage::Synced {
            return Err(error(
                ErrorCode::BadRequest,
                "this project is sealed, so it has no Mac mirror",
            )
            .with_fix(
                "Set mode = \"synced\" under [storage] in cave.toml to keep a copy on your Mac",
            ));
        }
        Ok(PathBuf::from(&id.project))
    }

    /// Checks that `id` has a Mac mirror: it is a synced project's own
    /// VM.
    pub fn check_mirror(&self, id: &VmId) -> Result<(), ErrorMessage> {
        self.mirrored(id).map(drop)
    }

    /// Waits up to `timeout` for `id` to be running, and says whether it
    /// is.
    pub fn wait_running(&self, id: &VmId, timeout: Duration) -> Result<bool, ErrorMessage> {
        let vm = self.vm(id)?;
        let slot = vm.slot.lock().expect("vm lock");
        let (slot, _) = vm
            .changed
            .wait_timeout_while(slot, timeout, |s| !matches!(s, Slot::Running(_)))
            .expect("vm lock");
        Ok(matches!(*slot, Slot::Running(_)))
    }

    /// Updates the Mac mirror from the running VM.
    pub fn sync(&self, id: &VmId) -> Result<Synced, ErrorMessage> {
        let project = self.mirrored(id)?;
        let _one_at_a_time = MIRROR.lock().unwrap_or_else(|e| e.into_inner());
        let exclusions = exclusions(&project);
        let last = read_state(&project);
        let mac = mac_hashes(&project, &exclusions)?;
        let vm = self.vm_hashes(id, &exclusions)?;
        let plan = sync::plan_sync(&last, &mac, &vm);
        let pending = mac
            .keys()
            .chain(last.keys())
            .filter(|p| mac.get(*p) != last.get(*p) && vm.get(*p) == last.get(*p))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut after = mac.clone();
        for path in &plan.write {
            let data = self.read_vm_file(id, path)?;
            write_mac_file(&project, path, &data.0, data.1)?;
            after.insert(path.clone(), vm[path].clone());
        }
        for path in &plan.delete {
            remove_mac_file(&project, path)?;
            after.remove(path);
        }
        write_state(&project, &sync::agreed(&last, &after, &vm))?;
        Ok(Synced {
            written: plan.write,
            deleted: plan.delete,
            conflicts: plan.conflicts,
            pending,
        })
    }

    /// Brings Mac edits into the running VM, after a snapshot.
    pub fn pull(&self, id: &VmId) -> Result<Pulled, ErrorMessage> {
        let project = self.mirrored(id)?;
        let _one_at_a_time = MIRROR.lock().unwrap_or_else(|e| e.into_inner());
        let exclusions = exclusions(&project);
        let last = read_state(&project);
        let mac = mac_hashes(&project, &exclusions)?;
        let vm = self.vm_hashes(id, &exclusions)?;
        let plan = sync::plan_pull(&last, &mac, &vm).map_err(|files| {
            let mut e = error(
                ErrorCode::Conflict,
                format!(
                    "{} {} changed both on your Mac and in the VM since the last sync, so nothing was pushed",
                    files.len(),
                    if files.len() == 1 { "file" } else { "files" }
                ),
            )
            .with_fix("keep one version of each file (copy it with cave cp, or undo the edit on your Mac), then run cave push again");
            e.files = Some(files);
            e
        })?;
        if plan.write.is_empty() && plan.delete.is_empty() {
            write_state(&project, &sync::agreed(&last, &mac, &vm))?;
            return Ok(Pulled::default());
        }
        let (snapshot, _) = self.snapshot(id, None)?;
        let mut after = vm.clone();
        let mut written = Vec::new();
        for path in &plan.write {
            let full = project.join(path);
            let data = fs::read(&full).map_err(|e| io_error(&format!("read {path}"), e))?;
            let mode = fs::metadata(&full)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o644);
            let started = audit::now_ms();
            self.write_vm_file(id, path, &data, mode)?;
            written.push(AuditEntry {
                time_unix_ms: started,
                event: AuditEvent::Write {
                    path: format!("{WORKSPACE}/{path}"),
                    size: data.len() as u64,
                },
            });
            after.insert(path.clone(), mac[path].clone());
        }
        audit::record_all(id, &written);
        if !plan.delete.is_empty() {
            let mut argv = vec!["rm".to_string(), "-f".into(), "--".into()];
            argv.extend(plan.delete.iter().cloned());
            let (code, _, stderr) = self.run_vm(id, argv)?;
            if code != Some(0) {
                return Err(io_error(
                    "delete files in the VM",
                    String::from_utf8_lossy(&stderr).trim(),
                ));
            }
            for path in &plan.delete {
                after.remove(path);
            }
        }
        write_state(&project, &sync::agreed(&last, &mac, &after))?;
        Ok(Pulled {
            snapshot: Some(snapshot),
            applied: plan.write,
            deleted: plan.delete,
        })
    }

    /// An agent connection with `request` sent.
    fn agent_request(&self, id: &VmId, request: &Message) -> Result<UnixStream, ErrorMessage> {
        let (mut agent, _in_use) = self.agent_connection(id)?;
        relay::open(&mut agent, request)?;
        Ok(agent)
    }

    /// Runs `argv` in the VM and returns its exit code, stdout and stderr.
    fn run_vm(&self, id: &VmId, argv: Vec<String>) -> Result<Ran, ErrorMessage> {
        let exec = Message::Exec(Exec {
            argv,
            cwd: Some(WORKSPACE.into()),
            ..Exec::default()
        });
        let mut agent = self.agent_request(id, &exec)?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match relay::read_agent(&mut agent)? {
                Message::Output { stream, data } => match stream {
                    Stream::Stdout => out.extend(data.0),
                    Stream::Stderr => err.extend(data.0),
                },
                Message::Exit { code, .. } => return Ok((code, out, err)),
                Message::Error(e) => return Err(e),
                other => return Err(io_error("run a command in the VM", format!("{other:?}"))),
            }
        }
    }

    fn vm_hashes(&self, id: &VmId, exclusions: &Exclusions) -> Result<Hashes, ErrorMessage> {
        let argv = vec!["sh".to_string(), "-c".into(), VM_HASHES.into()];
        let (code, out, err) = self.run_vm(id, argv)?;
        if code != Some(0) {
            return Err(io_error(
                "list the files in the VM",
                String::from_utf8_lossy(&err).trim(),
            ));
        }
        Ok(parse_hashes(&String::from_utf8_lossy(&out))
            .into_iter()
            .filter(|(path, _)| !exclusions.excluded(path, false))
            .collect())
    }

    /// A file in `/workspace` and its mode.
    fn read_vm_file(&self, id: &VmId, path: &str) -> Result<(Vec<u8>, u32), ErrorMessage> {
        let request = Message::FileRead {
            path: format!("{WORKSPACE}/{path}"),
        };
        let mut agent = self.agent_request(id, &request)?;
        let (mut data, mut mode) = (Vec::new(), 0o644);
        loop {
            match relay::read_agent(&mut agent)? {
                Message::FileInfo { mode: m, .. } => mode = m & 0o777,
                Message::FileData { data: chunk } => data.extend(chunk.0),
                Message::FileEnd { .. } => return Ok((data, mode)),
                Message::Error(e) => return Err(e),
                other => return Err(io_error(&format!("read {path}"), format!("{other:?}"))),
            }
        }
    }

    fn write_vm_file(
        &self,
        id: &VmId,
        path: &str,
        data: &[u8],
        mode: u32,
    ) -> Result<(), ErrorMessage> {
        let request = Message::FileWrite {
            path: format!("{WORKSPACE}/{path}"),
            mode: Some(mode),
            create_dirs: true,
        };
        let mut agent = self.agent_request(id, &request)?;
        let failed = |e: &dyn std::fmt::Display| io_error(&format!("write {path} in the VM"), e);
        for chunk in data.chunks(MAX_CHUNK) {
            let message = Message::FileData {
                data: chunk.to_vec().into(),
            };
            write_frame(&mut agent, &message).map_err(|e| failed(&e))?;
        }
        let sha256 = sha256_hex(data).map_err(|e| failed(&e))?;
        write_frame(&mut agent, &Message::FileEnd { sha256 }).map_err(|e| failed(&e))?;
        match relay::read_agent(&mut agent)? {
            Message::FileWritten { .. } => Ok(()),
            Message::Error(e) => Err(e),
            other => Err(failed(&format!("{other:?}"))),
        }
    }
}

/// The project's exclusions, with its root `.gitignore`.
fn exclusions(project: &Path) -> Exclusions {
    Exclusions::new(&fs::read_to_string(project.join(".gitignore")).unwrap_or_default())
}

fn read_state(project: &Path) -> Hashes {
    fs::read_to_string(project.join(STATE_DIR).join(STATE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_state(project: &Path, hashes: &Hashes) -> Result<(), ErrorMessage> {
    let dir = project.join(STATE_DIR);
    let text = serde_json::to_string_pretty(hashes).expect("JSON");
    write_atomic(&dir.join(STATE_FILE), text.as_bytes(), 0o644)
        .map_err(|e| io_error("save the sync state", e))
}

/// `hash  ./path` lines from `sha256sum` or `shasum`. Names `sha256sum`
/// had to escape, which start with `\`, are skipped.
fn parse_hashes(text: &str) -> Hashes {
    text.lines()
        .filter(|line| !line.starts_with('\\'))
        .filter_map(|line| {
            let (hash, path) = line.split_once("  ").or_else(|| line.split_once(" *"))?;
            let path = path.strip_prefix("./").unwrap_or(path);
            (hash.len() == 64).then(|| (path.to_string(), hash.to_string()))
        })
        .collect()
}

/// Every regular file under `project` that syncs, with its SHA-256.
/// Symbolic links are skipped.
fn mac_hashes(project: &Path, exclusions: &Exclusions) -> Result<Hashes, ErrorMessage> {
    let mut files = Vec::new();
    walk(project, "", exclusions, &mut files).map_err(|e| io_error("read the Mac mirror", e))?;
    let mut hashes = Hashes::new();
    for batch in files.chunks(HASH_BATCH) {
        let out = Command::new("/usr/bin/shasum")
            .args(["-a", "256", "--"])
            .args(batch)
            .current_dir(project)
            .output()
            .map_err(|e| io_error("run shasum", e))?;
        if !out.status.success() {
            return Err(io_error(
                "hash the Mac mirror",
                String::from_utf8_lossy(&out.stderr).trim(),
            ));
        }
        hashes.extend(parse_hashes(&String::from_utf8_lossy(&out.stdout)));
    }
    Ok(hashes)
}

fn walk(dir: &Path, prefix: &str, x: &Exclusions, files: &mut Vec<String>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !x.excluded(&path, true) {
                walk(&entry.path(), &path, x, files)?;
            }
        } else if kind.is_file() && !x.excluded(&path, false) && !path.contains('\n') {
            files.push(path);
        }
    }
    Ok(())
}

/// Checks that `path` stays inside the project.
fn inside(project: &Path, path: &str) -> Result<PathBuf, ErrorMessage> {
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|s| s == ".." || s.is_empty())
    {
        return Err(error(
            ErrorCode::BadRequest,
            format!("the VM listed a file outside the project: {path}"),
        ));
    }
    Ok(project.join(path))
}

fn write_mac_file(project: &Path, path: &str, data: &[u8], mode: u32) -> Result<(), ErrorMessage> {
    let full = inside(project, path)?;
    write_atomic(&full, data, mode).map_err(|e| io_error(&format!("write {path} on your Mac"), e))
}

fn remove_mac_file(project: &Path, path: &str) -> Result<(), ErrorMessage> {
    let full = inside(project, path)?;
    match fs::remove_file(&full) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(io_error(&format!("delete {path} on your Mac"), e))
        }
        _ => Ok(()),
    }
}

/// Writes `path` through a temporary file beside it, so a reader never
/// sees half a file.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.cave-sync"));
    let mut file = fs::File::create(&temp)?;
    file.write_all(data)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)
}

/// SHA-256 as lowercase hex, from `shasum`, as the SDK computes it.
fn sha256_hex(data: &[u8]) -> io::Result<String> {
    let mut child = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| io::Error::other("shasum input"))??;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .filter(|h| h.len() == 64)
        .map(String::from)
        .ok_or_else(|| io::Error::other("shasum printed no hash"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_lines_from_both_tools_are_read() {
        let a = "a".repeat(64);
        let text = format!("{a}  ./src/app.ts\n{a} *bin/tool\n\\{a}  ./we\\nird\nshort  x\n");
        let hashes = parse_hashes(&text);
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes["src/app.ts"], a);
        assert_eq!(hashes["bin/tool"], a);
    }

    #[test]
    fn paths_from_the_vm_stay_inside_the_project() {
        let p = Path::new("/p");
        assert_eq!(inside(p, "src/a").unwrap(), Path::new("/p/src/a"));
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "a//b"] {
            assert!(inside(p, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_mac_side_is_walked_with_the_exclusions() {
        let dir = std::env::temp_dir().join(format!("cave-mirror-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for f in [
            "src/a.rs",
            "node_modules/x.js",
            "dist/out.js",
            ".env",
            "keep.txt",
        ] {
            write_atomic(&dir.join(f), b"x", 0o644).unwrap();
        }
        fs::write(dir.join(".gitignore"), "dist/\n").unwrap();
        let hashes = mac_hashes(&dir, &exclusions(&dir)).unwrap();
        assert_eq!(
            hashes.keys().map(String::as_str).collect::<Vec<_>>(),
            [".gitignore", "keep.txt", "src/a.rs"]
        );
        assert_eq!(hashes["keep.txt"], sha256_hex(b"x").unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
