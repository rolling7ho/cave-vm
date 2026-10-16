# 0004: CLI and config open items D13 to D22

Date: 2026-10-12 · Decided by: Ivan · Status: accepted; D19's `cave export` renamed `cave pull` by [decision 0029](0029-cave-pull-and-cave-push.md) (D58)

## Context

Writing the phase 1 spec (`docs/cli.md`, `docs/config.md`, ROADMAP tasks P1.1 and P1.2) turned up ten questions the design does not answer. Each was listed in `ROADMAP.md` with a recommendation, and Ivan approved all ten recommendations as written.

## Decision

- **D13, existing files.** On the VM's first boot, the project folder's files (everything except `.cave/`) are copied into the VM once. In sealed mode the Mac files are left untouched but are no longer updated, and `cave` says so.
- **D14, synced mirror.** The Mac mirror in synced mode is the project folder itself.
- **D15, setup on first use.** `cave run`, `cave shell`, `cave exec` and `cave agent` run `cave init` with its defaults when there is no `cave.toml`, and say so. They refuse to do this in the home folder itself. This keeps the quickstart at two commands.
- **D16, project lookup.** `cave` looks for `cave.toml` in the current folder and its parents, up to the home folder, the way git finds a repository.
- **D17, `cave ls`.** The daemon keeps a list of project folders it has run, used only for listing and pruned when a folder is gone. The folder is still the VM's identity.
- **D18, forks.** In `cave fork [name] [--from <snapshot>]`, `name` is the new fork. The source is a fresh snapshot of the current state, or the snapshot given with `--from`. Commands that act on a VM take `--fork <name>`. The SDK follows the CLI.
- **D19, `cave export`.** Copies the project's files out of the VM, or out of a snapshot, into a folder on the Mac. Exporting and importing a whole VM waits until after v1.
- **D20, `cave stop`.** A new command that suspends the VM to a snapshot now.
- **D21, TTL.** Off by default. When a TTL is set and runs out, the VM is stopped and removed as `cave rm --yes` would, and the startup policy says when that will happen.
- **D22, digest pin.** On first run, `cave` writes the base-image digest into `cave.toml` as `image = "base@sha256:..."`, so the pin is committed with the project.

## Consequences

- The design's verb set gains `cave stop`.
- `docs/cli.md` and `docs/config.md` describe these as decided.
- In the same review Ivan approved every other item proposed in `docs/cli.md`, `docs/config.md` and `docs/protocol.md`, with one change: a redacted credential is shown as its name in brackets, so `API_KEY_1: snednao_#m34nr` streams as `API_KEY_1: [API_KEY_1]`.
- `ROADMAP.md` has no open decisions.
