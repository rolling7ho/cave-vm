# 0003: Roadmap open items D4 to D12

Date: 2026-10-11 · Decided by: Ivan · Status: accepted

## Context

`ROADMAP.md` listed nine items the design left open, each with a recommendation. Ivan approved all nine recommendations as written.

## Decision

- **D4, dependencies.** New dependencies are requested once per phase, each with a reason: phase 2 `vm-fdt` and `serde`/`serde_json`; phase 3 `vm-memory`, `virtio-queue`, `smoltcp`, `clap` and `sha2`; phase 5 an MCP server library or a small hand-written server. Small pieces are written in-house rather than pulling in large frameworks. Each request is still made and answered when the phase starts.
- **D5, guest memory.** The guest-memory code is a single `memory` module inside `vmm`, the only module in that crate allowed to use `unsafe`. No new crate.
- **D6, Linux builds.** Release builds of the kernel, base image and musl guest agent run in CI on Linux runners, with release automation set up by Ivan. Local builds move inside a cave VM once gate 3 passes; before that, a documented cross toolchain builds the agent and a container builds the kernel.
- **D7, toolchain.** `rust-toolchain.toml` pins the current stable release when the workspace lands, and bumps happen in their own PRs.
- **D8, interrupt-controller contingency.** If gate 2 shows interrupt-controller state cannot be saved and restored through Hypervisor.framework, a decision record at gate 2 compares a userspace GIC emulated in the VMM against snapshots taken only where interrupt state can be reconstructed.
- **D9, printed credentials.** If a command prints an injected credential, the exact value is redacted, best effort, from streamed output and MCP responses. The docs state that a command can still leak it in other ways, such as files or the network.
- **D10, config key names.** Key names in `cave.toml` for idle auto-suspend, automatic snapshots and deletion, TTL and the update detector are proposed and approved in the config reference (ROADMAP task P1.2).
- **D11, hardware CI.** A self-hosted Mac runner runs end-to-end tests on PRs from the repo itself while it is private. This is revisited before the repo goes public, since self-hosted runners on public repos run untrusted code. Ivan registers the runner and sets branch protection.
- **D12, README fork wording.** The README's `cave fork` row is aligned with the design ("create a new VM from a snapshot, copy-on-write") in ROADMAP task P1.7.

## Consequences

- `docs/design.md` gains the D9 redaction rule in its credentials row.
- `AGENTS.md` names `vmm::memory` as the guest-memory code where `unsafe` is allowed.
- `ROADMAP.md` has no open decisions; new ones go under "Needs Ivan's decision".
