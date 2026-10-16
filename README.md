# cave-vm

**A fast, disposable Linux VM for every project, built for coding agents on macOS.**

cave-vm (Coding Agent Virtual Environment) is an open-source microVM runtime for Macs with Apple Silicon (M-series) chips. It gives each project directory its own Linux VM that starts quickly, can be snapshotted, rolled back and forked, and shows you exactly what it can reach.

Coding agents are most useful when you stop approving every command, and most risky at the same moment. With cave-vm the agent works inside a hardware-isolated VM instead of on your Mac, so a bad command damages a disposable machine, not your files.

## Requirements

- macOS 26 (Tahoe) or later
- Apple Silicon Mac
- Guests are ARM64 Linux (Debian by default, or optionally Ubuntu)

## Quickstart

```sh
brew install rolling7ho/cave/cave-vm
cd [my-project] && cave agent [claude/codex]
```

Without Homebrew, `curl -fsSL https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh | sh` installs the release binaries in `~/.local/bin` (set `CAVE_PREFIX` to change that) after checking their checksum.

The first run downloads the kernel and a Debian image, signs the binary for Hypervisor.framework, and checks your setup. If something is wrong, the error says how to fix it, and `cave doctor` re-runs the checks.

Commands:

| Command | What it does |
| --- | --- |
| `cave init` | Initialize a project with `cave.toml` and `.cave/`, choosing sealed or synced mode |
| `cave run` | Boot or restore the project's VM, with optional networking, port forwarding, or fork selection |
| `cave shell` | Open an interactive shell inside the VM |
| `cave exec -- <cmd>` | Execute a command inside the VM |
| `cave stop` | Suspend the VM, preserving its state |
| `cave snapshot` | Save the VM's current state as a snapshot |
| `cave restore` | Roll back the VM to a snapshot, using the newest by default |
| `cave fork` | Create an independent VM from a snapshot using copy-on-write |
| `cave rm` | Remove the project's VM, a fork, or a snapshot |
| `cave agent <name>` | Launch a coding agent against the project's VM, with optional safety and host execution settings |
| `cave cp` | Copy files between the Mac and VM, using `:` to identify VM paths |
| `cave pull` | Export the project from the VM or a snapshot to a folder on the Mac |
| `cave sync` | Update the Mac mirror from the VM, optionally watching for changes |
| `cave push` | Copy Mac-side changes into the VM after taking a snapshot |
| `cave inspect` | Display the VM's policy, resources, and audit log |
| `cave view` | Watch VM activity live, including commands, file writes, and connections |
| `cave ls` | List project VMs, optionally including all entries or JSON output |
| `cave images` | List available base VM images |
| `cave gc` | Inspect disk usage and safely reclaim unused storage |
| `cave doctor` | Check Mac compatibility, cave's signature, and the image cache |
| `cave help` | Display general help or detailed help for a specific command |
| `cave --version` | Display the installed cave version |

The full command reference, with every flag and error, is in [docs/cli.md](docs/cli.md), and `cave.toml` is described in [docs/config.md](docs/config.md). SDKs for Rust ([crates/sdk](crates/sdk)), Python ([sdks/python](sdks/python)) and TypeScript ([sdks/typescript](sdks/typescript)) expose the same operations, and `cave mcp` serves them to agents as MCP tools.

## The idea

- **The project directory is the VM.** `cd` into a project and run `cave shell`. Delete the folder and everything is gone.
- **Code lives inside the VM.** The agent can't touch host files because nothing from the host is mounted. If you want your normal editor, an optional synced copy of the source stays on your Mac.
- **Snapshot, roll back, fork.** Snapshot before a risky run and restore after. Fork a project into parallel copies to try several approaches at once, using copy-on-write so forks are cheap.
- **Safety you can see.** Every VM has an inspectable policy: what is reachable, how much CPU and memory it gets, and a log of what the agent did.
- **Agent-first.** An MCP server and a ready-made skill let Claude Code and Codex use the VM as their sandbox with no host shell.
- **Easy to set up.** One install, and the first run fetches everything else and fixes what it can. No SSH keys, no cloud-init, no image formats to learn.

## Safety defaults

- **Files:** nothing from your Mac is reachable from the VM.
- **Network:** outbound NAT only. Private and local-network ranges are blocked unless you opt in with `--net lan`, and `--net off` turns the network off entirely.
- **Credentials:** injected into the single command that needs them, never returned to the agent and never written to the audit log.
- **Widening access** always takes an explicit flag, and the flag is echoed back when the VM starts.

A VM protects your Mac from a misbehaving or prompt-injected agent. It does not protect secrets you place inside it, or anything the agent can reach over the network, and an agent that still has a host shell can step outside the VM. [docs/safety.md](docs/safety.md) describes the model and its limits, and [SECURITY.md](SECURITY.md) says what counts as a vulnerability.

## How it works

cave-vm is a VMM written in Rust on top of Apple's Hypervisor.framework, in the spirit of [Firecracker](https://github.com/firecracker-microvm/firecracker): a minimal virtio device model, direct kernel boot with its own small kernel, and copy-on-write snapshots. One VMM process runs per VM, supervised by a daemon. The CLI, SDKs and MCP server all talk to that daemon, and a small guest agent inside the VM handles commands, shells, files and port forwarding over vsock.

The full design is in [docs/design.md](docs/design.md).

## Contributing

Contributions are welcome once the project opens up. Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Contributors are expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report security problems privately, as described in [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
