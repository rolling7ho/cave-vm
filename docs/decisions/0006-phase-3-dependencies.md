# 0006: Phase 3 dependencies (D23)

Date: 2026-10-12 · Decided by: Ivan · Status: accepted

## Context

D4 says dependencies are requested per phase, each with a reason. At the start of phase 3 the following were proposed in the project thread, and Ivan approved all of them as recommended.

## Decision

- **Added when needed:**
  - `serde` and `serde_json` for the `proto` codec (P3.1).
  - `clap` with derive for the `cave` command line (P3.11), whose surface in `docs/cli.md` is too big to parse by hand.
  - `sha2` for file checksums (P3.6) and image digests (P3.12).
  - `smoltcp` for the NAT stack (P3.7b to P3.7d), with default features off and only Ethernet, IPv4, TCP and UDP on.
- **Not added:**
  - `vm-memory` and `virtio-queue`. The VMM keeps its own `memory` module and a small hand-written virtqueue, which also keeps `unsafe` in one place.
  - No async runtime. The daemon uses threads and std Unix sockets.

Each crate is still added in the PR that first needs it, and passes `cargo deny` and `cargo audit` there.

## Consequences

- Binary data in `proto` is base64 encoded by a small function in the crate, rather than by another dependency.
- Whether `libc` is added and `unsafe` allowed outside `hvf` and the `memory` module is a separate question. It was answered as D24 in decision 0007.
