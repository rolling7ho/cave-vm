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

## Forks

A fork lives in `.cave/forks/<name>/` (D18). It is made as a snapshot folder: a fresh snapshot of the VM written straight there, or APFS clones of the files of the snapshot given with `--from`, with the manifest renamed to the fork. The fork boots from that memory with `disk.img` as its own disk. Once it has booted, `memory` and `state` are deleted and the manifest says `has_memory: false`, since the disk moves on from them; a fork that is stopped is suspended like any VM (below).

## Suspensions

`cave stop` suspends a VM (D20): the daemon saves it as a snapshot folder named `suspended`, in `.cave/suspended/` for the project's VM and `.cave/forks/<name>/suspended/` for a fork, then stops the VMM. `cave ls` shows the VM as suspended. The next start resumes from that folder with processes intact and deletes it, so a suspension is used once. It is not a snapshot: `cave restore` cannot name it, and a restore deletes it.

The daemon also suspends a VM left unused for its `idle_suspend` time ([config](config.md#vm)), and every running VM, all at once, when the Mac is about to sleep. macOS waits up to 30 seconds for that before it sleeps; a VM that is not saved by then keeps running and sleeps with the Mac. A suspension is a folder on disk, so it outlives a reboot, and the next command after the reboot resumes from it. A VM that was running when the Mac restarted without sleeping first has no suspension and boots from its disk.

To check sleep by hand: start a VM with a process in it (`cave exec -- sh -c 'setsid sleep 100000 &'`), run `pmset sleepnow`, wake the Mac, and see `cave ls` show it suspended and `cave exec -- pgrep sleep` find the process.

A start that changes the network between `off` and `nat` or `lan` cannot resume, because the VM would need another set of devices, so it boots from the disk and the processes are lost. A snapshot of a suspended VM copies the suspension, memory included.

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
  "has_memory": true,
  "auto": false
}
```

- `format`: the version of this page. A `cave` refuses a newer one and says to update.
- `host_os`: the macOS build (`sysctl kern.osversion`) the snapshot was taken on.
- `image`: the pinned image the disk started from.
- `credentials`: names of credentials injected into the VM before the snapshot, never their values. A fork or restore shows them so you know what the VM may still hold.
- `has_memory`: whether `memory` and `state` exist.
- `auto`: taken automatically, such as the one a restore takes first so it can be undone. A restore with no name passes over these. Missing means `false`.

Readers ignore keys they do not know, so a later `cave` can add keys without a new version.

## Restoring across upgrades

Memory state is loaded only when `has_memory` is true and `host_os` matches the running Mac, since interrupt-controller state may change between macOS builds. Otherwise restore boots from `disk.img`, keeping the files and losing the running processes, and says why.
