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

- **Guidance:** the skill that `cave init` writes into `.cave/AGENTS.md`, with a line in `AGENTS.md` pointing there, and into `.claude/skills/cave/` tells the agent to work only inside the VM. A skill is guidance, not enforcement. An agent can ignore it.
- **The VM itself:** `cave agent claude` and `cave agent codex` run the agent inside the VM, so its own tools, shell included, reach only the VM. That is why they skip its permission prompts there and print `Permissions skipped. The VM is the sandbox.`; `--safe` keeps the prompts ([D53](../ROADMAP.md#decisions)).
- **Guardrail on the Mac:** the MCP server, `cave mcp`, gives the agent `exec`, `read_file`, `write_file`, `snapshot`, `restore`, `fork` and `destroy`, all inside the VM. `cave agent claude --on-mac` starts Claude Code on the Mac with that server and with Bash denied, and `cave init` writes the same denial into `.claude/settings.json`. Claude Code is also denied reading `.env`, so it cannot see the credentials there. `cave agent codex --on-mac` starts Codex with the same server and its shell tools turned off; Codex then has no tool that reads a file on the Mac, so it cannot read `.env` either ([D44](../ROADMAP.md#decisions)). Codex's calls to the `cave` server are approved in advance, since they act only on the VM.

The limit, stated plainly: **a confused or prompt-injected agent that still has a host shell can step outside the VM.** The VM contains only what runs inside it. In particular:

- `cave agent --on-mac` never skips the agent's prompts, and refuses the agent's own flags that would.
- An agent you start yourself, without `cave agent` and without the settings `cave init` writes, keeps its host shell. The skill alone does not stop it.
- Agents other than Claude Code and Codex get the skill and the MCP server, but cave does not deny their shell. It denies only where the agent supports it. `cave init` writes the denial for Claude Code only; Codex has it only when started with `cave agent codex --on-mac`.
- Codex on the Mac keeps its other tools, including `apply_patch`, which edits files on the Mac, web search and any MCP servers you configured for it. Like Claude Code's edits, these do not reach the VM.
- Claude Code on the Mac has no Bash, Edit, Write or NotebookEdit, but keeps its Read, Glob, Grep and WebFetch tools and any other MCP servers you configured. It asks before reading outside the project folder, and the project folder holds the Mac files and, in synced mode, the mirror.
- Approving a tool when Claude Code asks removes the guardrail for that tool.
- If you answered yes to `cave init`'s question about skipping prompts, plain `claude` in the project runs without them in Claude Code's `dontAsk` mode: the `cave` tools are allowed, `.claude/settings.local.json` also denies Read, Glob, Grep and WebFetch, and any other tool is turned away without asking, so its own tools reach nothing on the Mac ([D55](../ROADMAP.md#decisions)). MCP servers and plugins from your own Claude Code settings run on the Mac only where your own settings allow them.

Inside the VM, by default, the agent has a full shell, but only in the VM. Its model API key and sign-in live in the VM too, so they are frozen into memory snapshots and forks taken afterwards, and a confused or prompt-injected agent can read them and send them out over the VM's network. The VM protects the Mac, not what you put in the VM. While `cave agent` runs, the VM can ask `cave` to open a link in the Mac's browser, which is how the agent signs in; only `https://` links on `claude.ai`, `claude.com` or `anthropic.com` are opened, and `cave` says afterwards how many it opened ([D56](../ROADMAP.md#decisions)). When such a link returns to `localhost`, `cave` listens on that port on the Mac's loopback until the browser comes back, for at most 10 minutes, and passes that one request into the VM. Use `--on-mac` to keep the key on the Mac. Credentials in `.env` are not affected: they stay on the Mac either way.

## Credentials

A credential named in an `exec` call is read from `.env` on the Mac and injected into that one command's environment. Its value is replaced with `[NAME]` in the output returned to the agent, then and in every later command and file read while the VM's memory may still hold it, and the audit log records the name only. `.env` is never copied into the VM or synced ([D33](../ROADMAP.md#decisions)).

What this does not cover:

- The command itself sees the value. A command the agent writes can save it to a file, print it encoded so it no longer matches, or send it over the network. Inject a credential only into commands you would trust with it.
- A snapshot taken after an injection may hold the value in the VM's memory. `cave fork`, `cave restore` and `cave export` warn when they use one.

## The Mac mirror backs up source only

In synced mode the project folder on your Mac is a mirror of the code in the VM. It is a convenience for your editor and a backup of source files, not of the VM:

- It lacks `node_modules`, `target`, everything `.gitignore` names, `.git`, and the VM's running processes.
- It is updated only when the daemon syncs: after `cave snapshot`, at the end of `cave agent`, and on `cave sync`, or every 2 seconds under `cave sync --watch`. Forks have no mirror. Work since the last sync is only in the VM.

VM snapshots are the real safety net. Take one before a risky change, and use `cave restore` to go back. To keep work outside the VM, use `git push` from inside it, `cave export`, or `cave cp`.

## Host isolation

macOS has no jailer like Firecracker's. Each VM's VMM process runs under a macOS sandbox profile that lets it read and write only its project's `.cave/` folder, read the shared image cache and the kernel, and make outbound connections. It has no access to your other folders. The daemon, not the VMM, writes the mirror. A guest also cannot exhaust the Mac by opening connections without end: the VMM keeps at most 1024 outbound TCP connections, 1024 UDP flows and 1024 guest connections to the host over vsock, and resets or drops any past that.

## The audit log

Each VM records exec calls, file writes and network connections, allowed or denied, in `.cave/`: the newest in `audit-latest.log`, older ones in files named for the date and time they were moved aside, once the latest passes 10 MB. `cave inspect` shows the last 20 entries (`--log` for all of them), and `cave view` follows them live. The log is a record of what happened, not a block, and it is removed with the VM.

## Known limits

These are known and accepted for v1:

- **The Mac's addresses are read once.** The VM is denied every address the Mac has when the VM starts. If the Mac joins another network or gets a new address while the VM runs, the VM can reach that new address until it is stopped and started again.
- **Turning off update checks is per project.** `[updates] check = false` in a project's `cave.toml` stops cave's own lookups only for commands run in that project. Commands run anywhere else still look for a newer cave.
- **That setting is read by a small parser.** To stay fast before every command, cave reads `check` with a simple line reader, not a full TOML parser. It understands `check = false` on its own line under `[updates]`. Other ways of writing it, such as an inline table, are not seen, and checks stay on.
- **Downloads use the Mac's curl.** Update lookups and installs run `/usr/bin/curl`, which reads your `~/.curlrc`. Settings there, such as a proxy, apply to these requests as well.

## Reporting a problem

If cave lets something past a guardrail described here, report it privately as [SECURITY.md](../SECURITY.md) describes.
