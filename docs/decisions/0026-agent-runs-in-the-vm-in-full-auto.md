# 0026: `cave agent` runs the agent in the VM, without permission prompts, D53 to D55

Date: 2026-10-15 · Decided by: Ivan (choices), with defaults picked while building (marked) · Status: accepted. Replaces decision 0002.

## Context

Ivan's proposal "Agent Launch and Project MCP Integration" asks for coding agents to run without permission prompts, since prompts push people to turn them off outside any sandbox. Skipping prompts is only safe when the agent's tools can reach nothing but the VM. Decision 0002 runs the agent on the Mac by default, where its own tools still reach the Mac, so it cannot skip prompts there.

## Decision

- **D53. Where the agent runs.** `cave agent <name>` runs the agent inside the VM, through the guest agent, with its permission prompts skipped (Claude Code's `--dangerously-skip-permissions`). It prints `Permissions skipped. The VM is the sandbox.` before it starts (Ivan). `--safe` keeps the prompts (Ivan). `--on-mac` runs it on the Mac through `cave mcp`, as decision 0002 did, and never skips prompts there (Ivan). `--on-mac` replaces `--in-vm`; `--in-vm` is still accepted and does nothing, so existing scripts keep working (*default*).
- **D54. Agents in v1.** Claude Code and Codex, both with the same default (Ivan). Others come after v1.
- **D55. Plain `claude` in a project folder.** `cave init` denies Claude Code's tools that act on the Mac in `.claude/settings.json`: Bash, Read, Glob, Grep, Edit, Write, NotebookEdit and WebFetch, so it can act only through the `cave` tools (*default*). Next to the "Keep a copy of the code on your Mac?" question, `cave init` asks whether Claude Code may skip its permission prompts in this project, saying the VM is its sandbox (Ivan). A yes writes `permissions.defaultMode = "bypassPermissions"` to `.claude/settings.local.json`, which is personal and not shared with collaborators (*default*). The question ships only once a test shows the deny rules still hold with prompts skipped (*default*).

## Consequences

- The agent's own sign-in, or its model API key, now lives in the VM by default. It is frozen into memory snapshots and forks taken afterwards, and a prompt-injected agent can read it and send it out over the VM's network. Project credentials in `.env` are unaffected: they stay on the Mac and are injected one command at a time (D33). The docs say so.
- The agent is installed in the VM on first use with its own installer, and you sign in inside the VM once per project VM. Your Mac-side agent settings and plugins are not in the VM.
- With `[network] mode = "off"`, an agent in the VM cannot reach its model API, so `cave agent` stops and says how to fix it (*default*).
- MCP servers and plugins configured in your own Claude Code settings still run on the Mac when you run plain `claude`, whatever the project's deny rules say. The docs say so.
- Gate 5 was closed with the agent on the Mac; that path remains as `--on-mac`.
