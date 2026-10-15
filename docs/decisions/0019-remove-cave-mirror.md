# 0019: Remove cave mirror, D46

Date: 2026-10-15 · Decided by: Ivan · Status: accepted

## Context

D39 (decision 0017) added `cave mirror`, which replayed the agent's terminal from a recording `cave agent` made. Trying it in 0.1.1, Ivan found it only shows the screen of Claude Code or whichever agent is running, which the agent's own terminal already shows.

## Decision

- **D46.** `cave mirror` is removed, and `cave agent` no longer records the agent's terminal (Ivan). `cave view`, the other half of D39, stays.
- `cave agent` deletes a recording 0.1.1 left in `.cave/agent.term` (or `.cave/forks/<name>/agent.term`), since it holds the agent's screen output (*default*).

## Consequences

- `docs/cli.md` loses the `cave mirror` section; the ROADMAP's D39 row points here.
