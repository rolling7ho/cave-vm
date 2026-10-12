# Changelog

All notable changes to cave-vm are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once there is a release.

## [Unreleased]

### Added

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
- Virtio-mmio transport (version 2) and split virtqueues in `cave-vm-vmm`, with indirect descriptors, level-triggered interrupts and 32 device slots in the device tree. Malformed queues from the guest are errors, never VMM crashes.
