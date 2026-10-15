## What and why

<!-- What does this change do, and why? Link the issue or design note it comes from. -->

Closes #

## Design

<!-- Which section of docs/design.md does this follow? If it changes a decided item, link the decision record in docs/decisions/. -->

## How it was tested

<!-- Unit tests, mock-backend tests, or end-to-end runs on a real Mac. Paste the commands. -->

## Safety

<!-- Does this change what a VM can reach, or how credentials, snapshots, the network or the update flow behave? Say how, or write "no change". -->

## Checklist

- [ ] The change is focused and the description explains why.
- [ ] `cargo fmt`, `cargo clippy -D warnings` and the tests pass.
- [ ] Docs and the changelog are updated if behavior changed.
- [ ] Every new `unsafe` block has a `// SAFETY:` comment.
- [ ] Any new dependency is explained above, and `cargo deny` and `cargo audit` pass.
- [ ] Commits follow Conventional Commits and are signed off (`git commit -s`).
