# 0017: CLI changes from Ivan's feedback on v0.1.0, D35 to D44

Date: 2026-10-15 · Decided by: Ivan (choices), with defaults picked while building (marked) · Status: accepted

## Context

After the v0.1.0 release Ivan sent a list of 17 changes to the `cave` command. Most are polish within the design. The ones below change something the design, `docs/cli.md` or an earlier decision had settled, or add a command. Where a choice was open, it was put to Ivan with a recommendation in the project, and he answered on 2026-10-15. Details he did not name, such as sizes, timeouts and prompts, are marked *default*: they were picked while building and can be changed here.

## Decision

- **D35, no `--`.** `cave exec <cmd> [args...]` runs the command without a `--` first; everything after the command belongs to it, so cave's own flags come before it. `--` still works. `cave agent <name> [args...]` passes its arguments the same way. This replaces the rule in `docs/cli.md` that the `--` is required.
- **D36, audit log files.** The newest entries are in `.cave/audit-latest.log` (in a fork's folder for a fork), and older ones move to `audit-<YYYY-MM-DD>-<HHMMSS>.log` in the same folder (Ivan). The move happens when `audit-latest.log` passes 10 MB (*default*). `cave inspect --log` reads them all, oldest first. A VM's existing `audit.log` becomes its `audit-latest.log`. They stay in `.cave/`, so they are gitignored and removed with the VM, as the design says.
- **D37, the agent text in `.cave/AGENTS.md`.** `cave init` writes the skill's text to `.cave/AGENTS.md`, and puts one line telling agents to read it, between the cave markers, in the project's `AGENTS.md` (Ivan). An existing `AGENTS.md` keeps everything else; an older cave section in it is replaced by the line. When there is no `AGENTS.md`, one is created with just that line, since agents only read the root file (*default*).
- **D38, `cave export [folder] [--from <snapshot>]`.** The folder is the positional argument and the snapshot moves to `--from`, as in `cave fork`. A named folder that does not exist is created after a question (Ivan); without a terminal it is created and the output says so (*default*). This replaces D19's `cave export [snapshot] [-o <dir>]`.
- **D39, `cave view` and `cave mirror`.** Two new commands (Ivan). `cave view [--fork <name>]` follows the audit log live, printing each command, file write and connection as it happens, until Ctrl-C. `cave mirror [--fork <name>]` shows the terminal of the agent that `cave agent` is running for that VM, live and read-only, until Ctrl-C. For this `cave agent` records the agent's terminal output to `agent.term` in the VM's folder, starting afresh each run (*default*). `cave mirror` and the recording were removed in 0.1.2 by D46 (decision 0019).
- **D40, more than one VM per project.** A project's other VMs are its forks, picked by name with `--fork <name>`; there are no numbered VMs (Ivan). `cave run` on a VM that is already running asks whether to start another VM as a new fork of it, or cancel (Ivan). It asks only in a terminal and with no flags; otherwise it behaves as before (*default*).
- **D41, `cave sync --watch`.** A live mode, one-way from the VM to the Mac, with the same rules as `cave sync` (Ivan). It syncs every 2 seconds until Ctrl-C and prints what each pass writes (*default*). A file changed on the Mac still waits for `cave push`. Live two-way sync stays a v1 non-goal.
- **D42, `cave push`.** `cave pull` is renamed `cave push`, since it sends Mac edits to the VM. `cave pull` keeps working, hidden from help, until the next minor release, and says it is now `cave push` (Ivan).
- **D43, `cave stop --force`.** An emergency stop of every running VM on the Mac, after a confirmation, saving nothing: SIGTERM, then SIGKILL (Ivan). The question is `[y/N]` and is refused without a terminal, and SIGKILL follows 5 seconds after SIGTERM (*default*). The next start boots each VM from its disk.
- **D44, Codex.** `cave agent codex` (Ivan) starts Codex on the Mac with `cave mcp` as its `cave` MCP server and its shell tools turned off (`--disable shell_tool --disable unified_exec`), the Codex counterpart of denying Bash to Claude Code (*default*). `--in-vm` installs it with npm inside the VM.

## Consequences

- `docs/cli.md` gains `cave view`, `cave mirror` and `cave push`, and changes `cave exec`, `cave agent`, `cave export`, `cave run`, `cave stop`, `cave sync`, `cave inspect` and `cave init` as above, each in the PR that builds it.
- The design's CLI table and its storage rule that names `cave pull` read `cave push` once D42 lands.
- `cave shell` keeps its name: it is a terminal through the guest agent, not SSH (design principle 5).
