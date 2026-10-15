//! `cave view`: the VM's audit log as it is written, so you can watch what
//! an agent does: each command, file write and connection (D39). It reads
//! `audit-latest.log` in the VM's folder and follows it, across the moves
//! aside that keep it small (D36).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use cave_vm_proto::daemon::AuditEntry;

use crate::commands::folder_name;
use crate::error::{CliError, Result};
use crate::inspect::{audit_line, local_offset};
use crate::project;
use crate::style::Paint;

/// Entries shown from before `cave view` started.
const EARLIER: usize = 10;

/// How often the log is checked for new entries.
const POLL: Duration = Duration::from_millis(300);

/// The newest audit file of the project's VM or of fork `fork`.
fn log_path(project: &Path, fork: Option<&str>) -> Result<PathBuf> {
    let state = project.join(".cave");
    Ok(match fork {
        None => state.join("audit-latest.log"),
        Some(name) if !name.is_empty() && !name.contains('/') && name != ".." => {
            state.join("forks").join(name).join("audit-latest.log")
        }
        Some(name) => {
            return Err(CliError::new(format!("no fork named \"{name}\""))
                .fix("Run cave ls to see this project's forks"));
        }
    })
}

/// Splits complete lines off the front of `pending` and returns the
/// entries they hold; a partial line waits for the rest.
fn take_entries(pending: &mut Vec<u8>) -> Vec<AuditEntry> {
    let Some(end) = pending.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = pending.drain(..=end).collect();
    String::from_utf8_lossy(&complete)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// `cave view [--fork <name>]`. Runs until interrupted.
pub fn run(fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let path = log_path(&project, fork.as_deref())?;
    let name = fork.clone().unwrap_or_else(|| folder_name(&project));
    eprintln!(
        "Following what happens in {name}: commands, file writes and connections. Ctrl-C stops"
    );
    let offset = local_offset();
    let paint = Paint::stdout();
    let mut file: Option<(File, u64)> = None;
    let mut pending = Vec::new();
    let mut first = true;
    loop {
        if file.is_none()
            && let Ok(f) = File::open(&path)
        {
            let inode = f.metadata().map(|m| m.ino()).unwrap_or(0);
            file = Some((f, inode));
        }
        if let Some((f, inode)) = &mut file {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            pending.extend_from_slice(&buf);
            let mut entries = take_entries(&mut pending);
            if first {
                entries.drain(..entries.len().saturating_sub(EARLIER));
            }
            for entry in &entries {
                println!("{}", audit_line(entry, offset, paint));
            }
            // Moved aside: what was left in the old file is read above; now
            // the new one.
            let current = fs::metadata(&path).map(|m| m.ino()).ok();
            if current.is_some_and(|c| c != *inode) {
                file = None;
                pending.clear();
            } else if current.is_some() {
                // A log written over from the start: read it again.
                let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if f.stream_position()? > len {
                    f.seek(SeekFrom::Start(0))?;
                }
            }
        }
        if first {
            first = false;
            if file.is_none() {
                eprintln!("Nothing recorded yet");
            }
        }
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_lines_wait_for_the_rest() {
        let line = r#"{"time_unix_ms":1000,"event":"exec","argv":["ls"],"exit_code":0}"#;
        let mut pending = format!("{line}\n{}", &line[..20]).into_bytes();
        assert_eq!(take_entries(&mut pending).len(), 1);
        assert_eq!(pending, &line.as_bytes()[..20]);
        pending.extend_from_slice(format!("{}\n", &line[20..]).as_bytes());
        assert_eq!(take_entries(&mut pending).len(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn fork_logs_are_in_the_forks_folder() {
        let p = Path::new("/p/app");
        assert_eq!(
            log_path(p, None).unwrap(),
            Path::new("/p/app/.cave/audit-latest.log")
        );
        assert_eq!(
            log_path(p, Some("try")).unwrap(),
            Path::new("/p/app/.cave/forks/try/audit-latest.log")
        );
        assert!(log_path(p, Some("../x")).is_err());
    }
}
