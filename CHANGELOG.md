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
