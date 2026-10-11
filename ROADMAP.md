# Roadmap

This is the implementation plan for cave-vm v1. It breaks the six phases in [docs/design.md](docs/design.md) into tasks that agents and contributors pick up one at a time.

**The design wins.** This file only orders and sizes the work. If this plan and `docs/design.md` disagree, the design is right and this file is the bug. Nothing here changes a decided item. Anything the design leaves open goes under [Needs Ivan's decision](#needs-ivans-decision) with a recommendation, and stays open until Ivan answers. Do not build on an open decision; pick another task. Answered items move to [Decisions](#decisions).

## How to use this file

- Phases are ordered, not dated. A phase starts when the previous gate closes. Tasks inside a phase can run in parallel unless a dependency is listed.
- Each task is sized as one small PR. Task IDs (`P3.4`) go in the PR title or description so progress can be traced. If a task turns out bigger than one PR, split it and say so in the PR.
- Each phase becomes a GitHub milestone, and its gate is the closing criterion (design, "How Claude Code works in this repo", item 6).
- `[A3]` marks a claim that depends on an assumption from the [assumptions register](#assumptions-to-verify). Treat these as unverified until the phase that checks them records the result.
- `[D4]` marks a task shaped by a decision in the [Decisions](#decisions) table.
- Before starting a task: read the design section it names, write a short plan, and write tests first where practical (AGENTS.md workflow).

## Phases and gates

The phases and gates are drawn in `docs/design.md` under "Milestones and planning phases" and recorded in [decision 0001](docs/decisions/0001-roadmap-phases-and-gates.md). Gate N closes phase N for phases 1 to 5, and phase 6 ends at the v1 release. The gate criteria below restate the design's; if they ever differ, the design wins.

| Phase | Name | Closes with |
| --- | --- | --- |
| 1 | Spec | Gate 1 |
| 2 | Spike | Gate 2 |
| 3 | Core VM | Gate 3 |
| 4 | Snapshot and fork | Gate 4 (numbers from the spike) |
| 5 | Agent layer | Gate 5 |
| 6 | Density and polish | v1 release |

---

## Phase 1: Spec

**Goal.** Write the user-facing surface before any code: the README quickstart, the CLI reference, the config file and the wire protocol. Design principle 7: if the quickstart takes more than two commands, simplify the design.

**Crates touched.** None with code. Docs and repo setup only.

**Depends on.** Nothing.

| ID | Task | Tests / check |
| --- | --- | --- |
| P1.1 | `docs/cli.md`: every command in the design's CLI table, with its flags, defaults, output and exit codes, and each error message with its fix. Flags the design names (`--net off`, `--net lan`, `-p 3000`, `cave agent --in-vm`, `cave rm` confirmation) are written as specified. Any flag or output format the design does not name is marked "proposed" in the PR for Ivan to accept or cut. | Every command in the design table appears; the quickstart is still two commands. |
| P1.2 | `docs/config.md`: the `cave.toml` reference. The keys in the design example (`[vm] image cpus memory`, `[storage] mode`, `[network] mode`) are fixed. Key names for things the design says are configurable but does not name (idle auto-suspend time, automatic snapshots and deletion, optional TTL, turning off the update detector) are proposed here and approved in this PR [D10]. | Example file parses as TOML (checked once the workspace exists). |
| P1.3 | `docs/protocol.md`: the guest agent protocol and daemon API. Length-prefixed JSON, versioned from day one, streamed output, a pseudo-terminal for `cave shell`, and the same format on the daemon socket (design, "Interfaces, defaults and process"). Framing width, maximum frame size and the version handshake are proposed in the PR. Message list: exec, stream chunk, exit status, pty open/resize/data, file read, file write, port forward, snapshot/restore notifications, error. | Reviewed by Ivan; becomes the source for `proto` in P3.1. |
| P1.4 | `CHANGELOG.md` in Keep a Changelog format with an Unreleased section, since the definition of done requires changelog updates. | File exists and is linked from CONTRIBUTING.md. |
| P1.5 | `.claude/settings.json`: allow cargo commands, deny force-push and destructive deletes (design, "How Claude Code works in this repo", item 5). | No `CLAUDE.md` or `CLAUDE.local.md` is added (item 1). |
| P1.6 | Create the six GitHub milestones with each gate's criteria as the description. Needs repo admin access, so Ivan or an agent he authorizes does it. | Milestones exist. |
| P1.7 | Fix the README's `cave fork` row to match the design ("create a new VM from a snapshot, copy-on-write") [D12]. | README and design agree. |

**Exit gate 1.** P1.1 to P1.3 are merged and approved by Ivan, every proposed item in them is accepted or cut, and the README quickstart is still two commands.

---

## Phase 2: Spike

**Goal.** Boot a Linux kernel to a shell through Hypervisor.framework, confirm the signing and snapshot assumptions, and measure a performance baseline. The spike answers questions; it does not need production-quality device models.

**Crates touched.** `hvf`, `vmm` (minimal boot path and the backend trait), `image` (kernel config). Workspace skeleton across all crates.

**Depends on.** Gate 1. P2.1 first; P2.3 and P2.4 before P2.6.

| ID | Task | Tests / check |
| --- | --- | --- |
| P2.1 | Workspace skeleton: root `Cargo.toml` with every crate in the AGENTS.md map as an empty member, published names prefixed `cave-vm-`, the toolchain pinned in `rust-toolchain.toml` (version per [D7]), `#![deny(unsafe_code)]` in every crate except `hvf` and the guest-memory module [D5], and a `deny.toml`. | `cargo build/test/fmt/clippy --workspace` pass on an empty workspace. |
| P2.2 | CI workflow with the one required check: fmt, clippy `-D warnings`, test, `cargo deny`, `cargo audit`, no secrets. GitHub-hosted macOS runners probably cannot run Hypervisor.framework guests [A7], so hardware tests are excluded here and run on the self-hosted Mac [D11]. Branch protection is Ivan's to set. | Workflow passes on the skeleton. |
| P2.3 | `hvf`: the safe wrapper. VM create and destroy, IPA size config [A4], memory map and unmap, vCPU create and run, exit reasons, general and system register get/set, virtual timer [A2], GIC create and state get/set if available [A1]. Every `unsafe` block has a `// SAFETY:` comment. | Hardware tests behind an `e2e` feature or `#[ignore]`, run on a real Mac. Pure logic (exit decoding, register tables) unit-tested anywhere. |
| P2.4 | Guest memory: allocate guest RAM as an `mmap`ed region (anonymous or file-backed), map it through `hvf`, with 16 KiB host page alignment [A3]. Location per [D5]. | Unit tests for layout and alignment; hardware test for map/unmap. |
| P2.5 | Backend trait in `vmm` with a mock backend that records memory maps, register writes and injected exits. The `hvf` implementation is an adapter in `vmm` over the `hvf` safe API. | The mock drives a scripted vCPU exit sequence in tests. |
| P2.6 | Minimal boot path in `vmm`: load the kernel `Image`, generate a device tree (memory, CPUs, PSCI, timer, GIC, one UART), handle PSCI calls trapped through HVC, and a UART console on stdin/stdout. | Device tree golden-file test; UART tests against the mock backend; boot test on a real Mac. |
| P2.7 | Spike kernel config in `image/`: minimal arm64 kernel plus a busybox initramfs, enough to reach a shell on the serial console. Container support is not needed yet. Build environment per [D6]. | Kernel boots to a shell in P2.6. |
| P2.8 | Ad hoc signing check: sign the `vmm` binary on a clean macOS 26 machine with `codesign -s -` and the `com.apple.security.hypervisor` entitlement, with no paid Apple account, then create a VM [A5]. Repeat for a binary installed the way Homebrew and the install script would install it. Also try creating a vmnet interface with the same signature [A6]. | Written result in the spike report (P2.11). |
| P2.9 | Snapshot experiment: pause a running guest, save vCPU registers, system registers, timer state, interrupt-controller state and guest memory to files, then restore them in a new process and confirm the shell keeps running [A1, A2]. | Manual run on a real Mac, steps recorded so they can be repeated. |
| P2.10 | Fork experiment: restore one memory snapshot into two processes at once using copy-on-write mappings (`MAP_PRIVATE` on the memory file, or a `clonefile` of it) [A3]. | Both guests run independently; timings recorded. |
| P2.11 | Spike report as a decision record in `docs/decisions/`: boot time, restore time, fork time and idle memory per VM, the signing result, the vmnet result, the interrupt-controller result, and proposed success targets for gate 4 (design: exact numbers are set after the spike measures a baseline). Ivan approves the targets. | Numbers come with the machine, macOS version and method used. |

**Spike questions** (each answered in P2.11 with a measurement, not an opinion):

1. Does an ad hoc signature with the hypervisor entitlement, made on the user's machine without a paid account, let the binary create VMs on macOS 26? Does it survive a Homebrew install and an upgrade? [A5]
2. Does vmnet need an entitlement that ad hoc signing cannot get? [A6] The design has already decided on a userspace NAT. A "vmnet works" result does not change that without a decision record.
3. Can interrupt-controller state be saved and restored exactly, through the framework's GIC API or otherwise? [A1]
4. Is all vCPU state needed for an exact restore reachable through the framework, including the virtual timer offset? [A2]
5. Can guest memory be restored copy-on-write from a file, and how long does it take for a 2 GB guest? [A3]
6. What are boot, restore and fork times, and idle memory per VM, for the 2 CPU / 2 GB default?

**Exit gate 2.** A kernel boots to a shell through Hypervisor.framework, the signing assumption is confirmed, a snapshot of a running guest restores in a new process and keeps running, and P2.11 is approved with baseline numbers and gate 4 targets.

**If gate 2 fails on interrupt-controller state:** stop. Write a decision record with the options [D8] and get Ivan's answer before any phase 3 work that assumes the current snapshot design. Phase 3 device work can continue only if it does not depend on the choice.

---

## Phase 3: Core VM

**Goal.** A usable VM for one project: block, network and vsock devices, the guest agent, the daemon and the basic CLI. Networking is a large piece of this phase (design, "NAT implementation") and is split into several PRs.

**Crates touched.** `proto`, `vmm`, `agent`, `daemon`, `cli`, `image`.

**Depends on.** Gate 2. P3.1 before P3.6, P3.10 and P3.11. P3.2 before P3.3, P3.5 and P3.7a. Dependencies per [D4].

| ID | Task | Tests / check |
| --- | --- | --- |
| P3.1 | `proto`: message types and the length-prefixed JSON codec from `docs/protocol.md`, with the protocol version in every connection handshake. | Round-trip tests for every message; tests for truncated, oversized and wrong-version frames. |
| P3.2 | `vmm`: virtio-mmio transport and virtqueue handling. No PCI, ACPI or UEFI. Devices appear in the generated device tree. | Mock-backend tests drive queues through the guest-memory abstraction. |
| P3.3 | virtio-blk on a raw sparse image: read, write, flush, discard. | Tests against a temp file; flush ordering test. |
| P3.4 | Project disk: create the 20 GB sparse disk as an APFS `clonefile` of the shared base image [A9]; grow on request. No absolute paths stored in `.cave/`. | Test that a clone shares blocks until written; test that moving the project folder keeps it working. |
| P3.5 | virtio-vsock device with a host-side Unix socket per port, inside `.cave/`. | Mock-backend tests for connect, data, close and reset. |
| P3.6 | `agent`: static musl binary, listens on vsock, handles exec with streamed stdout and stderr and exit status, the pty shell with resize, file read and write, and port forwarding. Cross-build per [D6]. | Protocol and session logic unit-tested on the host; Linux-specific code behind `cfg(target_os = "linux")` and covered by e2e tests in a real guest. |
| P3.7a | virtio-net device. | Mock-backend frame tests. |
| P3.7b | Userspace network stack: ARP, IPv4, DHCP or a fixed guest address, and the NAT gateway [D4]. | Unit tests with recorded frames; no real network. |
| P3.7c | Outbound TCP through host sockets. | Tests against an in-process fake socket layer. |
| P3.7d | Outbound UDP and DNS forwarding through the gateway. | Same. |
| P3.7e | Network policy: in `nat` mode deny 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 and the Mac itself, while still allowing the gateway for DNS and the daemon; `--net lan` allows private ranges; `--net off` removes the device. Ships with P3.7c so NAT never reaches the LAN by default. | A test per denied range, the Mac's own addresses, the gateway exception, and each mode. |
| P3.8 | Port forwarding to the Mac (`-p 3000`), none by default. | E2E test on a real Mac. |
| P3.9 | Base image build in `image/`: Debian arm64 raw image with git, node, npm, python, curl, vim and openssh-server, the agent installed as the init-started service, and a published digest. The full kernel config adds namespaces, cgroups, overlayfs and netfilter so Docker works. Ubuntu comes in P6.8. Publishing to GitHub releases touches release automation, which stays with Ivan. | E2E: Docker runs a container inside the VM. |
| P3.10 | `daemon`: Unix socket with user-only permissions, the API from `proto`, one VMM process per VM, running and stopped states, cleanup of crashed VMM processes at startup. | Tests with a fake VMM process: spawn, crash, restart, socket permissions. |
| P3.11 | `cli`: `cave init` (writes `cave.toml` and `.cave/`, and adds `.cave/` to `.gitignore`; skill files come in P5.7), `run`, `shell`, `exec`, `ls`, `rm` (asks for confirmation), `cp`, `images`. `cave run` prints the policy at startup, and widening flags are echoed back. | CLI tests against a fake daemon; error messages say how to fix the problem. |
| P3.12 | Image fetch and digest pinning: `image = "base"` is pinned to a digest on first use. The fetcher is a trait so tests use a local file source, never the network. | Digest mismatch and partial download tests. |
| P3.13 | VMM sandbox profile: the VMM process can reach only `.cave/` and the shared image cache [A8]. | E2E test that the VMM cannot read a file outside those paths. |

**Exit gate 3.** On a real Mac with ad hoc signing: `cave init && cave run && cave exec -- uname -a` works; `cave shell` is interactive with resize; outbound HTTPS and `apt` work; every denied LAN range is unreachable in an e2e test; Docker runs a container inside the VM; killing a VMM process is recovered by the daemon. After gate 3, development of cave-vm moves inside a cave VM (design item 7).

---

## Phase 4: Snapshot and fork

**Goal.** The headline feature: snapshot, restore and fork whole running projects quickly, with processes intact.

**Crates touched.** `vmm`, `daemon`, `cli`, `proto`, `agent` (reconnect after restore).

**Depends on.** Gate 3, and gate 2's interrupt-controller answer. P4.1 and P4.2 before P4.5.

| ID | Task | Tests / check |
| --- | --- | --- |
| P4.1 | Versioned snapshot format: a manifest with format version, VM config and metadata, disk state (an APFS clone), memory state and device state. The metadata has a field for injected credential names from the start (filled in P5.3). | Round-trip tests; a newer or unknown version triggers the fallback in P4.5. |
| P4.2 | Save and restore state for every device: UART, virtio-mmio transport, blk, net, vsock, plus vCPU and interrupt-controller state. | Mock-backend tests that save mid-queue, restore and continue. |
| P4.3 | Agent and vsock across restore: the guest agent reconnects after a vsock transport reset, and resets the guest clock from the host. | E2E: `cave exec` works right after a restore; clock is correct. |
| P4.4 | Network across restore: open host-side flows are dropped cleanly and the guest sees resets, not hangs. | Unit tests in the network stack; e2e reconnect test. |
| P4.5 | `cave snapshot [name]` and `cave restore [name]`. Default names follow `[ProjectFolder]-[YYYY-MM-DD]-[Distro]` with a counter on collisions (`-2`). If memory state cannot be loaded, restore falls back to the disk state and a normal boot, and says so. | Name generation tests; fallback test with a corrupted memory file. |
| P4.6 | `cave fork [name]`: a new VM from a snapshot, stored at `.cave/forks/<name>`, with the disk cloned and memory restored copy-on-write [A3]. | E2E: parent and fork diverge independently; fork time measured against the gate 4 target. |
| P4.7 | Lifecycle: the suspended state, auto-suspend after the configured idle time, suspend on Mac sleep [A10], restore from the last snapshot after a reboot, and states shown in `cave ls`. | Daemon tests with a fake clock and fake power events. |
| P4.8 | Automatic snapshots and automatic deletion, both configured in `cave.toml` (key names from P1.2). | Retention tests. |
| P4.9 | `cave gc`: show disk usage and reclaim it. Shared APFS clone blocks are reported honestly rather than double-counted [A9]. | Tests on cloned temp files. |
| P4.10 | `cave export`: move a project or snapshot out as files. | E2E round trip. |
| P4.11 | Benchmark harness for boot, restore and fork time and idle memory, run on the self-hosted Mac. | Results compared against the P2.11 targets. |

**Exit gate 4 (the headline demo).** On a real Mac: start a project VM with a running process, snapshot it, break the VM, restore it with the process intact, and fork it into several copies, each in well under a second. The exact restore and fork targets are the ones approved in P2.11. A snapshot written before a `cave` upgrade still loads, or falls back to a disk boot.

---

## Phase 5: Agent layer

**Goal.** Make the VM the agent's sandbox: `cave init` skill files, the MCP server, credentials, the safety policy and the audit log. Guidance plus a guardrail, with the limits stated honestly.

**Crates touched.** `sdk` (Rust), `mcp`, `cli`, `daemon`, `vmm` (network audit events), `agent`.

**Depends on.** Gate 4. P5.1 before P5.2.

| ID | Task | Tests / check |
| --- | --- | --- |
| P5.1 | Rust `sdk` over the daemon socket: `Sandbox::open(".")`, `exec`, `snapshot`, `fork`, `restore`, `destroy`. Built before the MCP server, which uses it. | Tests against a fake daemon. |
| P5.2 | `mcp` server: tools for exec, read file, write file, snapshot, fork, restore and destroy, resolving the VM from the working directory [D4]. | Tool tests against a fake daemon. |
| P5.3 | Credentials: a call names a credential (for example `API_KEY`), the host resolves it from `.env` or a similar source, and the value is injected into the command being run and never returned to the agent. The audit log records the name only. Snapshot metadata records which credentials were injected, and forking or exporting such a snapshot shows a warning. If the command prints the value, the exact value is redacted, best effort, from streamed output and MCP responses, and the docs state that a command can still leak it through files or the network [D9]. | Tests that the value never appears in MCP responses, the audit log or snapshot metadata; redaction tests for output split across chunks; warning tests. |
| P5.4 | Audit log in `.cave/`: exec calls, file writes and network connections (from the NAT stack). | Tests per event type; no credential values. |
| P5.5 | `cave inspect`: policy, resources and audit log. Optional TTL for disposable VMs. | CLI tests. |
| P5.6 | One skill source in the repo, used both by `cave init` and as the bootstrap skill on the website. | Test that `cave init` writes exactly that text. |
| P5.7 | `cave init` writes the `AGENTS.md` section, a `.claude/skills/` entry, and, where the agent supports it, a permissions config that denies host Bash. | Tests on a temp project; existing `AGENTS.md` content is preserved. |
| P5.8 | `cave agent <name>`: start the agent on the Mac, connected to this VM through the MCP server, with host Bash denied where the agent supports it. `--in-vm` runs the agent inside the VM instead, which needs the VM's network ([decision 0002](docs/decisions/0002-cave-agent-runs-on-the-mac.md)). | E2E with Claude Code on a real Mac, both modes. |
| P5.9 | Remote editing: sshd in the VM reached over vsock through a `cave` proxy command, with keys generated automatically, so VS Code Remote and Zed work. | E2E connection test. |
| P5.10 | Synced mode: `cave sync` and `cave pull` with the per-file SHA-256 rules, an automatic snapshot before a pull, and dependency and build folders excluded. | Tests for each rule: Mac-only change applied, VM-only change left, both-sides change stops the pull and is listed; exclusions. |
| P5.11 | Docs: the agent boundary limit (an agent with a host shell can step outside the VM), the threat model, and that the mirror backs up source only. | Reviewed against the design's wording. |

**Exit gate 5.** On a real Mac: after `cave init`, Claude Code completes a task using only the MCP server with host Bash denied; `cave inspect` shows the policy and an audit log of what it did; a credential is used by a command without appearing in any MCP response or the audit log; snapshot before and restore after an agent run works.

---

## Phase 6: Density and polish

**Goal.** Low memory, zero-setup install and the remaining surface, ending in the v1 release.

**Crates touched.** `vmm`, `daemon`, `cli`, `image`, `sdks/python`, `sdks/typescript`.

**Depends on.** Gate 5.

| ID | Task | Tests / check |
| --- | --- | --- |
| P6.1 | virtio-balloon, and a warning when host memory gets tight (no hard cap on VM count). | Mock-backend tests; idle memory measured on the self-hosted Mac. |
| P6.2 | `cave doctor`: entitlements, macOS version, disk space and image state, each failure with its fix. | Tests with each check failing. |
| P6.3 | First-run setup: fetch kernel and base image, ad hoc sign with the hypervisor entitlement [A5], create the data directory, run the health check. | E2E on a clean Mac account. |
| P6.4 | Python and TypeScript SDKs, thin clients over the daemon socket with no native extensions. | Tests against a fake daemon. |
| P6.5 | Homebrew formula and the install script, pinned to a version and checksum. Release and signing automation stays with Ivan. | Install on a clean Mac. |
| P6.6 | Bootstrap skill page text: check Apple Silicon and macOS version, install, `cave doctor`, `cave init`, verify with `cave exec -- uname -a`, show `cave inspect`; the agent shows each command before running it and fetches nothing further. | Text matches P5.6. |
| P6.7 | Image update detector: a plain version lookup with no identifying data, never auto-updates, can be turned off in `cave.toml`. The only network request `cave` makes. | Tests with an injected fetcher; test that turning it off sends nothing. |
| P6.8 | Ubuntu base image. | E2E boot. |
| P6.9 | README status, docs and changelog brought up to date for release. | Quickstart works as written on a clean Mac. |

**Exit: v1 release.** Gate 5 still holds, P6.1 to P6.9 are merged, and the quickstart works as written on a clean Mac.

---

## Cross-cutting rules

- **Testing.** Device models are tested against the mock backend, with no network in any test. Hardware and end-to-end tests run on the self-hosted Mac and are kept out of the default `cargo test` run.
- **unsafe.** Only in `hvf` and the guest-memory code, each block with a `// SAFETY:` comment. Every other crate has `#![deny(unsafe_code)]`.
- **Dependencies.** None added without asking first, with the reason. [D4] lists the ones this plan expects to request, asked for once per phase.
- **Off limits to agents.** Signing, release and CI secrets, branch protection, and publishing images or releases.
- **Changelog.** Every user-visible change adds a line under Unreleased in `CHANGELOG.md`.

## Assumptions to verify

None of these are established. Each is checked in the task listed, and the result is written down before anything depends on it.

| ID | Assumption | Checked in |
| --- | --- | --- |
| A1 | Hypervisor.framework on macOS 26 offers a GICv3 interrupt controller whose full state can be read and written back (the `hv_gic_*` API added in an earlier macOS release is the expected route). | P2.3, P2.9 |
| A2 | All vCPU state needed for an exact restore, including the virtual timer offset, is readable and writable through the framework. | P2.3, P2.9 |
| A3 | Guest memory can be backed by a file mapped copy-on-write (`MAP_PRIVATE` or a `clonefile` of the memory file) and mapped into a guest, with 16 KiB host page alignment, fast enough for sub-second forks. | P2.4, P2.10 |
| A4 | The configurable guest physical address size is large enough for the memory sizes users will set. | P2.3 |
| A5 | An ad hoc signature with `com.apple.security.hypervisor`, applied on the user's machine without a paid Apple account, lets `cave` create VMs on macOS 26, and survives Homebrew and install-script installs and upgrades. | P2.8, P6.3 |
| A6 | vmnet needs an entitlement that ad hoc signing cannot get (the design says "probably"). | P2.8 |
| A7 | GitHub-hosted macOS runners cannot run Hypervisor.framework guests, so hardware tests need the self-hosted Mac. | P2.2 |
| A8 | A macOS sandbox profile can still be applied to a CLI-launched VMM process on macOS 26. | P3.13 |
| A9 | The project folder is on APFS, so `clonefile` works; other file systems get a clear error. | P3.4 |
| A10 | The daemon can get Mac sleep and wake notifications in time to suspend VMs. | P4.7 |
| A11 | macOS 26 is the right minimum: everything above is available there, and nothing the plan needs requires a later release. | P2.11 |

## Needs Ivan's decision

None open. When a task hits something the design and the decisions below do not cover, add it here with a recommendation and stop work that depends on it.

## Decisions

Answered by Ivan. D1 to D3 are recorded in [decision 0001](docs/decisions/0001-roadmap-phases-and-gates.md) and [decision 0002](docs/decisions/0002-cave-agent-runs-on-the-mac.md), and D4 to D12 in [decision 0003](docs/decisions/0003-roadmap-open-items.md).

| ID | Question | Decision |
| --- | --- | --- |
| D1 | Phases and gates, since the design's roadmap diagram was lost in the export. | Redrawn in `docs/design.md`; gate N closes phase N for phases 1 to 5, and phase 6 ends at the v1 release. |
| D2 | What `cave agent claude` does. | Runs the agent on the Mac through the MCP server; `--in-vm` runs it inside the VM. |
| D3 | Phases for decided items the README phases did not place. | Sandbox profile in phase 3; Rust SDK, remote editing and synced mode in phase 5; Python and TypeScript SDKs and the Ubuntu image in phase 6. |
| D4 | Dependencies this plan expects to ask for. Each still needs its own request with a reason. | Ask per phase, not per crate: phase 2 `vm-fdt` (device tree) and `serde`/`serde_json`; phase 3 `vm-memory` and `virtio-queue` (rust-vmm, used by Firecracker-style VMMs), `smoltcp` (userspace TCP/IP for the NAT), `clap`, `sha2`; phase 5 an MCP server library or a small hand-written one. Prefer writing small pieces ourselves over pulling in large frameworks. |
| D5 | Where the guest-memory code lives. `unsafe` is allowed "in the guest-memory code" but the crate map has no memory crate. | A single `memory` module inside `vmm`, the only module in that crate allowed to use `unsafe`, rather than a new crate (the layout is decided). |
| D6 | Where the kernel, the base image and the musl guest agent are built. Building Linux artifacts on a Mac needs a cross toolchain or a Linux build environment. | Release builds in CI on Linux runners (release automation set up by Ivan); local builds inside a cave VM once gate 3 passes; before that, a documented cross toolchain for the agent and a container for the kernel. |
| D7 | Rust toolchain version to pin in `rust-toolchain.toml`. | The current stable release when P2.1 lands, bumped deliberately in its own PR. |
| D8 | Contingency if gate 2 shows interrupt-controller state cannot be saved and restored through the framework. | Decide at gate 2 with a decision record. The options to compare are a userspace GIC emulated in the VMM, or snapshots taken only at points where interrupt state can be reconstructed. |
| D9 | "Never returned to the agent": what if the command prints the injected credential to its own output? | Best-effort redaction of the exact value in streamed output and MCP responses, plus a documented limit that a command can still leak it in other ways (files, network). |
| D10 | Key names in `cave.toml` for idle auto-suspend, automatic snapshots and deletion, TTL and the update detector. | Propose them in P1.2 and approve them there. |
| D11 | CI for hardware tests: registering the self-hosted Mac runner, when e2e runs (every PR or nightly), and branch protection. | Self-hosted runner runs e2e on PRs from the repo itself while it is private; revisit before going public, since self-hosted runners on public repos run untrusted code. Ivan registers the runner and sets protection. |
| D12 | README describes `cave fork` as "create a copy-on-write copy of a project VM"; the design says "create a new VM from a snapshot, copy-on-write". | Align the README with the design in P1.7. |
