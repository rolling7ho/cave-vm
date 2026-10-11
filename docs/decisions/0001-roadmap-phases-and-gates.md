# 0001: Roadmap phases and gates

Date: 2026-10-11 · Decided by: Ivan · Status: accepted

## Context

The design's roadmap was an embedded diagram ("6 phases, 5 gates") that did not survive the export to `docs/design.md`. The text defined only gates 2 and 4, and several decided features (synced mode, remote editing, the Rust SDK, the VMM sandbox profile, the Ubuntu image) had no phase.

## Decision

- The diagram is redrawn as text in `docs/design.md` under "Milestones and planning phases".
- Gate N closes phase N for phases 1 to 5. Phase 6 ends at the v1 release.
- Gates 1, 3 and 5 and the release criteria are the ones written in that section.
- Phase placement: VMM sandbox profile in phase 3; Rust SDK (before the MCP server), remote editing and synced mode in phase 5; Python and TypeScript SDKs and the Ubuntu image in phase 6.

## Consequences

`ROADMAP.md` follows this phase plan. Changing a gate or moving work between phases needs a new decision record.
