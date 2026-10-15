# cave-vm

**A fast, disposable Linux VM for every project, built for coding agents on macOS.**

> **Status: v0.1.0, the first release.** It works on Apple Silicon Macs with macOS 26 or later. v1.0 follows the release review.

cave-vm (Coding Agent Virtual Environment, command: `cave`) is an open-source microVM runtime for Macs with Apple Silicon (M-series) chips. It gives each project directory its own Linux VM that starts quickly, can be snapshotted, rolled back and forked, and shows you exactly what it can reach.

Coding agents are most useful when you stop approving every command, and most risky at the same moment. With cave-vm the agent works inside a hardware-isolated VM instead of on your Mac, so a bad command damages a disposable machine, not your files.

## The idea

- **The project directory is the VM.** `cd` into a project and run `cave shell`. No names, no registry, no IDs. Delete the folder and everything is gone.
- **Code lives inside the VM.** The agent can't touch host files because nothing from the host is mounted. If you want your normal editor, an optional synced copy of the source stays on your Mac.
- **Snapshot, roll back, fork.** Snapshot before a risky run and restore after. Fork a project into parallel copies to try several approaches at once, using copy-on-write so forks are cheap. The target is a fork in well under a second.
- **Safety you can see.** Every VM has an inspectable policy: what is reachable, how much CPU and memory it gets, and a log of what the agent did.
- **Agent-first.** An MCP server and a ready-made skill let Claude Code and other agents use the VM as their sandbox with no host shell.
- **Easy to set up.** One install, and the first run fetches everything else and fixes what it can. No SSH keys, no cloud-init, no image formats to learn.

## Quickstart

```sh
brew install rolling7ho/cave/cave-vm
cd my-project && cave agent claude
```

The first run downloads the kernel and a Debian image, signs the binary for Hypervisor.framework, and checks your setup. If something is wrong, the error says how to fix it, and `cave doctor` re-runs the checks.

Commands:

| Command | What it does |
| --- | --- |
| `cave init` | Set up a project: `cave.toml`, the `.cave/` folder, and the agent skill files |
| `cave run` / `cave shell` | Boot or restore this project's VM, and open a shell in it |
| `cave exec -- <cmd>` | Run a command in the VM |
| `cave snapshot` / `cave restore` | Save VM state and roll back to it |
| `cave fork` | Create a new VM from a snapshot, copy-on-write |
| `cave inspect` | Show the VM's policy, resources and audit log |
| `cave sync` / `cave pull` | Keep a Mac copy of the code in step with the VM, and bring Mac edits back (synced mode) |
| `cave agent claude` | Start Claude Code against this project's VM, with host Bash denied |
| `cave doctor` | Check entitlements, macOS version, disk space and image state |

The full command reference, with every flag and error, is in [docs/cli.md](docs/cli.md), and `cave.toml` is described in [docs/config.md](docs/config.md). SDKs for Rust ([crates/sdk](crates/sdk)), Python ([sdks/python](sdks/python)) and TypeScript ([sdks/typescript](sdks/typescript)) expose the same operations, and `cave mcp` serves them to agents as MCP tools.

## Safety defaults

- **Files:** nothing from your Mac is reachable from the VM.
- **Network:** outbound NAT only. Private and local-network ranges are blocked unless you opt in with `--net lan`, and `--net off` turns the network off entirely.
- **Credentials:** injected into the single command that needs them, never returned to the agent and never written to the audit log.
- **Widening access** always takes an explicit flag, and the flag is echoed back when the VM starts.

A VM protects your Mac from a misbehaving or prompt-injected agent. It does not protect secrets you place inside it, or anything the agent can reach over the network, and an agent that still has a host shell can step outside the VM. [docs/safety.md](docs/safety.md) describes the model and its limits, and [SECURITY.md](SECURITY.md) says what counts as a vulnerability.

## How it works

cave-vm is a VMM written in Rust on top of Apple's Hypervisor.framework, in the spirit of [Firecracker](https://github.com/firecracker-microvm/firecracker): a minimal virtio device model, direct kernel boot with its own small kernel, and copy-on-write snapshots. One VMM process runs per VM, supervised by a daemon. The CLI, SDKs and MCP server all talk to that daemon, and a small guest agent inside the VM handles commands, shells, files and port forwarding over vsock.

The full design is in [docs/design.md](docs/design.md).

## Requirements

- macOS 26 (Tahoe) or later
- Apple Silicon Mac
- Guests are ARM64 Linux (Debian by default, or optionally Ubuntu)

## Roadmap

Phases 1 to 5 are done, each closed by a gate measured on a real Mac ([docs/bench/gate4.md](docs/bench/gate4.md) has the snapshot and fork numbers). Phase 6 is under way; [ROADMAP.md](ROADMAP.md) has the details.

1. **Spec:** README quickstart and CLI surface written before any code.
2. **Spike:** boot a Linux kernel to a shell through Hypervisor.framework, and confirm the signing and snapshot assumptions.
3. **Core VM:** block, network and vsock devices, the guest agent, and the basic CLI.
4. **Snapshot and fork:** the headline feature.
5. **Agent layer:** `cave init`, the agent skill, the MCP server, the safety policy and audit log.
6. **Density and polish:** memory ballooning, `cave doctor`, first-run setup, the SDKs, the Ubuntu image, and the Homebrew formula.

## Contributing

Contributions are welcome once the project opens up. Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Contributors are expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md). AI-assisted contributions are allowed with disclosure in the pull request. Report security problems privately, as described in [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
