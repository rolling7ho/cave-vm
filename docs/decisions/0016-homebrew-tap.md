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

- `docs/design.md` shows the tap command and the install script's URL; Ivan asked for this in the release review on 2026-10-15. It changes back to `brew install cave-vm` if the formula later moves to homebrew/core.
- Releasing means tagging, uploading the images with a manifest of their release URLs, updating the formula's `url` and `sha256` in the tap, and uploading `cave-vm-<version>-aarch64-apple-darwin.tar.gz` (`cave`, `cave-daemon`, `cave-vmm`, `LICENSE` and `scripts/hypervisor.entitlements` in a `cave-vm-<version>` folder) with a copy of `scripts/install.sh` whose `@VERSION@` and `@SHA256@` are filled in, for people without Homebrew. `scripts/release.sh <version>` does these steps after the version bump is merged.
