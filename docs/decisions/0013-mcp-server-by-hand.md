# 0013: The MCP server is written by hand (D31)

Date: 2026-10-14 · Decided by: Ivan · Status: accepted

## Context

D4 left the phase 5 choice open: an MCP server library or a small hand-written server, asked when the phase starts. The official Rust SDK (`rmcp`) covers more of the specification but brings in tokio and a large dependency tree. `cave mcp` needs little of it: the stdio transport, `initialize`, `ping`, `tools/list` and `tools/call` for seven tools, one request at a time.

## Decision

The `mcp` crate implements the server itself: newline-delimited JSON-RPC 2.0 on stdin and stdout, on `serde_json`, which the workspace already uses, and the Rust SDK for the daemon. No MCP library is added.

## Consequences

- No new dependencies for phase 5's MCP server.
- Protocol features beyond tools (resources, prompts, sampling, progress, cancellation) are not implemented until a task needs them. Requests are handled one at a time, so a long `exec` holds up later requests until it ends.
- Supported protocol versions are listed in the crate and updated by hand when the specification moves.

## Addendum: destroy removes forks only (D32)

Date: 2026-10-14 · Decided by: Ivan

The `destroy` tool removes forks only. It requires the `fork` argument and never removes the project's own VM, its disk or its snapshots, so an agent cannot delete the work in `.cave/`. Removing the project's VM stays a deliberate step on the Mac, `cave rm`, which asks for confirmation (design).
