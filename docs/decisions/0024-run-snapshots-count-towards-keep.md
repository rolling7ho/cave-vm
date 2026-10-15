# 0024: Snapshots before agent runs and pushes count towards keep, D51

Date: 2026-10-15 · Decided by: Ivan (choice), with defaults picked while building (marked) · Status: accepted

## Context

The design takes a snapshot before each `cave agent` run and each `cave push`. Both were saved like named snapshots, so `[snapshots] keep` never pruned them and they piled up on disk. Only the undo point a restore saves first was automatic. Ivan chose to count them towards `keep`.

## Decision

- **D51.** The snapshots before an agent run and before a push are automatic snapshots (Ivan). They show as `auto` in `cave ls`, count towards `keep`, and are deleted oldest first after each new one, after each restore and by `cave gc`. The one just taken is never deleted.
- They are still always taken, even with `auto = false`, because the design requires them (*default*).
- **A restore with no name** passes over only the undo points restores save, not every automatic snapshot (*default*). So `cave restore` after an agent run still goes back to the snapshot taken before it. Snapshot manifests gain an `undo` flag. A manifest without one counts as an undo point when it is automatic, since those were the only automatic snapshots before.
- The daemon's `snapshot` request gains `before`, which `cave agent` sets. The SDKs and MCP server keep taking named snapshots.

## Consequences

- Undoing an agent run by name works only until `keep` newer automatic snapshots have been taken, 10 by default.
- Snapshots taken before this change stay as they are: named ones are never pruned.
