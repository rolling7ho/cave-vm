# Safety model and its limits

This page says what cave protects, what it does not, and where its guardrails stop. It follows the design's "Safety model" and "Agent integration" sections. When this page and `docs/design.md` disagree, the design is right.

## What the VM protects

The VM protects your Mac from a misbehaving or prompt-injected agent. A bad command damages a disposable Linux machine, not your files.

It does not protect:

- Secrets you place inside the VM. Anything in `/workspace`, in the VM's environment or on its disk can be read by the agent.
- Data the agent can reach over the network. The VM has outbound internet by default, so an agent can send what it can read to any site.

Every VM's policy is printed by `cave run` and shown at any time by `cave inspect`:

| Policy item | Default | How it widens |
| --- | --- | --- |
| Host files reachable | none (sealed) | `synced` adds a one-way mirror written by the daemon, never the agent |
| Network | NAT, outbound only, LAN ranges and the Mac denied | `--net lan` allows private ranges; `--net off` removes the network |
| CPU and memory | capped in `cave.toml` | edit the config |
| Lifetime | no TTL | `ttl` in `cave.toml` |
| Ports exposed to the Mac | none | `-p 3000` |

Widening always takes an explicit flag or config change, and `cave run` echoes it back.

## The agent boundary

cave ships guidance and a guardrail.

- **Guidance:** the skill that `cave init` writes into `AGENTS.md` and `.claude/skills/cave/` tells the agent to work only inside the VM. A skill is guidance, not enforcement. An agent can ignore it.
- **Guardrail:** the MCP server, `cave mcp`, gives the agent `exec`, `read_file`, `write_file`, `snapshot`, `restore`, `fork` and `destroy`, all inside the VM. `cave agent claude` starts Claude Code with that server and with Bash denied, and `cave init` writes the same denial into `.claude/settings.json`. Claude Code is also denied reading `.env`, so it cannot see the credentials there.

The limit, stated plainly: **a confused or prompt-injected agent that still has a host shell can step outside the VM.** The VM contains only what runs inside it. In particular:

- An agent you start yourself, without `cave agent` and without the settings `cave init` writes, keeps its host shell. The skill alone does not stop it.
- Agents other than Claude Code get the skill and the MCP server, but cave does not deny their shell. It denies only where the agent supports it, and today that is Claude Code.
- Claude Code on the Mac keeps its own tools other than Bash. It can read and edit files on the Mac with its Read, Edit and Write tools. It asks first outside the project folder, and the project folder holds the Mac files and, in synced mode, the mirror. It can also fetch web pages and use any other MCP servers you configured. Edits it makes to the Mac files do not reach the VM: in sealed mode those files are no longer used, and in synced mode they wait for you to run `cave pull`.
- Approving a tool when Claude Code asks, or running it with permissions skipped, removes the guardrail for that tool.

`cave agent claude --in-vm` runs the agent inside the VM instead. Then it has a full shell, but only in the VM, and the model API key and the agent's sign-in live in the VM.

## Credentials

A credential named in an `exec` call is read from `.env` on the Mac and injected into that one command's environment. Its value is replaced with `[NAME]` in the output returned to the agent, and the audit log records the name only. `.env` is never copied into the VM or synced ([D33](../ROADMAP.md#decisions)).

What this does not cover:

- The command itself sees the value. A command the agent writes can save it to a file, print it encoded so it no longer matches, or send it over the network. Inject a credential only into commands you would trust with it.
- A snapshot taken after an injection may hold the value in the VM's memory. `cave fork`, `cave restore` and `cave export` warn when they use one.

## The Mac mirror backs up source only

In synced mode the project folder on your Mac is a mirror of the code in the VM. It is a convenience for your editor and a backup of source files, not of the VM:

- It lacks `node_modules`, `target`, everything `.gitignore` names, `.git`, and the VM's running processes.
- It is updated only when the daemon syncs: after `cave snapshot`, at the end of `cave agent`, and on `cave sync`. Work since the last sync is only in the VM.

VM snapshots are the real safety net. Take one before a risky change, and use `cave restore` to go back. To keep work outside the VM, use `git push` from inside it, `cave export`, or `cave cp`.

## Host isolation

macOS has no jailer like Firecracker's. Each VM's VMM process runs under a macOS sandbox profile that lets it read and write only its project's `.cave/` folder, read the shared image cache and the kernel, and make outbound connections. It has no access to your other folders. The daemon, not the VMM, writes the mirror.

## The audit log

Each VM records exec calls, file writes and network connections, allowed or denied, in `audit.log` in `.cave/`. `cave inspect` shows the latest entries. The log is a record of what happened, not a block, and it is removed with the VM.

## Reporting a problem

If cave lets something past a guardrail described here, report it privately as [SECURITY.md](../SECURITY.md) describes.
