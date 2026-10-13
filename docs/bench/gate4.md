# Gate 4 benchmark

`scripts/bench/gate4.py` measures what [decision 0005](../decisions/0005-spike-report.md) set gate 4 targets for. It runs end to end through `cave` and the daemon, with the default 2 CPU / 2 GiB VM, the real Debian image and the guest agent (ROADMAP P4.11). Run it after `cargo build --release --workspace`. It exits 1 when a target is missed.

## Results, 2026-10-14

Apple M4 Pro, 24 GiB, macOS 27.0.1 (26A434), release build, warm cache, 7 runs (3 for ten forks). Times are wall clock around the `cave` commands. Memory is `phys_footprint` from `footprint`, as in decision 0005. A process was left running in the VM, as an agent session would leave one.

| Measure | Median | Min | Max | Target | Result |
|---|---|---|---|---|---|
| Boot until a command runs | 377 ms | 359 ms | 441 ms | none | |
| `cave snapshot` of a running VM | 595 ms | 587 ms | 877 ms | under 500 ms | missed |
| `cave restore` until `cave exec` runs a command | 922 ms | 803 ms | 948 ms | under 100 ms | missed |
| The same with `[snapshots] auto = false` | 214 ms | 111 ms | 225 ms | | |
| `cave fork --from` until the fork runs a command | 162 ms | 134 ms | 224 ms | under 150 ms | missed |
| Ten forks of one snapshot, all ran a command | 508 ms | 497 ms | 525 ms | under 1 s | met |
| Idle memory, booted VM | 275 MB | | | under 128 MB | missed |
| Idle memory, fork | 35 MB | | | under 32 MB | missed |

## Why each miss happens

- **Snapshot.** Saving scans and writes all 2 GiB of guest memory, at about 300 ms per GiB, as the spike found. Decision 0005 already named the fix: keep guest RAM in its file from the start, or track dirty pages, so a save writes only what changed.
- **Restore.** Most of the time goes to the undo point that `docs/cli.md` has a restore save first, which is a full snapshot. Without it a restore takes 214 ms: stopping the old VMM, starting a new one from the snapshot, and the agent handshake. The snapshot fix above shrinks the undo point too.
- **Fork.** 162 ms against 150 ms. A fork from a snapshot clones its files, starts a VMM from them and waits for the agent. The first command after a restore sometimes waits on the guest's vsock reset (see the CHANGELOG entry on agent retries).
- **Booted memory.** The spike booted a busybox initramfs. Debian with systemd, the agent and Docker's prerequisites touches about 275 MB of guest RAM, and every touched page counts. Free page reporting or a balloon would hand freed pages back. Otherwise the target needs a new baseline for a Debian guest.
- **Fork memory.** 35 MB against 32 MB. A fork's guest RAM is the snapshot file mapped copy-on-write, so this is what the restored guest has written since.

Decision 0005 says the targets change only through a new decision record. The choice is in ROADMAP, "Needs Ivan's decision".
