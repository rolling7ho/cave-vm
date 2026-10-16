# Threat model

This page says who cave defends against, what it protects, where the boundaries are and what crosses them. [safety.md](safety.md) describes each guardrail in more detail and lists the known limits. When this page and [design.md](design.md) disagree, the design is right.

## What cave protects

In order of importance:

1. **The Mac:** your files outside the project, your other processes, your keychain and your network position (the LAN, services on `localhost`).
2. **Credentials in `.env`:** they stay on the Mac and are given to one command at a time ([D33](../ROADMAP.md#decisions)).
3. **Other VMs:** one project's VM cannot reach another's disk, memory or network.
4. **Your work:** snapshots, and in synced mode the Mac mirror, let you go back after a bad change.

cave does not protect what you put inside the VM, or what the VM can reach over its network ([design](design.md), "Safety model").

## Who we defend against

- **A confused or prompt-injected agent inside the VM**, with a root shell there. This is the main adversary. It may run any code in the guest, including its own kernel modules, and may try to talk to the host in any way the guest can.
- **Code the agent runs**: packages, build scripts and tests from the internet, with the same reach as the agent.
- **An agent on the Mac** (`cave agent --on-mac`, or plain `claude` in a project) that reads untrusted text and tries to act outside the VM.
- **A repository you clone** that ships its own `.claude/`, `.mcp.json` or `cave.toml`. cave echoes a cloned `cave.toml`'s policy when a VM starts. It does not check `.claude/` or `.mcp.json` files it did not write: Claude Code's own trust prompt is the guard there.

Out of scope: malware or another person already running code as your macOS user, an attacker with physical access, a compromised macOS or Hypervisor.framework, and a malicious cave release.

## Trust boundaries

```
 agent in the VM ──(1) hardware VM──▶ VMM process ──(2) control socket──▶ daemon ──(3) daemon.sock──▶ cave CLI, MCP, SDKs
        │                              │ sandbox-exec                         │
        └──(4) NAT──▶ internet          └──(5) .cave/ files                    └──(6) Mac mirror, .env, terminal
```

1. **Guest to VMM.** Hardware virtualization through Hypervisor.framework. The guest reaches the VMM only through the devices it emulates: a PL011 UART and virtio-mmio block, net, vsock and balloon devices, plus PSCI calls and the interrupt controller.
2. **VMM to daemon.** One VMM process per VM, started by the daemon under the sandbox profile in `crates/daemon/src/vmm.sb`. It can read and write only its project's `.cave/` folder, read the image cache and kernel, read the DNS configuration and make outbound connections. It talks to the daemon over a socket pair on its standard input, with no socket file ([protocol](protocol.md)).
3. **Clients to daemon.** The daemon's socket `daemon.sock` has mode `0600` in a data directory with mode `0700`, so only your user can reach it.
4. **Guest to network.** A userspace NAT in the VMM. Outbound only; the Mac's own addresses, `localhost`, reserved and multicast ranges are denied in every mode, and private, link-local and shared (`100.64.0.0/10`) ranges unless `--net lan` ([safety](safety.md#what-the-vm-protects)). No Mac service listens for the guest, and the NAT gateway only forwards DNS; ports reach the Mac only with `-p`, and only on its loopback.
5. **VMM to its files.** The disk image and snapshots in `.cave/`.
6. **Daemon and CLI to the Mac.** The daemon writes the mirror; the CLI relays the guest's terminal output and file contents. In the other direction, `cave push` copies the Mac files you changed into the VM, never `.env` at the top level, `.git` or the paths `.gitignore` names.

## What the guest can send to the Mac

The guest has no service on the Mac to call. Everything it can send crosses one of these, and each is bounded:

| Path | What the host does with it | Limit |
| --- | --- | --- |
| Agent protocol over vsock | The daemon parses length-prefixed JSON from the guest agent | Frames over 1 MiB are refused; guest vsock connections are capped at 1024 |
| Outbound network through the NAT | The VMM opens the connection on the guest's behalf | Policy above; 1024 TCP connections and 1024 UDP flows |
| DNS to the gateway | The VMM sends the query on to the Mac's DNS servers from `/etc/resolv.conf` | UDP port 53 only; those servers are reached even when they are on the LAN or the Mac |
| Mirror sync | The daemon writes files under the project folder | Paths that are absolute or contain `.` or `..` segments are refused, and nothing is written through a symbolic link in the project folder. `.git`, `.cave`, the top-level `.env`, `cave.toml`, `.mcp.json`, `.gitignore` and `.claude/` are never written, compared without case ([D59](../ROADMAP.md#decisions)). Credential values are not redacted from the mirror |
| Folders read out (`cave cp` from the VM, `cave pull`) | A tar stream made in the VM is unpacked by the Mac's `/usr/bin/tar` | That tar refuses absolute paths, `..` and writing through symbolic links; links themselves are recreated and may point anywhere on the Mac |
| Terminal output (`cave shell`, `cave exec`, `cave agent`) | Relayed to your terminal | Escape sequences pass through, except cave's own link marker |
| Sign-in links during `cave agent` | Opened in the Mac's browser | Only `https://` on `claude.ai`, `claude.com`, `anthropic.com` or their subdomains, with no user name or port ([D56](../ROADMAP.md#decisions)) |
| Sign-in callback during `cave agent` | A loopback port on the Mac passes one request into the VM | Port 1024 or more, at most 10 minutes |

## Credentials

A credential is read from `.env` only when a command names it, injected into that command's environment, and redacted as `[NAME]` from that and every later output and file read while the VM may still hold it. The audit log records names, never values. The command itself sees the value, so a command can leak it ([safety](safety.md#credentials)).

## What happens if a boundary falls

- **Guest escapes into the VMM** (a bug in a device model or in Hypervisor.framework): the attacker runs with the VMM's sandbox, which still allows the project's `.cave/` folder and outbound connections. The NAT policy runs in that same process, so from there the LAN and the Mac's own addresses are no longer denied. Other folders and the keychain stay out of reach.
- **VMM sandbox escape:** the attacker runs as your user. This is why the VMM has no unsafe Rust outside `hvf` and the guest-memory and host modules, and why device models are tested against the mock backend.
- **Agent on the Mac ignores the skill:** the deny rules in `.claude/settings.json` and, after a yes in `cave init`, `.claude/settings.local.json` still hold for Claude Code; other agents keep their shell ([safety](safety.md#the-agent-boundary)).

## Reviewing changes

Every change that touches one of the boundaries above, or what crosses it, is a security change: its PR says which boundary under "Safety" and updates this page or [safety.md](safety.md) when the answer changes.
