# 0007: `unsafe` for host calls, and PRs against main (D24, D25)

Date: 2026-10-13 · Decided by: Ivan · Status: accepted

## Context

**D24.** Phase 3 needs system calls that std does not wrap. The guest agent needs vsock sockets, ptys with resize, and `setsid` for the shell. The VMM needs `clonefile` for project disks (P3.4) and hole punching for discard (P3.3). The design allowed `unsafe` only in `hvf` and the guest-memory code. Safe wrappers such as `nix` or `rustix` are larger and would still leave pty resize and the controlling terminal needing `unsafe`.

**D25.** Stacked PRs were closed automatically when the branch below them merged and was deleted, and one stacked PR was merged into a feature branch instead of main (#19 to #21, re-landed as #22).

## Decision

- **D24.**
  - Add `libc`.
  - `unsafe` is allowed in two more places: a `sys` module in `agent`, and a `host` module in `vmm`.
  - Each `unsafe` block keeps its `// SAFETY:` comment.
  - Each module wraps its calls in safe functions, so the rest of the crate stays `unsafe`-free.
- **D25.**
  - Every PR targets main. PRs are not stacked.
  - Work that depends on an open PR waits on its own branch, and its PR opens once that PR merges.
  - Independent PRs can be open at the same time.

## Consequences

- AGENTS.md, CONTRIBUTING.md and the design's guardrails list the four places `unsafe` may appear.
- Discard (P3.3), project disks (P3.4), vsock (P3.5), the agent (P3.6) and the sandbox profile (P3.13) can proceed.
