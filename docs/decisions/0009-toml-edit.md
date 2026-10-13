# 0009: Reading and writing `cave.toml` (D27)

Date: 2026-10-13 · Decided by: Ivan · Status: accepted

## Context

The daemon and the CLI both read `cave.toml` (`docs/config.md`), and on first run the daemon writes the base-image digest pin back into it (D22). The file is the user's: it has comments and its own layout. D23 did not include a TOML parser, so one was asked for in the project thread with two options: `toml_edit`, or `toml` for reading plus a separate way to write the pin.

## Decision

Add `toml_edit`. It parses the file into a document that keeps comments, whitespace and key order, so writing the pin changes only the `image` value.

## Consequences

- One crate covers both reading and writing; `toml` is not added.
- It is added in the PR that first reads `cave.toml`, with default features limited to parsing and display, and passes `cargo deny` and `cargo audit` there.
- Writes go through `toml_edit` only; nothing hand-edits the file as text.
