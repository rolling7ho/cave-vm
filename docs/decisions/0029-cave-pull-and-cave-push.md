# 0029: `cave pull` copies out, `cave push` copies in, in both modes, D58

Date: 2026-10-16 · Decided by: Ivan (choice), with defaults picked while building (marked) · Status: accepted

## Context

`cave export` copied the project out of the VM, and `cave push` brought Mac edits into the VM, but only in synced mode. In a sealed project the VM got the folder once, at first boot (D13), and later Mac edits had no way in short of `cave cp`. `cave pull` was the hidden old name of `cave push` (D42), due to go in the next minor release. Ivan asked for `cave pull` to replace `cave export` and for `cave push` to copy Mac edits into the VM, refusing when the VM changed the same files unless `--force`, with help text that keeps them apart from git.

## Decision

- **D58.** `cave export` is renamed `cave pull`, and `export` stays as a hidden alias (Ivan). The hidden `pull` alias of `cave push` is dropped in 1.0.0, since `pull` now copies the other way (Ivan, as D42 planned).
- `cave push` works in sealed projects too. It refuses files changed both on the Mac and in the VM, and `--force` applies the Mac's version of those instead, in both modes, after the usual snapshot (Ivan).
- **What counts as changed in a sealed project** (*default*): the daemon records the hashes of the project folder's files right after the first-boot copy, in `.cave/sync.json`, and each push updates them, as synced mode already does. A sealed project set up before this has no record, so `cave push` asks for `--force`, which copies every file that differs.
- **Deletions** (*default*): in a sealed project a file deleted on the Mac is not deleted in the VM, since the Mac's files are not kept up to date there and a cleanup on the Mac should not remove work in the VM. Synced mode keeps deleting, as before.
- **Output** (*default*): `cave pull` says `Copied <n> files (<size>) to <folder>`; the default folder name keeps `-export-`.
- **Help** (*default*): both commands' help says they are not `git pull` or `git push`.

## Consequences

- A script that ran `cave pull` to mean `cave push` now copies the project out instead. The alias was hidden and announced as temporary.
- `cave push` copies Mac files into the VM in sealed mode, never the top-level `.env`, `.git` or `.gitignore`d paths. `docs/threat-model.md` lists it.
