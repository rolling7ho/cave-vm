//! `cave agent <name>`: a coding agent against this project's VM
//! (docs/cli.md, "cave agent"; decision 0002).
//!
//! By default the agent runs on the Mac and reaches the VM only through
//! `cave mcp`, with host Bash denied, so the model API key stays on the
//! Mac. `--in-vm` runs it inside the VM on a pseudo-terminal instead,
//! installing it there with its own installer on first use. Either way a
//! snapshot is taken first, and the way back to it is printed at the end.

use std::path::{Path, PathBuf};
use std::process::Command;

use cave_vm_proto::daemon::{Reply, Request, Storage, VmId};
use serde_json::json;

use crate::client::Connection;
use crate::commands::{ensure_running, vm_id};
use crate::error::{CliError, Result};
use crate::exec::{self, EXIT_CAVE_FAILED_RUNNING};
use crate::init;
use crate::mirror;
use crate::shell;

/// The agents `cave agent` knows.
const AGENTS: [&str; 1] = ["claude"];

/// Installs Claude Code in the VM with its own installer, unless it is
/// there already.
const INSTALL_IN_VM: &str = "command -v claude >/dev/null || test -x \"$HOME/.local/bin/claude\" || \
     { echo 'Installing Claude Code in the VM with its installer'; \
     curl -fsSL https://claude.ai/install.sh | bash; }";

/// `cave agent`. Returns the agent's exit code.
pub fn run(name: &str, in_vm: bool, fork: Option<String>, args: Vec<String>) -> Result<u8> {
    let failed = |e: CliError| CliError {
        code: EXIT_CAVE_FAILED_RUNNING,
        ..e
    };
    if !AGENTS.contains(&name) {
        return Err(failed(
            CliError::new(format!("unknown agent \"{name}\""))
                .fix(format!("Supported agents: {}", AGENTS.join(", "))),
        ));
    }
    // Checked first, so nothing boots for an agent that cannot start.
    let on_mac = if in_vm {
        None
    } else {
        Some(find_on_path(name).ok_or_else(|| {
            failed(
                CliError::new(format!("{name} is not installed on this Mac")).fix(format!(
                    "Install Claude Code, or run it inside the VM with `cave agent {name} --in-vm`"
                )),
            )
        })?)
    };
    let project = init::project_or_set_up().map_err(failed)?;
    let vm = vm_id(&project, fork.clone());
    let policy = ensure_running(&vm, None, Vec::new(), true).map_err(failed)?;
    let snapshot = snapshot_before(&vm).map_err(failed)?;

    let code = match on_mac {
        Some(program) => {
            let cave = std::env::current_exe().map_err(|e| failed(e.into()))?;
            let status = Command::new(&program)
                .args(claude_args(&cave, fork.as_deref(), &args))
                .current_dir(&project)
                .status()
                .map_err(|e| {
                    failed(CliError::new(format!(
                        "cannot start {}: {e}",
                        program.display()
                    )))
                })?;
            exit_status_code(status)
        }
        None => {
            let installed = exec::run(
                fork.clone(),
                vec!["bash".into(), "-c".into(), INSTALL_IN_VM.into()],
            )?;
            if installed != 0 {
                return Err(failed(
                    CliError::new("could not install Claude Code in the VM")
                        .fix("Check the VM's network with `cave inspect`; the installer needs it"),
                ));
            }
            shell::session(vm.clone(), Some(in_vm_argv(&args)))?
        }
    };
    // The design syncs the mirror after agent runs. A failed sync does
    // not change how the run ended, so it is only reported.
    if policy.storage == Storage::Synced && fork.is_none() {
        match mirror::sync_vm(&vm) {
            Ok(reply) => mirror::sync_lines(&reply)
                .iter()
                .for_each(|line| eprintln!("{line}")),
            Err(e) => eprintln!("{e}"),
        }
    }
    if let Some(snapshot) = snapshot {
        eprintln!("{}", undo_lines(&snapshot));
    }
    Ok(code)
}

/// Takes the snapshot before the run, as the design asks. Forks cannot be
/// snapshotted yet, so a fork's run has none, and says so.
fn snapshot_before(vm: &VmId) -> Result<Option<String>> {
    if vm.fork.is_some() {
        eprintln!("No snapshot before this run: forks cannot be snapshotted yet");
        return Ok(None);
    }
    match Connection::call(&Request::Snapshot {
        vm: vm.clone(),
        name: None,
    })? {
        Reply::Snapshot { name, .. } => Ok(Some(name)),
        _ => Err(CliError::new(
            "the daemon sent an unexpected reply to snapshot",
        )),
    }
}

/// The arguments for Claude Code on the Mac: `cave mcp` as its MCP server,
/// for the fork if one is given, the Bash tool denied, then `extra`.
fn claude_args(cave: &Path, fork: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut mcp_args = vec!["mcp".to_string()];
    if let Some(fork) = fork {
        mcp_args.extend(["--fork".into(), fork.into()]);
    }
    let config = json!({"mcpServers": {"cave": {
        "type": "stdio",
        "command": cave.to_string_lossy(),
        "args": mcp_args,
    }}});
    let mut args = vec![
        "--mcp-config".to_string(),
        config.to_string(),
        "--disallowedTools".into(),
        "Bash".into(),
    ];
    args.extend(extra.iter().cloned());
    args
}

/// Claude Code in the VM, found where its installer puts it, with `extra`
/// passed through unchanged.
fn in_vm_argv(extra: &[String]) -> Vec<String> {
    let mut argv = vec![
        "bash".to_string(),
        "-lc".into(),
        "PATH=\"$HOME/.local/bin:$PATH\" exec claude \"$@\"".into(),
        "claude".into(),
    ];
    argv.extend(extra.iter().cloned());
    argv
}

/// The lines printed when the agent exits (docs/cli.md).
fn undo_lines(snapshot: &str) -> String {
    format!(
        "Snapshot before this run: {snapshot}\n\
         Undo everything the agent did with: cave restore {snapshot}"
    )
}

/// The first `name` on `PATH` that is a file.
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A process's exit code, or 128 plus the signal that ended it, as a
/// shell reports it.
fn exit_status_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code).unwrap_or(1),
        (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(1),
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_gets_cave_mcp_and_no_bash() {
        let args = claude_args(Path::new("/opt/cave"), None, &["-p".into(), "hi".into()]);
        assert_eq!(args[0], "--mcp-config");
        let config: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(
            config,
            json!({"mcpServers": {"cave": {"type": "stdio", "command": "/opt/cave", "args": ["mcp"]}}})
        );
        assert_eq!(args[2..], ["--disallowedTools", "Bash", "-p", "hi"]);

        let args = claude_args(Path::new("/opt/cave"), Some("try"), &[]);
        let config: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(
            config["mcpServers"]["cave"]["args"],
            json!(["mcp", "--fork", "try"])
        );
    }

    #[test]
    fn in_the_vm_arguments_pass_through_unquoted() {
        assert_eq!(
            in_vm_argv(&["-p".into(), "it's \"quoted\"".into()])[3..],
            ["claude", "-p", "it's \"quoted\""]
        );
    }

    #[test]
    fn the_undo_lines_match_the_docs() {
        assert_eq!(
            undo_lines("my-project-2026-10-12-debian-4"),
            "Snapshot before this run: my-project-2026-10-12-debian-4\n\
             Undo everything the agent did with: cave restore my-project-2026-10-12-debian-4"
        );
    }
}
