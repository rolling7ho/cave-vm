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
