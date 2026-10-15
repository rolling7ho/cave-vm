//! `cave agent <name>`: a coding agent against this project's VM
//! (docs/cli.md, "cave agent"; decision 0026).
//!
//! By default the agent runs inside the VM on a pseudo-terminal, installed
//! there with its own installer on first use, with its permission prompts
//! skipped, since its tools reach only the VM (D53). `--safe` keeps the
//! prompts. `--on-mac` runs it on the Mac instead, reaching the VM only
//! through `cave mcp`, with host Bash and reading `.env` denied, so the
//! model API key stays on the Mac; there the prompts are never skipped.
//! Either way a snapshot is taken first, and the way back to it is printed
//! at the end.

use std::path::{Path, PathBuf};
use std::process::Command;

use cave_vm_proto::daemon::{NetMode, Reply, Request, Storage, VmId};
use serde_json::json;

use crate::client::Connection;
use crate::commands::{ensure_running, vm_id};
use crate::error::{CliError, Result};
use crate::exec::{self, EXIT_CAVE_FAILED_RUNNING};
use crate::init;
use crate::mirror;
use crate::shell;
use crate::skill;

/// The agents `cave agent` knows.
const AGENTS: [&str; 2] = ["claude", "codex"];

/// Installs Claude Code in the VM with its own installer, unless it is
/// there already.
const INSTALL_CLAUDE_IN_VM: &str = "command -v claude >/dev/null || test -x \"$HOME/.local/bin/claude\" || \
     { echo 'Installing Claude Code in the VM with its installer'; \
     curl -fsSL https://claude.ai/install.sh | bash; }";

/// Installs Codex in the VM with npm, which the image has, unless it is
/// there already (D44).
const INSTALL_CODEX_IN_VM: &str = "command -v codex >/dev/null || \
     { echo 'Installing Codex in the VM with npm'; \
     sudo npm install -g @openai/codex; }";

/// The line printed before an agent starts with its prompts skipped.
const SKIPPED_LINE: &str = "Permissions skipped. The VM is the sandbox.";

/// The flag that skips agent `name`'s permission prompts. Checked with
/// Claude Code 2.1 and Codex 0.162.
fn skip_flag(name: &str) -> &'static str {
    match name {
        "codex" => "--dangerously-bypass-approvals-and-sandbox",
        _ => "--dangerously-skip-permissions",
    }
}

/// The agent arguments that would skip its prompts, refused on the Mac,
/// where its own tools reach the Mac (D53).
fn skips_prompts(name: &str, arg: &str) -> bool {
    let arg = arg.split('=').next().unwrap_or(arg);
    match name {
        "codex" => matches!(arg, "--dangerously-bypass-approvals-and-sandbox" | "--yolo"),
        _ => {
            matches!(
                arg,
                "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions"
            )
        }
    }
}

/// The agent's product name, for messages.
fn product(name: &str) -> &'static str {
    match name {
        "codex" => "Codex",
        _ => "Claude Code",
    }
}

/// How to install the agent on the Mac, for the error when it is missing.
fn install_hint(name: &str) -> &'static str {
    match name {
        "codex" => "Install Codex (npm install -g @openai/codex)",
        _ => "Install Claude Code",
    }
}

/// `cave agent`. Returns the agent's exit code.
pub fn run(
    name: &str,
    safe: bool,
    on_mac: bool,
    fork: Option<String>,
    args: Vec<String>,
) -> Result<u8> {
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
    let on_mac = if on_mac {
        if let Some(arg) = args.iter().find(|a| skips_prompts(name, a)) {
            return Err(failed(
                CliError::new(format!(
                    "{arg} is refused with --on-mac: the agent's own tools reach this Mac"
                ))
                .fix(format!(
                    "Run `cave agent {name}` without --on-mac, which skips the prompts inside the VM"
                )),
            ));
        }
        Some(find_on_path(name).ok_or_else(|| {
            failed(
                CliError::new(format!("{name} is not installed on this Mac")).fix(format!(
                    "{}, or run it inside the VM with `cave agent {name}`",
                    install_hint(name)
                )),
            )
        })?)
    } else {
        None
    };
    let project = init::project_or_set_up().map_err(failed)?;
    forget_terminal_recording(&project, fork.as_deref());
    let vm = vm_id(&project, fork.clone());
    let policy = ensure_running(&vm, None, Vec::new(), true).map_err(failed)?;
    if on_mac.is_none() && policy.net == NetMode::Off {
        return Err(failed(
            CliError::new(format!(
                "{} cannot reach its model API: this VM has no network",
                product(name)
            ))
            .fix(format!(
                "Run `cave stop`, set mode = \"nat\" under [network] in cave.toml if it says \"off\", then run it again; or use `cave agent {name} --on-mac`"
            )),
        ));
    }
    let snapshot = snapshot_before(&vm).map_err(failed)?;

    let code = match on_mac {
        Some(program) => {
            let cave = std::env::current_exe().map_err(|e| failed(e.into()))?;
            let status = Command::new(&program)
                .args(mac_args(name, &cave, fork.as_deref(), &args))
                .current_dir(&project)
                .status()
                .map_err(|e| {
                    failed(
                        CliError::new(format!("cannot start {}: {e}", program.display()))
                            .fix("Check that it runs on its own in a terminal, then try again"),
                    )
                })?;
            exit_status_code(status)
        }
        None => {
            let install = match name {
                "codex" => INSTALL_CODEX_IN_VM,
                _ => INSTALL_CLAUDE_IN_VM,
            };
            let installed = exec::run(
                fork.clone(),
                vec!["bash".into(), "-c".into(), install.into()],
            )?;
            if installed != 0 {
                return Err(failed(
                    CliError::new(format!("could not install {} in the VM", product(name)))
                        .fix("Check the VM's network with `cave inspect`; the installer needs it"),
                ));
            }
            if !safe {
                eprintln!("{SKIPPED_LINE}");
            }
            shell::session(vm.clone(), Some(in_vm_argv(name, safe, &args)))?
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

/// Deletes the agent's terminal output that `cave agent` 0.1.1 recorded
/// for the removed `cave mirror` (D46), so it does not linger.
fn forget_terminal_recording(project: &Path, fork: Option<&str>) {
    let state = project.join(".cave");
    let dir = match fork {
        None => state,
        Some(name) => state.join("forks").join(name),
    };
    let _ = std::fs::remove_file(dir.join("agent.term"));
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
        before: true,
    })? {
        Reply::Snapshot { name, .. } => Ok(Some(name)),
        _ => Err(CliError::unexpected("snapshot")),
    }
}

/// What Claude Code on the Mac may not use: a shell, and its Read tool on
/// `.env`, which holds the credentials (D33). Checked with Claude Code 2.1:
/// the Read rule covers Grep too.
pub const DENIED_TOOLS: [&str; 2] = ["Bash", "Read(./.env)"];

/// The arguments for agent `name` on the Mac.
fn mac_args(name: &str, cave: &Path, fork: Option<&str>, extra: &[String]) -> Vec<String> {
    match name {
        "codex" => codex_args(cave, fork, extra),
        _ => claude_args(cave, fork, extra),
    }
}

/// The Codex features that would give it a shell on the Mac. Turned off,
/// Codex has no shell and no tool that reads files on the Mac, the Codex
/// counterpart of [`DENIED_TOOLS`]; checked with Codex 0.162 (D44).
pub const CODEX_DISABLED: [&str; 2] = ["shell_tool", "unified_exec"];

/// The arguments for Codex on the Mac: `cave mcp` as its `cave` MCP
/// server, for the fork if one is given, with its tools approved, since
/// they act only on the VM; [`CODEX_DISABLED`] turned off; then `extra`.
fn codex_args(cave: &Path, fork: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut mcp_args = vec!["mcp".to_string()];
    if let Some(fork) = fork {
        mcp_args.extend(["--fork".into(), fork.into()]);
    }
    // `-c` values are TOML; a JSON string or array of strings is valid TOML.
    let toml = |v: serde_json::Value| v.to_string();
    let mut args = vec![
        "-c".to_string(),
        format!(
            "mcp_servers.cave.command={}",
            toml(json!(cave.to_string_lossy()))
        ),
        "-c".into(),
        format!("mcp_servers.cave.args={}", toml(json!(mcp_args))),
        "-c".into(),
        "mcp_servers.cave.default_tools_approval_mode=\"approve\"".into(),
        // Codex has no shell here to open `.cave/AGENTS.md`, which the
        // project's AGENTS.md points to (D37), so it gets the text itself.
        "-c".into(),
        format!("developer_instructions={}", toml(json!(skill::body()))),
    ];
    for feature in CODEX_DISABLED {
        args.extend(["--disable".into(), feature.into()]);
    }
    args.extend(extra.iter().cloned());
    args
}

/// The arguments for Claude Code on the Mac: `cave mcp` as its MCP server,
/// for the fork if one is given, [`DENIED_TOOLS`] denied, then `extra`.
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
    // One argument with `=`: the option takes a list, and would otherwise
    // swallow a prompt in `extra`.
    let denied = format!("--disallowedTools={}", DENIED_TOOLS.join(","));
    let mut args = vec!["--mcp-config".to_string(), config.to_string(), denied];
    args.extend(extra.iter().cloned());
    args
}

/// Agent `name` in the VM, found where its installer puts it, with its
/// prompts skipped unless `safe`, then `extra` passed through unchanged.
fn in_vm_argv(name: &str, safe: bool, extra: &[String]) -> Vec<String> {
    let mut argv = vec![
        "bash".to_string(),
        "-lc".into(),
        format!("PATH=\"$HOME/.local/bin:$PATH\" exec {name} \"$@\""),
        name.into(),
    ];
    if !safe {
        argv.push(skip_flag(name).into());
    }
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
    fn claude_gets_cave_mcp_and_no_bash_or_env() {
        let args = claude_args(Path::new("/opt/cave"), None, &["-p".into(), "hi".into()]);
        assert_eq!(args[0], "--mcp-config");
        let config: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(
            config,
            json!({"mcpServers": {"cave": {"type": "stdio", "command": "/opt/cave", "args": ["mcp"]}}})
        );
        assert_eq!(
            args[2..],
            ["--disallowedTools=Bash,Read(./.env)", "-p", "hi"]
        );

        let args = claude_args(Path::new("/opt/cave"), Some("try"), &[]);
        let config: serde_json::Value = serde_json::from_str(&args[1]).unwrap();
        assert_eq!(
            config["mcpServers"]["cave"]["args"],
            json!(["mcp", "--fork", "try"])
        );
    }

    #[test]
    fn codex_gets_cave_mcp_and_no_shell() {
        let args = codex_args(Path::new("/opt/cave"), Some("try"), &["hi".into()]);
        assert_eq!(
            args,
            [
                "-c",
                "mcp_servers.cave.command=\"/opt/cave\"",
                "-c",
                "mcp_servers.cave.args=[\"mcp\",\"--fork\",\"try\"]",
                "-c",
                "mcp_servers.cave.default_tools_approval_mode=\"approve\"",
                "-c",
                &format!("developer_instructions={}", json!(skill::body())),
                "--disable",
                "shell_tool",
                "--disable",
                "unified_exec",
                "hi",
            ]
        );
        assert_eq!(
            in_vm_argv("codex", true, &[])[2],
            "PATH=\"$HOME/.local/bin:$PATH\" exec codex \"$@\""
        );
    }

    #[test]
    fn in_the_vm_arguments_pass_through_unquoted() {
        assert_eq!(
            in_vm_argv("claude", true, &["-p".into(), "it's \"quoted\"".into()])[3..],
            ["claude", "-p", "it's \"quoted\""]
        );
    }

    #[test]
    fn in_the_vm_prompts_are_skipped_unless_safe() {
        assert_eq!(
            in_vm_argv("claude", false, &["-p".into(), "hi".into()])[3..],
            ["claude", "--dangerously-skip-permissions", "-p", "hi"]
        );
        assert_eq!(
            in_vm_argv("codex", false, &[])[3..],
            ["codex", "--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(in_vm_argv("codex", true, &[])[3..], ["codex"]);
    }

    #[test]
    fn flags_that_skip_prompts_are_recognised() {
        assert!(skips_prompts("claude", "--dangerously-skip-permissions"));
        assert!(skips_prompts(
            "claude",
            "--allow-dangerously-skip-permissions"
        ));
        assert!(skips_prompts("codex", "--yolo"));
        assert!(skips_prompts(
            "codex",
            "--dangerously-bypass-approvals-and-sandbox"
        ));
        assert!(!skips_prompts("claude", "--resume"));
        assert!(!skips_prompts("codex", "--dangerously-skip-permissions"));
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
