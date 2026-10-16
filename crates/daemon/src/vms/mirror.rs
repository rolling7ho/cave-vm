//! `sync` for synced mode and `pull` (`cave push`) for both modes
//! (docs/cli.md, "cave sync" and "cave push"; P5.10): the I/O around the rules in [`crate::sync`]. The Mac
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
use crate::credentials::{self, Redactor};
use crate::project::STATE_DIR;
use crate::relay;
use crate::sync::{self, Exclusions, Hashes};

/// The project folder in the VM.
const WORKSPACE: &str = "/workspace";

/// The file in `.cave/` with the hashes of the last sync.
pub const STATE_FILE: &str = "sync.json";

/// The file in `.cave/` listing the files the sync redacted on the Mac,
/// each with its SHA-256 on the Mac, as redacted, and in the VM.
pub const REDACTED_FILE: &str = "sync-redacted.json";

/// Files the sync redacted: path, then the Mac's and the VM's SHA-256.
type Redacted = std::collections::BTreeMap<String, (String, String)>;

/// Files per `shasum` run on the Mac.
const HASH_BATCH: usize = 200;

/// Largest file the sync copies from the VM, held in memory while it is
/// redacted and written.
const MAX_SYNC_FILE: usize = 256 << 20;

/// Most stdout the daemon keeps from a command it runs in the VM, such as
/// the file listing, and most stderr, so a VM cannot fill its memory.
const MAX_RUN_STDOUT: usize = 256 << 20;
const MAX_RUN_STDERR: usize = 64 << 10;

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
    /// Changed only in the VM, but the VM may not change them on the Mac
    /// (D59).
    pub held: Vec<String>,
    /// Written with credential values replaced by their names.
    pub redacted: Vec<String>,
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
    error(ErrorCode::Io, format!("cannot {what}: {e}")).with_fix(
        "Check that you can write to the project folder and the disk has space, then try again",
    )
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
        let mut redacted = read_redacted(&project);
        let mac = as_synced(mac_hashes(&project, &exclusions)?, &redacted);
        let vm = self.vm_hashes(id, &exclusions)?;
        let plan = sync::plan_sync(&last, &mac, &vm);
        let known = self.held_values(id, &project)?;
        let mut redacted_now = Vec::new();
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
            let (data, mode) = self.read_vm_file(id, path)?;
            // Credentials the VM was given never reach the Mac's copy.
            let clean = redact(&known, &data);
            write_mac_file(&project, path, &clean, mode)?;
            if clean == data {
                redacted.remove(path);
            } else {
                let on_mac = sha256_hex(&clean).map_err(|e| io_error("hash a redacted file", e))?;
                redacted.insert(path.clone(), (on_mac, vm[path].clone()));
                redacted_now.push(path.clone());
            }
            after.insert(path.clone(), vm[path].clone());
        }
        for path in &plan.delete {
            remove_mac_file(&project, path)?;
            redacted.remove(path);
            after.remove(path);
        }
        write_redacted(&project, &redacted)?;
        write_state(&project, &sync::agreed(&last, &after, &vm))?;
        Ok(Synced {
            written: plan.write,
            deleted: plan.delete,
            conflicts: plan.conflicts,
            pending,
            held: plan.held,
            redacted: redacted_now,
        })
    }

    /// Brings Mac edits into the running VM, after a snapshot. In a
    /// sealed project, files deleted on the Mac are left in the VM, since
    /// the Mac's copy is not kept up to date there. With `force`, files
    /// changed on both sides take the Mac's version.
    pub fn pull(&self, id: &VmId, force: bool) -> Result<Pulled, ErrorMessage> {
        if id.fork.is_some() {
            return Err(
                error(ErrorCode::BadRequest, "forks have no Mac copy to push")
                    .with_fix("Push to the project's own VM, without --fork"),
            );
        }
        let project = PathBuf::from(&id.project);
        let sealed = self.source.policy(id)?.storage == Storage::Sealed;
        let _one_at_a_time = MIRROR.lock().unwrap_or_else(|e| e.into_inner());
        let exclusions = exclusions(&project);
        let state = project.join(STATE_DIR).join(STATE_FILE);
        if sealed && !force && !state.exists() {
            return Err(error(
                ErrorCode::Conflict,
                "cave has no record of the files it first copied into this VM, so it cannot tell your edits from the VM's",
            )
            .with_fix("Run cave push --force to copy every file that differs from your Mac into the VM, after a snapshot"));
        }
        let last = read_state(&project);
        let mut redacted = read_redacted(&project);
        let mac = as_synced(mac_hashes(&project, &exclusions)?, &redacted);
        let vm = self.vm_hashes(id, &exclusions)?;
        let mut plan = sync::plan_pull(&last, &mac, &vm, force).map_err(|files| {
            let mut e = error(
                ErrorCode::Conflict,
                format!(
                    "{} {} changed both on your Mac and in the VM since the last {}, so nothing was pushed",
                    files.len(),
                    if files.len() == 1 { "file" } else { "files" },
                    if sealed { "push" } else { "sync" }
                ),
            )
            .with_fix("keep one version of each file (copy it with cave cp, or undo the edit on your Mac) and run cave push again, or run cave push --force to overwrite the VM's version");
            e.files = Some(files);
            e
        })?;
        if sealed {
            plan.delete.clear();
        }
        if plan.write.is_empty() && plan.delete.is_empty() {
            write_state(&project, &sync::agreed(&last, &mac, &vm))?;
            return Ok(Pulled::default());
        }
        let (snapshot, _) = self.snapshot_before(id)?;
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
            redacted.remove(path);
        }
        write_redacted(&project, &redacted)?;
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
                    Stream::Stdout if out.len() + data.0.len() > MAX_RUN_STDOUT => {
                        return Err(io_error(
                            "run a command in the VM",
                            format!("it printed more than {} MiB", MAX_RUN_STDOUT >> 20),
                        ));
                    }
                    Stream::Stdout => out.extend(data.0),
                    Stream::Stderr => {
                        let room = MAX_RUN_STDERR.saturating_sub(err.len());
                        err.extend(data.0.into_iter().take(room));
                    }
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
            .filter(|(path, _)| sync::plain(path) && !exclusions.excluded(path, false))
            .collect())
    }

    /// The names and values of the credentials the running VM was given,
    /// to redact from what the sync writes to the Mac.
    fn held_values(
        &self,
        id: &VmId,
        project: &Path,
    ) -> Result<Vec<(String, String)>, ErrorMessage> {
        let vm = self.vm(id)?;
        let held: Vec<String> = match &*vm.slot.lock().expect("vm lock") {
            Slot::Running(running) => running.credentials.iter().cloned().collect(),
            _ => Vec::new(),
        };
        Ok(credentials::values_of(project, &held))
    }

    /// A file in `/workspace` and its mode.
    fn read_vm_file(&self, id: &VmId, path: &str) -> Result<(Vec<u8>, u32), ErrorMessage> {
        let request = Message::FileRead {
            path: format!("{WORKSPACE}/{path}"),
        };
        let mut agent = self.agent_request(id, &request)?;
        file_reply(&mut agent, path, MAX_SYNC_FILE)
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
/// Records the project folder's files as the VM's starting point, right
/// after the first-boot copy, so `cave push` can tell edits on the Mac
/// from edits in the VM.
pub(super) fn record_first_copy(project: &Path) -> Result<(), ErrorMessage> {
    let hashes = mac_hashes(project, &exclusions(project))?;
    write_state(project, &hashes)
}

fn exclusions(project: &Path) -> Exclusions {
    Exclusions::new(&fs::read_to_string(project.join(".gitignore")).unwrap_or_default())
}

/// The contents and mode of a file the agent on `agent` is sending, up to
/// `limit` bytes.
fn file_reply(
    agent: &mut UnixStream,
    path: &str,
    limit: usize,
) -> Result<(Vec<u8>, u32), ErrorMessage> {
    let (mut data, mut mode) = (Vec::new(), 0o644);
    let too_large = || {
        error(
            ErrorCode::Io,
            format!(
                "{path} in the VM is larger than {} MiB, so cave sync does not copy it",
                limit >> 20
            ),
        )
        .with_fix("Add it to .gitignore so the sync leaves it out, or copy it with `cave cp`")
    };
    loop {
        match relay::read_agent(agent)? {
            Message::FileInfo { size, .. } if size > limit as u64 => {
                return Err(too_large());
            }
            Message::FileInfo { mode: m, .. } => mode = m & 0o777,
            Message::FileData { data: chunk } if data.len() + chunk.0.len() > limit => {
                return Err(too_large());
            }
            Message::FileData { data: chunk } => data.extend(chunk.0),
            Message::FileEnd { .. } => return Ok((data, mode)),
            Message::Error(e) => return Err(e),
            other => return Err(io_error(&format!("read {path}"), format!("{other:?}"))),
        }
    }
}

/// `data` with each of `known`'s values replaced by its name.
fn redact(known: &[(String, String)], data: &[u8]) -> Vec<u8> {
    if known.is_empty() {
        return data.to_vec();
    }
    let mut redactor = Redactor::new(known);
    let mut out = redactor.push(data);
    out.extend(redactor.finish());
    out
}

/// `mac` with each file the sync redacted, unchanged since, at the VM's
/// hash, so the redaction is not taken for an edit made on the Mac.
fn as_synced(mut mac: Hashes, redacted: &Redacted) -> Hashes {
    for (path, (on_mac, in_vm)) in redacted {
        if mac.get(path) == Some(on_mac) {
            mac.insert(path.clone(), in_vm.clone());
        }
    }
    mac
}

fn read_redacted(project: &Path) -> Redacted {
    fs::read_to_string(project.join(STATE_DIR).join(REDACTED_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_redacted(project: &Path, redacted: &Redacted) -> Result<(), ErrorMessage> {
    let file = project.join(STATE_DIR).join(REDACTED_FILE);
    if redacted.is_empty() {
        return match fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(io_error("save the sync state", e))
            }
            _ => Ok(()),
        };
    }
    let text = serde_json::to_string_pretty(redacted).expect("JSON");
    write_atomic(&file, text.as_bytes(), 0o644).map_err(|e| io_error("save the sync state", e))
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

/// Checks that `path` stays inside the project and is one the VM may
/// change there: a plain relative path, not excluded or held, with no
/// symbolic link among its folders, so a write cannot leave the project.
fn inside(project: &Path, path: &str) -> Result<PathBuf, ErrorMessage> {
    let refused = |why: &str| {
        error(ErrorCode::BadRequest, format!("{why}: {path}"))
            .with_fix("Nothing was written there. Check the file in the VM with `cave exec ls -la`, then run cave sync again")
    };
    if !sync::plain(path) || path.starts_with('/') {
        return Err(refused("the VM listed a file outside the project"));
    }
    if exclusions(project).excluded(path, false) || sync::held(path) {
        return Err(refused(
            "the VM listed a file it may not change on your Mac",
        ));
    }
    let mut dir = project.to_path_buf();
    let segments: Vec<&str> = path.split('/').collect();
    for segment in &segments[..segments.len() - 1] {
        dir.push(segment);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(error(
                    ErrorCode::BadRequest,
                    format!("{} on your Mac is a symbolic link, so cave will not write {path} through it", dir.display()),
                )
                .with_fix("Replace the link with a folder, or delete it, then run cave sync again"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(io_error(&format!("check {}", dir.display()), e)),
        }
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
    // A link left at the temporary name is removed, never followed:
    // `create_new` (O_EXCL) does not follow one either.
    match fs::remove_file(&temp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
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
    fn a_file_past_the_limit_is_not_read_into_memory() {
        use cave_vm_proto::frame::write_frame;
        const LIMIT: usize = 1000;
        let reply = |messages: Vec<Message>| {
            let (mut daemon, mut agent) = UnixStream::pair().unwrap();
            for message in &messages {
                write_frame(&mut agent, message).unwrap();
            }
            file_reply(&mut daemon, "big.bin", LIMIT)
        };
        let info = |size| Message::FileInfo { size, mode: 0o640 };
        let data = |n| Message::FileData {
            data: vec![b'x'; n].into(),
        };
        let end = || Message::FileEnd {
            sha256: String::new(),
        };
        let e = reply(vec![info(LIMIT as u64 + 1)]).unwrap_err();
        assert!(e.message.contains("big.bin in the VM is larger than"));
        // A VM that understates the size is stopped at the limit too.
        let e = reply(vec![info(1), data(600), data(600)]).unwrap_err();
        assert!(e.message.contains("big.bin in the VM is larger than"));
        assert_eq!(
            reply(vec![info(LIMIT as u64), data(LIMIT), end()]).unwrap(),
            (vec![b'x'; LIMIT], 0o640)
        );
    }

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
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "././.env",
            "./a",
            ".GIT/config",
            "src/.Git/hooks/pre-commit",
            ".ENV",
            ".Cave/sync.json",
            "Cave.toml",
            ".claude/settings.json",
            ".CLAUDE/settings.local.json",
            ".mcp.j\u{17f}on",
            ".gitignore",
            "a\nb",
        ] {
            assert!(inside(p, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_redacted_file_is_not_taken_for_a_mac_edit() {
        let known = [("API_KEY".to_string(), "s3cret".to_string())];
        assert_eq!(redact(&known, b"key=s3cret\n"), b"key=[API_KEY]\n");
        assert_eq!(redact(&[], b"key=s3cret"), b"key=s3cret");
        let redacted = Redacted::from([("a".to_string(), ("r".to_string(), "v".to_string()))]);
        let mac = Hashes::from([
            ("a".to_string(), "r".to_string()),
            ("b".to_string(), "x".to_string()),
        ]);
        assert_eq!(as_synced(mac.clone(), &redacted)["a"], "v");
        // Edited on the Mac since: its own hash counts.
        let edited = Hashes::from([("a".to_string(), "e".to_string())]);
        assert_eq!(as_synced(edited, &redacted)["a"], "e");
    }

    #[test]
    fn the_mirror_is_never_written_through_a_link() {
        let dir = std::env::temp_dir().join(format!("cave-mirror-link-{}", std::process::id()));
        let outside = dir.with_extension("outside");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        let err = write_mac_file(&dir, "out/x.plist", b"x", 0o644).unwrap_err();
        assert!(err.message.contains("symbolic link"), "{err:?}");
        assert!(!outside.join("x.plist").exists());
        assert!(remove_mac_file(&dir, "out/x.plist").is_err());

        // A link at the temporary name is replaced, not written through.
        fs::create_dir_all(dir.join("src")).unwrap();
        std::os::unix::fs::symlink(outside.join("t"), dir.join("src/.a.cave-sync")).unwrap();
        write_mac_file(&dir, "src/a", b"y", 0o644).unwrap();
        assert_eq!(fs::read(dir.join("src/a")).unwrap(), b"y");
        assert!(!outside.join("t").exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
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
