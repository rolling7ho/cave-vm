# 0014: `.env` is not copied into the VM (D33)

Date: 2026-10-14 · Decided by: Ivan · Status: accepted

## Context

D13 copies the project folder's files, everything except `.cave/`, into the VM once on its first boot. Credentials (P5.3) are read from the project's `.env` on the Mac and injected into one command at a time, and the design says a value is never returned to the agent. If the first-boot copy included `.env`, an agent could read `/workspace/.env` and get every credential, and gate 5's "an injected credential never reaches the agent" would fail.

## Decision

The first-boot copy leaves out `.env` as well as `.cave/`. Only the file named exactly `.env` in the project folder is left out. Other files such as `.env.local` or `.env.example` are copied.

## Consequences

- Credentials stay on the Mac and reach the VM only through injection.
- A project that wants its `.env` inside the VM copies it in by hand with `cave cp`, and accepts that the agent can read it.
- D13's wording in ROADMAP and `docs/cli.md` gains the exception.
