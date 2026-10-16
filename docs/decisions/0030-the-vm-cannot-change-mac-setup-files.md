# 0030: The VM cannot change the project's setup files on the Mac, D59

Date: 2026-10-16 · Decided by: Ivan (choice), with defaults picked while building (marked) · Status: accepted

## Context

In synced mode the VM is authoritative, and `cave sync` writes every file the VM changed to the Mac mirror. That included the files that set up the project on the Mac: `cave.toml`, which sets the VM's network and ports on the next start, `.claude/settings.json`, which holds the deny rules that keep plain `claude` and `cave agent --on-mac` off the Mac's shell, `.mcp.json`, and `.gitignore`, which decides what syncs. So an agent in the VM could widen its own policy or loosen the Mac agent's rules through the mirror. The release review for 1.0.0-rc.3 also found that the sync compared `.git`, `.cave` and `.env` by exact case, while the Mac's file system ignores case, and that it followed symbolic links in the project folder.

## Decision

- **D59.** A change the VM makes to `cave.toml`, `.mcp.json` or `.gitignore` at the project's top, or to anything in the top-level `.claude/` folder, is not written to the Mac. `cave sync` lists those files in its reply (`held`) and says to look at the change with `cave exec cat` and make it on the Mac by hand (Ivan).
- **Which files** (*default*): the four above, since each changes what runs or what is allowed on the Mac. `CLAUDE.md` and `AGENTS.md` still sync: they are project content an agent edits on purpose, and they carry instructions, not permissions.
- **Case** (*default*): the sync compares the fixed names (`.git`, `.cave`, `.env`, `node_modules`, `target` and the held files) without case, as APFS does, and folds `ſ` and the Kelvin sign to `s` and `k`.
- **Paths** (*default*): paths from the VM with `.` segments or control characters are dropped, and the daemon refuses to write or delete through a symbolic link in the project folder, with an error that says to replace the link.
- `cave push` is unchanged: edits made on the Mac still reach the VM.

## Consequences

- An agent in the VM that edits `cave.toml` or `.claude/` for a good reason sees the change listed on every sync until you make it on the Mac, or undo it in the VM.
- The threat model's mirror row says what the VM can and cannot write.
