# Snapshot format

A snapshot is a folder in the project's `.cave/snapshots/<name>/`. This page is the on-disk format, version 1. The design (`docs/design.md`, "Snapshots" and "Upgrades") says what snapshots are for; this page says what is in one.

## Files

| File | Written by | What it holds |
|------|-----------|---------------|
| `snapshot.json` | daemon, last | The manifest, below. A folder without one is an unfinished snapshot and is ignored. |
| `disk.img` | daemon | The project disk, as an APFS clone of `.cave/disk.img`. |
| `memory` | VMM | Guest RAM. Missing for a snapshot of a stopped VM. |
| `state` | VMM | vCPU, interrupt-controller, timer, UART and virtio state, and the guest's open TCP connections. Missing with `memory`. |

The virtio state is each transport's registers and its queues with the device's position in each ring. It leaves out what cannot outlive the VMM process: frames waiting for the guest are dropped, and host vsock connections end, which the guest learns from a transport reset (P4.3).

The guest's TCP connections through the network stack end too: their host sockets belong to the old VMM. `state` lists each one (the guest port and the destination), and the restored network stack sends the guest segments it cannot accept on each. The guest answers with an acknowledgment, which the stack, knowing no such connection, answers with a reset. So programs in the guest see "connection reset" at once rather than waiting on a connection nothing is behind, and can connect again. A connection missing from the list, say one opened as the snapshot was taken, is reset the first time the guest sends on it (P4.4).

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
