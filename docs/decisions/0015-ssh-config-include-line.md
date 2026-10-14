# 0015: cave adds an Include line to ~/.ssh/config (D34)

Date: 2026-10-14 · Decided by: Ivan · Status: accepted

## Context

Remote editing (P5.9) needs an SSH host entry, such as `my-project.cave`, that VS Code Remote-SSH and Zed can find. Both read `~/.ssh/config`. The design does not say whether cave may change that file or only print the entry for the user to paste.

## Decision

`cave init` keeps its own host entries in files under cave's data folder and adds one line, `Include "<data>/ssh/hosts/*.conf"`, at the top of `~/.ssh/config`, once. It says so the first time it adds the line. Nothing else in `~/.ssh/config` is changed.

## Consequences

- Editors find `<project>.cave` with no copying by hand.
- Removing the line turns the entries off; cave's own files stay in its data folder.
