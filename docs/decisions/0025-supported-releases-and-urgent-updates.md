# 0025: The 3 newest releases are supported, and older copies must update, D52

Date: 2026-10-15 · Decided by: Ivan (choices), with defaults picked while building (marked) · Status: accepted; `cave export` renamed `cave pull` by [decision 0029](0029-cave-pull-and-cave-push.md) (D58)

## Context

`SECURITY.md` still said cave had no stable release, so it named no supported versions. Since D47, cave asks before updating, and a no skips that version for a day, so a copy could fall far behind.

## Decision

- **D52.** The three newest stable releases get security fixes (Ivan). A copy two or more stable releases behind asks urgently to update (Ivan) and, where it can, does not run until it is updated (Ivan).
- **Urgent asking.** In a terminal, such a copy asks every time, not once a day, and says only the 3 newest releases get security fixes. A no, an empty answer from Ctrl-D, or a failed update stops the command with exit 1 and the update command as the fix (*default*).
- **Where it cannot block** it warns on stderr and runs: without a terminal, since scripts and agents cannot answer (*default*), and for a copy cave cannot update itself, such as a build folder or `cargo install` (*default*). `cave mcp` and `cave ssh-proxy` stay silent.
- `cave export` (now `cave pull`, D58) is asked but never stopped, so the work can always be copied out of the VM (Ivan).
- `[updates] check = false` still turns all of it off for that project (*default*).
- **Counting releases.** `scripts/release.sh` writes `releases`, every stable release from the repository's tags newest first, into each release's `images.json`. cave counts the entries newer than itself. A manifest without the list, as before this change, counts as one release behind at most (*default*).

## Consequences

- The first release after this one is the first whose manifest can make a copy urgent.
- A copy declining the urgent update cannot run cave commands in a terminal; `check = false` in the project is the escape hatch.
