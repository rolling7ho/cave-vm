# 0020: Ask before updating, looked up when cave runs, D47

Date: 2026-10-15 · Decided by: Ivan (choices), with defaults picked while building (marked) · Status: accepted

## Context

D45 (decision 0018) had cave learn about a new release from the image update detector's daily lookup, then install it in the background without asking. Ivan asked to change this: cave should look when it is opened, not once a day, and ask whether to update instead of updating by itself.

## Decision

- **D47.** When cave runs in a terminal, it looks up the newest stable release, and when that is newer than the running cave it asks `Update now? [Y/n]` before the command runs (Ivan). Yes installs it in the foreground, the way cave was installed (Homebrew or the install script, as in D45), then cave stops and asks for the command to be run again on the new version. No skips it.
- **The lookup** is the same plain GET of the latest release's `images.json` as the image detector's. It runs in the background so no command waits for the network, and what it finds is offered the next time cave runs (*default*). Lookups are at least 10 minutes apart, so a run of cave commands looks once (*default*).
- **Declining** a version stops cave asking about it for a day (*default*).
- **Never in scripts.** Without a terminal on standard input and standard error, cave neither looks up nor asks, and `cave mcp` and `cave ssh-proxy` never do (*default*).
- `[updates] check = false` turns off the lookup and the question for that project, as before. Images still never update by themselves, and the daemon still replaces itself once no VM runs (D45).

## Consequences

- D45's background install is gone. The design's "cave updates" and "Telemetry" rows, `docs/config.md` (`[updates]`), `docs/cli.md` and the ROADMAP say so.
- A cave run in a terminal makes a request at most every 10 minutes, where before only a VM start did, once a day.
