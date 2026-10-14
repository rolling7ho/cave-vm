//! The audit log: what was done in each VM, so you can see what an agent
//! did (design, "Audit log"). It records exec calls and file writes made
//! through the daemon, and network connections the VM's NAT saw.
//!
//! Each VM's log is `audit.log` in its folder: `.cave/` for the project's
//! VM, `.cave/forks/<name>/` for a fork. It holds one JSON object per line,
//! an [`AuditEntry`], appended with a single write so lines from parallel
//! operations never mix. Credentials appear by name only; the daemon never
//! has a value to write here.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cave_vm_proto::daemon::{AuditEntry, AuditEvent, VmId};

use crate::project::STATE_DIR;
use crate::snapshot;

/// The log's file name in a VM's folder.
pub const AUDIT_FILE: &str = "audit.log";

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

/// Appends `entries` to the log at `path`, in one write.
pub fn append(path: &Path, entries: &[AuditEntry]) -> io::Result<()> {
    let mut lines = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut lines, entry).map_err(io::Error::other)?;
        lines.push(b'\n');
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&lines)
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

/// The last `limit` entries of the log at `path` (all of them when `limit`
/// is `None`), oldest first, and the number of entries in the log. A
/// missing log is empty, and lines that do not parse are skipped.
pub fn read(path: &Path, limit: Option<u32>) -> io::Result<(Vec<AuditEntry>, u64)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut entries: Vec<AuditEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let total = entries.len() as u64;
    if let Some(limit) = limit {
        let keep = usize::try_from(limit).unwrap_or(usize::MAX);
        entries.drain(..entries.len().saturating_sub(keep));
    }
    Ok((entries, total))
}

#[cfg(test)]
mod tests;
