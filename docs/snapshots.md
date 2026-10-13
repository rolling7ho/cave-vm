# Snapshot format

A snapshot is a folder in the project's `.cave/snapshots/<name>/`. This page is the on-disk format, version 1. The design (`docs/design.md`, "Snapshots" and "Upgrades") says what snapshots are for; this page says what is in one.

## Files

| File | Written by | What it holds |
|------|-----------|---------------|
| `snapshot.json` | daemon, last | The manifest, below. A folder without one is an unfinished snapshot and is ignored. |
| `disk.img` | daemon | The project disk, as an APFS clone of `.cave/disk.img`. |
| `memory` | VMM | Guest RAM. Missing for a snapshot of a stopped VM. |
| `state` | VMM | vCPU, interrupt-controller, timer and device state. Missing with `memory`. |

The manifest is written to a temporary file and renamed into place, so a crash leaves either no manifest or a whole one.

## Manifest

```json
{
  "format": 1,
  "name": "app-2026-10-13-debian",
  "created_unix": 1790000000,
  "cave_version": "0.1.0",
  "host_os": "26A434",
  "image": "debian@sha256:...",
  "vm": { "cpus": 2, "memory_bytes": 2147483648, "net": "nat" },
  "credentials": ["API_KEY"],
  "has_memory": true
}
```

- `format`: the version of this page. A `cave` refuses a newer one and says to update.
- `host_os`: the macOS build (`sysctl kern.osversion`) the snapshot was taken on.
- `image`: the pinned image the disk started from.
- `credentials`: names of credentials injected into the VM before the snapshot, never their values. A fork or restore shows them so you know what the VM may still hold.
- `has_memory`: whether `memory` and `state` exist.

Readers ignore keys they do not know, so a later `cave` can add keys without a new version.

## Restoring across upgrades

Memory state is loaded only when `has_memory` is true and `host_os` matches the running Mac, since interrupt-controller state may change between macOS builds. Otherwise restore boots from `disk.img`, keeping the files and losing the running processes, and says why.
