# Contributing to cave-vm

Thanks for helping. cave-vm is an open-source (Apache 2.0) microVM runtime for macOS, built for coding agents. This guide covers how to propose changes and what a good pull request looks like.

## Before you start

- Read `docs/design.md`. It is the source of truth for what cave-vm does and why. Decisions marked as decided there are not changed in a pull request.
- For anything larger than a small fix, open an issue with a short design note first. Say what you want to change, why, and which part of the design it touches.
- Found a security problem? Do not open a public issue. Follow `SECURITY.md` instead.
- Everyone taking part is expected to follow `CODE_OF_CONDUCT.md`.

## Requirements

- macOS 26 or later on Apple Silicon. The VMM needs Hypervisor.framework, so it cannot be developed or tested on other hosts.
- The Rust toolchain pinned in `rust-toolchain.toml`. `rustup` installs it automatically when you run cargo in the repo.

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo deny check && cargo audit
```

Tests must not use the network. Device models are tested against the mock hypervisor backend, so most tests run without booting a VM. End-to-end tests need a real Mac with the hypervisor entitlement and are run separately.

## Making a change

1. Fork the repo and branch from `main`.
2. Make one focused change. Small pull requests get reviewed faster and merged sooner.
3. Add or update tests, and write them first where practical.
4. Update docs and the changelog if behavior changes.
5. Open a pull request against `main`.

`main` is protected. Pull requests need the required CI check to pass and are squash-merged.

## Commit messages

Use Conventional Commits, so changelogs can be generated:

```
feat(vmm): add virtio-vsock device
fix(daemon): wait for the VM before syncing
docs: clarify snapshot naming
```

Common types are `feat`, `fix`, `docs`, `refactor`, `test`, `build` and `ci`. Add `!` or a `BREAKING CHANGE:` footer for breaking changes.

## Sign your commits (DCO)

cave-vm uses the Developer Certificate of Origin instead of a contributor license agreement. Sign off each commit to say you have the right to submit it under the project license:

```sh
git commit -s -m "fix(daemon): wait for the VM before syncing"
```

This adds a `Signed-off-by: Your Name <you@example.com>` line. Pull requests with unsigned commits fail the DCO check.

## Code standards

- Run `cargo fmt` and fix every `cargo clippy` warning.
- `unsafe` is allowed only in the Hypervisor.framework wrapper (`hvf`) and the guest-memory code. Every `unsafe` block needs a `// SAFETY:` comment explaining why it is sound.
- Do not add a dependency without a stated reason in the pull request. `cargo deny` and `cargo audit` must pass.
- Keep the user-facing surface small: names instead of IDs, sensible defaults, and errors that say how to fix the problem.
- Credentials are injected into the command being run and never returned to the agent. Logs record credential names, never values.

## AI-assisted contributions

AI-assisted contributions are welcome. Say so in the pull request description, and name the tool if you like. You are responsible for everything you submit: you have read it, understood it, tested it, and can answer review questions about it. The DCO sign-off applies to you, not to the tool.

## Pull request checklist

- [ ] The change is focused and the PR description explains why.
- [ ] `cargo fmt`, `cargo clippy -D warnings` and the tests pass.
- [ ] Docs and the changelog are updated if behavior changed.
- [ ] Every new `unsafe` block has a `// SAFETY:` comment.
- [ ] Any new dependency is explained.
- [ ] Commits follow Conventional Commits and are signed off.
- [ ] AI assistance is disclosed, if used.

## License

By contributing, you agree that your contributions are licensed under the Apache License 2.0, the same license as the project.
