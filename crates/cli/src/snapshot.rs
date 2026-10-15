//! `cave snapshot`, `cave restore` and `cave fork` (docs/cli.md).

use std::time::Instant;

use cave_vm_proto::daemon::{Reply, Request};

use crate::client::Connection;
use crate::commands::vm_id;
use crate::error::{CliError, Result};
use crate::policy;
use crate::project;
use crate::style::Paint;

/// `cave snapshot [name]`.
pub fn snapshot(name: Option<String>, fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let started = Instant::now();
    let Reply::Snapshot { name, .. } = Connection::call(&Request::Snapshot {
        vm: vm_id(&project, fork),
        name,
        before: false,
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
        credentials,
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
        eprintln!(
            "{}",
            Paint::stderr().warning(&fallback_warning(&name, reason))
        );
    }
    if !credentials.is_empty() {
        let source = format!("snapshot {name}");
        eprintln!(
            "{}",
            Paint::stderr().warning(&credential_warning(
                &source,
                &credentials,
                "the VM's memory"
            ))
        );
    }
    println!("{}", restored(&name, started.elapsed(), saved.as_deref()));
    Ok(())
}

/// `cave fork [name] [--from <snapshot>]`.
pub fn fork(name: Option<String>, from: Option<String>) -> Result<()> {
    let project = project::require()?;
    let started = Instant::now();
    let from_snapshot = from.is_some();
    let Reply::Forked {
        name,
        from,
        reason,
        credentials,
    } = Connection::call(&Request::Fork {
        vm: vm_id(&project, None),
        name,
        from,
    })?
    else {
        return Err(CliError::new("the daemon sent an unexpected reply to fork"));
    };
    if let Some(reason) = &reason {
        eprintln!("{}", fallback_warning(&from, reason));
    }
    if !credentials.is_empty() {
        // Without --from the fork is made from a fresh snapshot of the VM.
        let source = if from_snapshot {
            format!("snapshot {from}")
        } else {
            format!("the snapshot of {from}")
        };
        eprintln!(
            "{}",
            Paint::stderr().warning(&credential_warning(
                &source,
                &credentials,
                "the fork's memory"
            ))
        );
    }
    println!("{}", forked(&name, &from, started.elapsed()));
    Ok(())
}

fn forked(name: &str, from: &str, elapsed: std::time::Duration) -> String {
    format!(
        "Forked {name} from {from} ({}). Use it with: cave shell --fork {name}",
        policy::seconds(elapsed)
    )
}

/// The warning for a restore or fork that booted from the disk
/// (docs/cli.md).
fn fallback_warning(name: &str, reason: &str) -> String {
    format!(
        "warning: could not load the memory state of {name} ({reason})\n  \
         restored the disk state and booted normally; processes from the snapshot were not kept"
    )
}

/// The warning for a snapshot that recorded injected credentials
/// (docs/cli.md, "cave fork"): `source` was taken after `names` were
/// injected, so `holder` may contain them.
pub(crate) fn credential_warning(source: &str, names: &[String], holder: &str) -> String {
    let (verb, pronoun) = if names.len() == 1 {
        ("was", "it")
    } else {
        ("were", "them")
    };
    let names = match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!(
        "warning: {source} was taken after {names} {verb} injected\n  \
         {holder} may contain {pronoun}"
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
    fn the_credential_warning_matches_the_docs() {
        assert_eq!(
            credential_warning(
                "snapshot my-project-2026-10-12-debian",
                &["API_KEY".into()],
                "the fork's memory"
            ),
            "warning: snapshot my-project-2026-10-12-debian was taken after API_KEY was injected\n  \
             the fork's memory may contain it"
        );
        assert_eq!(
            credential_warning(
                "snapshot s",
                &["A".into(), "B".into(), "C".into()],
                "the exported files"
            ),
            "warning: snapshot s was taken after A, B and C were injected\n  \
             the exported files may contain them"
        );
    }

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
        assert_eq!(
            forked(
                "fork-1",
                "my-project-2026-10-12-debian",
                Duration::from_millis(300)
            ),
            "Forked fork-1 from my-project-2026-10-12-debian (0.3s). Use it with: cave shell --fork fork-1"
        );
    }
}
