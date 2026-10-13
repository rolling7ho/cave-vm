//! `cave rm`: removes the project's VM, or a named fork or snapshot, after
//! asking, because in sealed mode `.cave/` can hold the only copy of the
//! work (docs/cli.md, "cave rm").

use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use cave_vm_proto::daemon::Request;

use crate::client::Connection;
use crate::commands::vm_id;
use crate::error::{CliError, Result};
use crate::project::{self, CONFIG_FILE, STATE_DIR};

/// Bytes `path` takes on disk, counting what sparse files really use.
fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let own = meta.blocks() * 512;
    if !meta.is_dir() {
        return own;
    }
    let children = fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| disk_usage(&entry.path()))
                .sum()
        })
        .unwrap_or(0);
    own + children
}

/// Whether `cave.toml` says `[storage] mode = "synced"`. Anything unclear
/// counts as sealed, so the warning is shown when in doubt.
fn is_synced(config: &str) -> bool {
    let mut in_storage = false;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_storage = line == "[storage]";
        } else if in_storage
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "mode"
        {
            return value.trim().trim_matches('"') == "synced";
        }
    }
    false
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1 << 30))
}

/// The question for removing the project's VM.
fn question(name: &str, used: u64, synced: bool) -> String {
    let mut text = format!(
        "Remove the VM for {name}? Its disk, snapshots and forks are deleted ({}).",
        gigabytes(used)
    );
    if !synced {
        text.push_str("\nThis project is sealed, so code that is only in the VM is lost.");
    }
    text.push_str(" [y/N] ");
    text
}

fn confirmed(question: &str) -> Result<bool> {
    eprint!("{question}");
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// `cave rm`.
pub fn run(name: Option<String>, yes: bool) -> Result<()> {
    let dir = project::require()?;
    let project_name = dir.file_name().map_or_else(
        || dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(CliError::new("cave rm needs a terminal to ask first")
                .fix("Add --yes to remove without asking, for scripts"));
        }
        let text = match &name {
            Some(name) => format!("Remove {name}? [y/N] "),
            None => {
                let config = fs::read_to_string(dir.join(CONFIG_FILE)).unwrap_or_default();
                question(
                    &project_name,
                    disk_usage(&dir.join(STATE_DIR)),
                    is_synced(&config),
                )
            }
        };
        if !confirmed(&text)? {
            return Err(CliError::new("nothing was removed"));
        }
    }
    Connection::call(&Request::Remove {
        vm: vm_id(&dir, None),
        name: name.clone(),
    })?;
    match name {
        Some(name) => println!("Removed {name}"),
        None => println!("Removed the VM for {project_name}; cave.toml is still here"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_mode_is_read_from_the_storage_table() {
        assert!(is_synced("[storage]\nmode = \"synced\"  # or sealed\n"));
        assert!(!is_synced(
            "[storage]\nmode    = \"sealed\"      # or \"synced\"\n"
        ));
        assert!(!is_synced("[network]\nmode = \"synced\"\n"));
        assert!(!is_synced(""));
    }

    #[test]
    fn the_question_warns_only_for_sealed_projects() {
        let sealed = question("app", 3 << 30, false);
        assert!(sealed.contains("(3.0 GB)"), "{sealed}");
        assert!(sealed.contains("is sealed"));
        assert!(!question("app", 0, true).contains("sealed"));
    }
}
