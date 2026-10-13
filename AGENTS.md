# AGENTS.md

cave-vm is an open-source (Apache 2.0) microVM runtime for macOS, built for coding agents. The command is `cave`. Each project directory gets its own disposable Linux VM that starts fast, can be snapshotted and forked, and shows what it can reach. Hardware virtualization comes from Apple's Hypervisor.framework. Guests are ARM64 Linux. Host: macOS 26 or later, Apple Silicon only.

## Source of truth

`docs/design.md` is the design. Anything marked decided there is not yours to change.

- If the design is missing something or contradicts itself, stop and ask. Do not guess.
- A real design change gets a short decision record in `docs/decisions/` before the code changes.
- Things the design leaves to the spike (signing, vmnet, interrupt-controller state, performance numbers) must be measured, not assumed.

`ROADMAP.md` is the implementation plan: phases, gates and tasks sized as small PRs. Pick work from it, put the task ID in your PR, and do not build on anything in its "Needs Ivan's decision" list until it is answered. If it disagrees with the design, the design wins.

## Commands

- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo fmt --check && cargo clippy --workspace -- -D warnings`
- Supply chain: `cargo deny check && cargo audit`

## Crates

- `proto`: message types shared by daemon, agent and CLI
- `hvf`: the only wrapper around Hypervisor.framework
- `vmm`: per-VM process, device models, snapshots
- `daemon`: supervises one VMM process per VM, policy, audit log, sync
- `agent`: guest agent, built for Linux (musl), reached over vsock
- `cli`: builds the `cave` binary
- `sdk`, `mcp`: Rust SDK and MCP server over the daemon's socket API
- `sdks/python`, `sdks/typescript`: thin clients, no native extensions
- `image`: kernel config and Debian and Ubuntu image builds

## Rules

- `unsafe` is allowed only in `hvf`, the guest-memory code (the `memory` module in `vmm`), the `host` module in `vmm` and the `sys` module in `agent` (D24). Every `unsafe` block has a `// SAFETY:` comment.
- No new dependencies without asking, and say why when you ask.
- No network access in tests. Test device models against the mock backend.
- Do not touch signing, release or CI secrets.
- Credentials are injected into the command being run and never returned to the agent. Audit logs record names, not values.
- Keep the user-facing surface small: names instead of IDs, sensible defaults, errors that say how to fix the problem.

## Workflow

1. Read the relevant section of `docs/design.md`.
2. Write a short plan. For anything large, open a design note in an issue first.
3. Write tests first where practical.
4. Implement one crate at a time.
5. Open a small PR against main. Do not stack PRs: work that depends on an open PR waits on its branch until that PR merges (D25). Commits use Conventional Commits (`feat:`, `fix:`) and DCO sign-off (`git commit -s`).
6. Request a review from @rolling7ho on every PR (`gh pr edit <n> --add-reviewer rolling7ho`).

## Definition of done

- `cargo fmt`, `cargo clippy -D warnings` and the tests pass.
- Docs and the changelog are updated.
- Any new `unsafe` has its safety comment.
- PRs that include AI-assisted work say so.
