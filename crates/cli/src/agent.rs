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
use crate::links;
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

/// What an agent in the VM is told, since the project's skill and
/// `AGENTS.md` line are written for agents on the Mac: where it is, and
/// the few steps only the user can take on the Mac, so it can pass them
/// on. The VM cannot change its own size or policy.
const IN_VM_NOTE: &str = concat!(
    "You are running inside this project's cave VM, a disposable Linux machine, and /workspace is the project. ",
    "Run commands and edit files directly with your own tools. ",
    "Instructions that say to use `cave` commands or the cave MCP tools are for agents on the Mac; ignore them here, and do not look for .cave/AGENTS.md.\n\n",
    "`cave` runs only on the user's Mac, and nothing in the VM can change the VM's size, network or settings. ",
    "When a task needs one of these, stop and tell the user the exact step, in the project folder on the Mac:\n",
    "- Out of disk space (\"No space left on device\"; check with `df -h /`): raise `disk` under `[storage]` in cave.toml, for example `disk = \"40G\"`.\n",
    "- Out of memory (processes killed, \"Out of memory\" in `dmesg`; check with `free -h`): raise `memory` under `[vm]` in cave.toml, for example `memory = \"4G\"`, or `cpus` for more CPUs.\n",
    "- After changing cave.toml: quit the agent, run `cave stop`, then start it again, such as `cave agent claude`. The VM boots from its disk with the new size; files stay, running processes stop.\n",
    "- A host the network policy blocks: `[network] mode` in cave.toml, then `cave stop` and start again; for the local network, `cave stop`, then `cave run --net lan`.\n",
    "- Copy the project out to a folder on the Mac: `cave pull`. Bring edits made on the Mac in: `cave push`.\n",
    "- Save the VM's state: `cave snapshot`. Undo this run: `cave restore` with the snapshot name `cave agent` printed before you started.",
);

/// The arguments that keep the project's Mac-side setup away from agent
/// `name` in the VM: Claude Code reads only its user settings, so the
/// project's deny rules and `.mcp.json` server (written for the Mac) do
/// not apply, and both agents get [`IN_VM_NOTE`].
fn in_vm_setup(name: &str) -> Vec<String> {
    match name {
        "codex" => vec![
            "-c".into(),
            format!("developer_instructions={}", json!(IN_VM_NOTE)),
        ],
        _ => vec![
            "--setting-sources".into(),
            "user".into(),
            "--strict-mcp-config".into(),
            "--append-system-prompt".into(),
            IN_VM_NOTE.into(),
        ],
    }
}

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

/// The first of the agent's arguments that would skip its prompts or give
/// it back a tool `cave` turned off, refused on the Mac, where its own
/// tools reach the Mac (D53). Returned as written, with its value when it
/// takes one. Arguments after `--` are a prompt and are not checked.
/// Checked with Claude Code 2.1 and Codex 0.162.
fn refused_arg(name: &str, args: &[String]) -> Option<String> {
    let mut args = args.iter().map(String::as_str).take_while(|a| *a != "--");
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg, None),
        };
        // Codex's short options also take their value joined: `-anever`.
        let (flag, inline) = match (name, flag.get(..2)) {
            ("codex", Some(short @ ("-a" | "-c" | "-s"))) if flag.len() > 2 && inline.is_none() => {
                (short, Some(&flag[2..]))
            }
            _ => (flag, inline),
        };
        // The value given as the next argument, kept for the message.
        let mut next = None;
        let mut value = |takes: &[&str]| -> Option<&str> {
            if !takes.contains(&flag) {
                return None;
            }
            inline.or_else(|| {
                next = args.next();
                next
            })
        };
        let refused = match name {
            "codex" => {
                matches!(
                    flag,
                    "--dangerously-bypass-approvals-and-sandbox" | "--yolo" | "--full-auto"
                ) || value(&["-a", "--ask-for-approval"]) == Some("never")
                    || value(&["-s", "--sandbox"]) == Some("danger-full-access")
                    || value(&["--enable"]).is_some_and(|f| CODEX_DISABLED.contains(&f))
                    || value(&["-c", "--config"]).is_some_and(codex_setting_refused)
            }
            _ => {
                matches!(
                    flag,
                    "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions"
                ) || value(&["--permission-mode"])
                    .is_some_and(|mode| !matches!(mode, "default" | "plan"))
            }
        };
        if refused {
            return Some(match next {
                Some(next) => format!("{arg} {next}"),
                None => arg.to_string(),
            });
        }
    }
    None
}

/// Whether a Codex `-c key=value` setting would change what `cave` set up
/// on the Mac: its approvals, its sandbox, its features or the `cave`
/// server.
fn codex_setting_refused(setting: &str) -> bool {
    let key = setting.split('=').next().unwrap_or(setting).trim();
    key == "approval_policy"
        || key == "sandbox_mode"
        || key == "features"
        || key.starts_with("features.")
        || key.starts_with("sandbox_workspace_write")
        || key == "mcp_servers"
        || key == "mcp_servers.cave"
        || key.starts_with("mcp_servers.cave.")
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
            CliError::new(format!("unknown agent \"{name}\"")).fix(format!(
                "Supported agents: {}. Run any other agent in the VM with cave shell or cave exec",
                AGENTS.join(", ")
            )),
        ));
    }
    // Checked first, so nothing boots for an agent that cannot start.
    let on_mac = if on_mac {
        if let Some(arg) = refused_arg(name, &args) {
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
                vec![
                    "bash".into(),
                    "-c".into(),
                    format!("{}\n{install}", links::INSTALL_HELPER),
                ],
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
            shell::session(vm.clone(), Some(in_vm_argv(name, safe, &args)), true)?
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

/// What Claude Code on the Mac may not use: a shell, its Read tool on
/// `.env`, which holds the credentials (D33), and its tools that edit
/// files on the Mac, whose edits never reach the VM (D55). Checked with
/// Claude Code 2.1: the Read rule covers Grep too.
pub const DENIED_TOOLS: [&str; 5] = ["Bash", "Read(./.env)", "Edit", "Write", "NotebookEdit"];

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
/// `BROWSER` is the helper that opens sign-in links on the Mac (D56), and
/// [`in_vm_setup`] comes before `extra`.
fn in_vm_argv(name: &str, safe: bool, extra: &[String]) -> Vec<String> {
    let mut argv = vec![
        "bash".to_string(),
        "-lc".into(),
        format!(
            "PATH=\"$HOME/.local/bin:$PATH\" BROWSER=\"{}\" exec {name} \"$@\"",
            links::HELPER_PATH
        ),
        name.into(),
    ];
    if !safe {
        argv.push(skip_flag(name).into());
    }
    argv.extend(in_vm_setup(name));
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
            [
                "--disallowedTools=Bash,Read(./.env),Edit,Write,NotebookEdit",
                "-p",
                "hi"
            ]
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
            "PATH=\"$HOME/.local/bin:$PATH\" BROWSER=\"$HOME/.local/bin/cave-open\" exec codex \"$@\""
        );
    }

    #[test]
    fn in_the_vm_arguments_pass_through_unquoted() {
        assert_eq!(
            in_vm_argv("claude", true, &["-p".into(), "it's \"quoted\"".into()])[9..],
            ["-p", "it's \"quoted\""]
        );
    }

    #[test]
    fn in_the_vm_prompts_are_skipped_unless_safe() {
        assert_eq!(
            in_vm_argv("claude", false, &["-p".into(), "hi".into()])[3..],
            [
                "claude",
                "--dangerously-skip-permissions",
                "--setting-sources",
                "user",
                "--strict-mcp-config",
                "--append-system-prompt",
                IN_VM_NOTE,
                "-p",
                "hi"
            ]
        );
        let note = format!("developer_instructions={}", json!(IN_VM_NOTE));
        assert_eq!(
            in_vm_argv("codex", false, &[])[3..],
            [
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                &note
            ]
        );
        assert_eq!(in_vm_argv("codex", true, &[])[3..], ["codex", "-c", &note]);
    }

    fn refused(name: &str, args: &[&str]) -> Option<String> {
        refused_arg(
            name,
            &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn claude_flags_that_skip_prompts_are_refused() {
        for args in [
            &["--dangerously-skip-permissions"][..],
            &["--allow-dangerously-skip-permissions"],
            &["--dangerously-skip-permissions=true"],
            &["--permission-mode", "bypassPermissions"],
            &["--permission-mode=dontAsk"],
            &["--resume", "--permission-mode", "acceptEdits"],
        ] {
            assert!(refused("claude", args).is_some(), "{args:?}");
        }
        assert_eq!(
            refused("claude", &["-p", "hi", "--permission-mode", "dontAsk"]).as_deref(),
            Some("--permission-mode dontAsk")
        );
        for args in [
            &["--resume"][..],
            &["--permission-mode", "default"],
            &["--permission-mode=plan"],
            &["--", "--dangerously-skip-permissions"],
            &["--yolo"],
        ] {
            assert_eq!(refused("claude", args), None, "{args:?}");
        }
    }

    #[test]
    fn codex_flags_that_skip_prompts_or_restore_its_shell_are_refused() {
        for args in [
            &["--dangerously-bypass-approvals-and-sandbox"][..],
            &["--yolo"],
            &["--full-auto"],
            &["-a", "never"],
            &["-anever"],
            &["--ask-for-approval=never"],
            &["-s", "danger-full-access"],
            &["--sandbox=danger-full-access"],
            &["--enable", "shell_tool"],
            &["--enable=unified_exec"],
            &["-c", "approval_policy=\"never\""],
            &["-csandbox_mode=\"danger-full-access\""],
            &["--config", "features.shell_tool=true"],
            &["-c", "features={shell_tool=true}"],
            &["-c", "mcp_servers.cave.command=\"/bin/sh\""],
            &["-c", " mcp_servers.cave = {}"],
        ] {
            assert!(refused("codex", args).is_some(), "{args:?}");
        }
        for args in [
            &["--resume"][..],
            &["-a", "on-request"],
            &["-s", "read-only"],
            &["--enable", "web_search"],
            &["-c", "model=\"o3\""],
            &["-c", "mcp_servers.other.command=\"x\""],
            &["never"],
            &["--", "--yolo"],
            &["--dangerously-skip-permissions"],
        ] {
            assert_eq!(refused("codex", args), None, "{args:?}");
        }
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
