# 0016: Install from a custom Homebrew tap

Date: 2026-10-15 · Decided by: Ivan · Status: accepted

## Context

The design's install command is `brew install cave-vm`, which assumes a formula in homebrew/core. Homebrew core reviews new formulae for notability, which a project that has just gone public does not meet. Asked in the project thread whether to use a custom tap or core, Ivan chose a custom tap.

## Decision

- The formula lives in the tap `rolling7ho/homebrew-cave`, made with `brew tap-new` as Homebrew's tap guide describes, in `Formula/cave-vm.rb`.
- The install command is `brew install rolling7ho/cave/cave-vm`. The docs, the README and the agent skill say so.
- The formula builds the tagged source with cargo and signs `cave-vmm` ad hoc with the hypervisor entitlement during install. `cave` also signs it on first run if needed (P6.3).
- Each GitHub release carries the base images, the kernel and `images.json` (D28), so the first run finds them at the stable `releases/latest` URL.

## Consequences

- `docs/design.md` still shows `brew install cave-vm`; it becomes true if the formula later moves to homebrew/core.
- Releasing means tagging, uploading the images with a manifest of their release URLs, and updating the formula's `url` and `sha256` in the tap.
