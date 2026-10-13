//! `cave snapshot` and `cave restore` (docs/cli.md).

use std::time::Instant;

use cave_vm_proto::daemon::{Reply, Request};

use crate::client::Connection;
use crate::commands::vm_id;
use crate::error::{CliError, Result};
use crate::policy;
use crate::project;

/// `cave snapshot [name]`.
pub fn snapshot(name: Option<String>, fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let started = Instant::now();
    let Reply::Snapshot { name, .. } = Connection::call(&Request::Snapshot {
        vm: vm_id(&project, fork),
        name,
    })?
    else {
        return Err(CliError::new(
            "the daemon sent an unexpected reply to snapshot",
        ));
    };
    println!(
        "Saved snapshot {name} ({})",
        policy::seconds(started.elapsed())
    );
    Ok(())
}

/// `cave restore [name]`.
pub fn restore(name: Option<String>, fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let started = Instant::now();
    let Reply::Restored {
        name,
        reason,
        saved,
        ..
    } = Connection::call(&Request::Restore {
        vm: vm_id(&project, fork),
        name,
    })?
    else {
        return Err(CliError::new(
            "the daemon sent an unexpected reply to restore",
        ));
    };
    if let Some(reason) = &reason {
        eprintln!("{}", fallback_warning(&name, reason));
    }
    println!("{}", restored(&name, started.elapsed(), saved.as_deref()));
    Ok(())
}

/// The warning for a restore that booted from the disk (docs/cli.md).
fn fallback_warning(name: &str, reason: &str) -> String {
    format!(
        "warning: could not load the memory state of {name} ({reason})\n  \
         restored the disk state and booted normally; processes from the snapshot were not kept"
    )
}

fn restored(name: &str, elapsed: std::time::Duration, saved: Option<&str>) -> String {
    let mut out = format!("Restored {name} ({})", policy::seconds(elapsed));
    if let Some(saved) = saved {
        out.push_str(&format!(
            "\nThe state before was saved as {saved}; `cave restore {saved}` goes back to it"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_messages_match_the_docs() {
        assert_eq!(
            fallback_warning("my-project-2026-10-12-debian", "format 1 is not supported"),
            "warning: could not load the memory state of my-project-2026-10-12-debian (format 1 is not supported)\n  \
             restored the disk state and booted normally; processes from the snapshot were not kept"
        );
        assert_eq!(
            restored(
                "before",
                Duration::from_millis(1400),
                Some("app-2026-10-13-debian")
            ),
            "Restored before (1.4s)\n\
             The state before was saved as app-2026-10-13-debian; `cave restore app-2026-10-13-debian` goes back to it"
        );
        assert_eq!(
            restored("before", Duration::from_millis(800), None),
            "Restored before (0.8s)"
        );
    }
}
