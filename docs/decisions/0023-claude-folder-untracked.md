# 0023: The repository's .claude folder is not tracked, D50

Date: 2026-10-15 · Decided by: Ivan · Status: accepted

## Context

P1.5 checked in `.claude/settings.json` for this repository, allowing cargo commands and denying force-pushes and destructive deletes for Claude Code (design, "How Claude Code works in this repo", item 5). Ivan asked to stop tracking `.claude`.

## Decision

- **D50.** `.claude/` at the repository root is in `.gitignore` and no longer tracked (Ivan). Each contributor keeps their own Claude Code settings there.

## Consequences

- The design's guardrails item no longer names a checked-in `.claude/settings.json`.
- A checkout that pulls this change loses its tracked `.claude/settings.json`; copy it aside first to keep it (`git show <older commit>:.claude/settings.json`).
- This is about this repository only. `cave init` still writes `.claude/settings.json` into user projects.
