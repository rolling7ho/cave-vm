# Changelog

All notable changes to cave-vm are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once there is a release.

## [Unreleased]

### Fixed

- Restored spike snapshots got no more timer interrupts once the guest went idle, so they hung at 100% CPU. `hv_gic_state_get_data` leaves out each redistributor's SGI and PPI state; snapshots now save it per vCPU (format version 3, so older spike snapshots are refused).
- Device MMIO loads now honor sign extension (`LDRSB`, `LDRSH`, `LDRSW`) and 32-bit destination registers.
- A virtqueue buffer whose address range would wrap past the top of the address space is an error instead of an overflow, and an oversized `QueueNum` leaves the queue invalid instead of truncating to a valid size.
- A restored spike VM saved again now records the time of the second save, not of the restore, for its timers.

### Added

- `cave-vm-daemon`: the daemon socket (`daemon.sock`, user-only) with the hello handshake, one VMM process per VM with `start`, `stop` and `list`, crash detection so the next `start` relaunches a crashed VMM, and startup cleanup of VMM processes an earlier daemon left behind. The VMM control link and its messages (`cave-vm-proto::vmm`) are specified in `docs/protocol.md` (P3.10a).
- `cave-vm-proto`: the daemon API requests and replies, with the reply objects written down in `docs/protocol.md` (P3.1b).
- `cave-vm-proto`: the length-prefixed JSON frame codec, the `hello` version handshake, `error` messages and every guest agent message from `docs/protocol.md` (P3.1).
- Image names and digests (`debian@sha256:...`) and the shared image cache in `cave-vm-daemon`: downloads are hashed as they are written and kept only when size and SHA-256 match, so an interrupted or wrong download leaves nothing behind (P3.12a).
- Design document, roadmap and decision records.
- Spec for the `cave` command line in `docs/cli.md`.
- Reference for `cave.toml` in `docs/config.md`.
- Guest agent and daemon wire protocol in `docs/protocol.md`.
- Cargo workspace with an empty crate for each component, the Rust toolchain pinned to 1.99.0, and a `deny.toml`.
- CI that runs `cargo fmt`, `cargo clippy`, the tests, `cargo deny` and `cargo audit` on every pull request.
- `cave-vm-hvf`: a safe wrapper around Hypervisor.framework for VMs, guest memory mappings, vCPUs, registers, the virtual timer and the GICv3 interrupt controller, including saving and restoring its state.
- Guest memory in `cave-vm-vmm`: page-aligned regions from anonymous memory or a file mapped copy-on-write or shared, mapped into a VM through `cave-vm-hvf`.
- Spike kernel config and busybox initramfs in `image/spike/`, built in an Apple container, that boot to a shell in about 40 ms (`docs/spike/p2.7-boot.md`).
- A backend trait in `cave-vm-vmm` with a mock backend for tests and a Hypervisor.framework backend.
- A direct kernel boot path: arm64 Image loading, a generated device tree, PSCI, a PL011 UART console and the virtual timer, with a `cave-vmm-spike` binary that boots a kernel on the terminal. Input wakes the vCPU (Hypervisor.framework handles guest WFI itself), and `--stop-at` stops the VM when the guest prints a given text, for boot timing.
- Spike snapshots: `cave-vmm-spike --snapshot` saves a paused guest (vCPU, GIC, timer, UART and sparse memory file) and `--restore` resumes it in a new process in about 3 ms (`docs/spike/p2.9-snapshot.md`).
- Fork experiment: `scripts/spike/fork-check.sh` restores one snapshot into two VMs at once with copy-on-write memory and checks they diverge (`docs/spike/p2.10-fork.md`).
- Spike report with baseline numbers and the approved gate 4 targets (`docs/decisions/0005-spike-report.md`).
- 2-vCPU spike numbers, measured with `scripts/spike/vcpu-bench.py`, as an addendum to the spike report. The gate 4 targets stand.
- Virtio-mmio transport (version 2) and split virtqueues in `cave-vm-vmm`, with indirect descriptors, level-triggered interrupts and 32 device slots in the device tree. Malformed queues from the guest are errors, never VMM crashes.
- Secondary vCPUs: PSCI `CPU_ON`, `CPU_OFF` and `AFFINITY_INFO`, each vCPU on its own thread, and `cave-vmm-spike --cpus <n>` to boot, snapshot and restore with more than one. Spike snapshots record each vCPU's power state.
