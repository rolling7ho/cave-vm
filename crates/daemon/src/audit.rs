//! The audit log: what was done in each VM, so you can see what an agent
//! did (design, "Audit log"). It records exec calls and file writes made
//! through the daemon, and network connections the VM's NAT saw.
//!
//! Each VM's log is in its folder: `.cave/` for the project's VM,
//! `.cave/forks/<name>/` for a fork. The newest entries are in
//! `audit-latest.log`; when it passes [`ROTATE_BYTES`] it is renamed
//! `audit-<YYYY-MM-DD>-<HHMMSS>.log`, for the local time it was renamed,
//! and a new one starts (D36). Each file holds one JSON object per line,
//! an [`AuditEntry`], appended with a single write so lines from parallel
//! operations never mix. Credentials appear by name only; the daemon never
//! has a value to write here.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use cave_vm_proto::daemon::{AuditEntry, AuditEvent, VmId};

use crate::project::STATE_DIR;
use crate::snapshot;

/// The newest entries' file name in a VM's folder.
pub const AUDIT_FILE: &str = "audit-latest.log";

/// The single log file of VMs from before D36. Renamed to [`AUDIT_FILE`]
/// on the next write.
const OLD_FILE: &str = "audit.log";

/// The size past which [`AUDIT_FILE`] is moved aside (D36).
pub const ROTATE_BYTES: u64 = 10 << 20;

/// Held while a log is appended to or moved aside, so two writers never
/// rotate the same file.
static WRITING: Mutex<()> = Mutex::new(());

/// The audit log of `vm`, or `None` for a fork name that cannot be one.
pub fn path(vm: &VmId) -> Option<PathBuf> {
    let state = Path::new(&vm.project).join(STATE_DIR);
    match &vm.fork {
        None => Some(state.join(AUDIT_FILE)),
        Some(name) => {
            snapshot::check_name(name).ok()?;
            Some(snapshot::fork_dir(&state, name).join(AUDIT_FILE))
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Appends `entries` to the log at `path`, an [`AUDIT_FILE`], in one
/// write. The file is moved aside first if it has passed [`ROTATE_BYTES`].
pub fn append(path: &Path, entries: &[AuditEntry]) -> io::Result<()> {
    append_rotating(path, entries, ROTATE_BYTES)
}

fn append_rotating(path: &Path, entries: &[AuditEntry], rotate_bytes: u64) -> io::Result<()> {
    let mut lines = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut lines, entry).map_err(io::Error::other)?;
        lines.push(b'\n');
    }
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        let old = dir.join(OLD_FILE);
        if !path.exists() && old.is_file() {
            fs::rename(&old, path)?;
        }
        if fs::metadata(path).is_ok_and(|m| m.len() >= rotate_bytes) {
            fs::rename(path, rotated_name(dir))?;
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&lines)
}

/// A free `audit-<YYYY-MM-DD>-<HHMMSS>.log` name in `dir` for now, with
/// `-2`, `-3` and so on when one was already used this second.
fn rotated_name(dir: &Path) -> PathBuf {
    let stamp = local_stamp();
    let mut path = dir.join(format!("audit-{stamp}.log"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("audit-{stamp}-{n}.log"));
        n += 1;
    }
    path
}

/// The local time as `2026-10-15-120405`, through date(1), since std has
/// no time zones.
fn local_stamp() -> String {
    Command::new("/bin/date")
        .arg("+%Y-%m-%d-%H%M%S")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| (now_ms() / 1000).to_string())
}

/// Sorts `audit-<stamp>.log` before `audit-<stamp>-2.log`, which a plain
/// name sort would not.
fn rotated_order(path: &Path) -> (String, u32) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let body = name.trim_start_matches("audit-").trim_end_matches(".log");
    match body.rsplit_once('-') {
        Some((stamp, n)) if stamp.len() == "2026-10-15-120405".len() => {
            (stamp.to_string(), n.parse().unwrap_or(1))
        }
        _ => (body.to_string(), 1),
    }
}

/// The log files in the folder of `latest`, oldest first: the moved-aside
/// files in name order, which is time order, then the old single file,
/// then `latest`.
fn files(latest: &Path) -> Vec<PathBuf> {
    let Some(dir) = latest.parent() else {
        return vec![latest.to_path_buf()];
    };
    let mut rotated: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("audit-") && n.ends_with(".log") && n != AUDIT_FILE)
        })
        .collect();
    rotated.sort_by_key(|p| rotated_order(p));
    rotated.push(dir.join(OLD_FILE));
    rotated.push(latest.to_path_buf());
    rotated
}

/// Appends `event`, timed `time_unix_ms`, to the log of `vm`.
pub fn record(vm: &VmId, time_unix_ms: u64, event: AuditEvent) {
    record_all(
        vm,
        &[AuditEntry {
            time_unix_ms,
            event,
        }],
    );
}

/// Appends `entries` to the log of `vm`. A log that cannot be written is
/// reported on the daemon's output, and the operations it records are not
/// undone.
pub fn record_all(vm: &VmId, entries: &[AuditEntry]) {
    let Some(path) = path(vm) else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    if let Err(e) = append(&path, entries) {
        eprintln!("cannot write the audit log {}: {e}", path.display());
    }
}

/// The last `limit` entries of the log whose newest file is `path` (all of
/// them when `limit` is `None`), across every file, oldest first, and the
/// number of entries in the log. A missing log is empty, and lines that
/// do not parse are skipped. Files older than the entries asked for are
/// only counted.
pub fn read(path: &Path, limit: Option<u32>) -> io::Result<(Vec<AuditEntry>, u64)> {
    let keep = limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let mut newest_first: Vec<Vec<AuditEntry>> = Vec::new();
    let mut kept = 0;
    let mut total = 0u64;
    for file in files(path).iter().rev() {
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if kept >= keep {
            total += text.lines().filter(|l| !l.trim().is_empty()).count() as u64;
            continue;
        }
        let entries: Vec<AuditEntry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        total += entries.len() as u64;
        kept += entries.len();
        newest_first.push(entries);
    }
    let mut entries: Vec<AuditEntry> = newest_first.into_iter().rev().flatten().collect();
    entries.drain(..entries.len().saturating_sub(keep));
    Ok((entries, total))
}

#[cfg(test)]
mod tests;
