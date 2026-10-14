//! `cave sync` and `cave pull`: synced mode's Mac mirror (docs/cli.md,
//! "cave sync" and "cave pull"). The daemon does the work; this prints
//! what it did.

use cave_vm_proto::daemon::{Reply, Request, VmId};

use crate::client::Connection;
use crate::commands::vm_id;
use crate::error::{CliError, Result};
use crate::project;

/// Names listed in a line before the rest are counted.
const LISTED: usize = 10;

/// `cave sync`. Waits for the VM if it is not running.
pub fn sync() -> Result<()> {
    let vm = vm_id(&project::require()?, None);
    for line in sync_lines(&sync_vm(&vm)?) {
        println!("{line}");
    }
    Ok(())
}

/// Sends `sync` for `vm` and returns the reply.
pub fn sync_vm(vm: &VmId) -> Result<Reply> {
    match Connection::call(&Request::Sync { vm: vm.clone() })? {
        reply @ Reply::Synced { .. } => Ok(reply),
        _ => Err(CliError::new("the daemon sent an unexpected reply to sync")),
    }
}

/// `cave pull`.
pub fn pull() -> Result<()> {
    let vm = vm_id(&project::require()?, None);
    let Reply::Pulled {
        snapshot,
        applied,
        deleted,
    } = Connection::call(&Request::Pull { vm })?
    else {
        return Err(CliError::new("the daemon sent an unexpected reply to pull"));
    };
    if let Some(snapshot) = snapshot {
        println!("Saved snapshot {snapshot} before pulling");
    }
    if applied.is_empty() && deleted.is_empty() {
        println!("Nothing to pull: the VM has every edit from your Mac");
    }
    if !applied.is_empty() {
        println!(
            "Applied {} from your Mac: {}",
            files(&applied),
            list(&applied)
        );
    }
    if !deleted.is_empty() {
        println!(
            "Deleted {} in the VM to match your Mac: {}",
            files(&deleted),
            list(&deleted)
        );
    }
    Ok(())
}

/// What a `synced` reply says, one line each.
pub fn sync_lines(reply: &Reply) -> Vec<String> {
    let Reply::Synced {
        written,
        deleted,
        conflicts,
        pending,
    } = reply
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if written.is_empty() && deleted.is_empty() {
        lines.push("The Mac mirror is up to date".to_string());
    }
    if !written.is_empty() {
        lines.push(format!(
            "Wrote {} to your Mac: {}",
            files(written),
            list(written)
        ));
    }
    if !deleted.is_empty() {
        lines.push(format!(
            "Deleted {} on your Mac to match the VM: {}",
            files(deleted),
            list(deleted)
        ));
    }
    if !pending.is_empty() {
        lines.push(format!(
            "{} you changed on your Mac {} for `cave pull`: {}",
            capitalized(&files(pending)),
            if pending.len() == 1 { "waits" } else { "wait" },
            list(pending)
        ));
    }
    if !conflicts.is_empty() {
        lines.push(format!(
            "Left alone {} changed both on your Mac and in the VM: {}",
            files(conflicts),
            list(conflicts)
        ));
    }
    lines
}

fn files(paths: &[String]) -> String {
    match paths.len() {
        1 => "1 file".into(),
        n => format!("{n} files"),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `a, b, c`, or the first ten and how many more.
fn list(paths: &[String]) -> String {
    let mut text = paths
        .iter()
        .take(LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > LISTED {
        text.push_str(&format!(" and {} more", paths.len() - LISTED));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_sync_says_what_it_wrote_and_what_waits() {
        let reply = Reply::Synced {
            written: paths(&["src/app.ts", "README.md"]),
            deleted: paths(&["old.ts"]),
            conflicts: paths(&["package.json"]),
            pending: paths(&["notes.md"]),
        };
        assert_eq!(
            sync_lines(&reply),
            [
                "Wrote 2 files to your Mac: src/app.ts, README.md",
                "Deleted 1 file on your Mac to match the VM: old.ts",
                "1 file you changed on your Mac waits for `cave pull`: notes.md",
                "Left alone 1 file changed both on your Mac and in the VM: package.json",
            ]
        );
        let quiet = Reply::Synced {
            written: vec![],
            deleted: vec![],
            conflicts: vec![],
            pending: vec![],
        };
        assert_eq!(sync_lines(&quiet), ["The Mac mirror is up to date"]);
    }

    #[test]
    fn long_lists_are_cut_short() {
        let many: Vec<String> = (1..=12).map(|n| format!("f{n}")).collect();
        assert_eq!(
            list(&many),
            "f1, f2, f3, f4, f5, f6, f7, f8, f9, f10 and 2 more"
        );
    }
}
