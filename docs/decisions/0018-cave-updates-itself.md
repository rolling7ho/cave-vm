# 0018: cave updates itself, D45

Date: 2026-10-15 · Decided by: Ivan (choices), with defaults picked while building (marked) · Status: accepted; installs without asking replaced by [decision 0020](0020-ask-before-updating.md) (D47)

## Context

The design's update detector only says when a newer base image exists, and "never updates anything by itself". Ivan asked for copies of cave to update themselves. Asked how, he chose automatic installs over a notice with a `cave update` command, and asked for it in 0.1.1, so every copy from 0.1.1 on keeps itself current. Copies on 0.1.0 have no updater and need one manual `brew upgrade`.

## Decision

- **D45, cave updates itself.** cave installs a newer release of itself without being asked (Ivan). Base images are still never updated by themselves: a pinned image stays pinned.
- **One lookup.** Each release's `images.json` gains a top-level `"cave"` key with that release's version. The update detector's daily lookup is unchanged, so cave still makes one plain request with no identifying data; it now also records the newest cave version (*default*). `scripts/release.sh` sets the key. Older copies ignore it, since unknown keys are ignored (D28).
- **When.** Before a command, if the recorded version is newer than the running one, no VM is running, and the project's `[updates] check` is not `false`, `cave` starts the install in the background, says so in one line, and carries on with the command (*default*). A running VM waits, since replacing the daemon would stop it. A failed install is tried again a day later, and its output is kept in `update.log` in the data folder.
- **How.** cave installs the way it was installed (*default*): `brew update` then `brew upgrade rolling7ho/cave/cave-vm` for a Homebrew copy, or the release's `install.sh` into the same folder for a script copy. A copy run from a build folder or from `cargo install` is never touched.
- **The daemon follows.** When `cave` finds a daemon of another version and no VM is running, it stops that daemon and starts its own (*default*). This also fixes manual upgrades, which used to keep the old daemon.
- `[updates] check = false` turns off both the lookup and the install for that project (Ivan).

## Consequences

- The design's "Image updates" and "Telemetry" rows, `docs/config.md` (`[updates]`), `docs/cli.md` and the ROADMAP's decision table say so.
- Installing a new version runs code fetched from the project's GitHub releases without a prompt. It is the same code `brew upgrade` or the install script would fetch, from the same places, checked the same way (the formula's and `install.sh`'s sha256).
- Homebrew copies build from source, so an update takes a few minutes in the background.
