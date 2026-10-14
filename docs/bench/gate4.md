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

Decision 0005 says the targets change only through a new decision record. Ivan chose to fix the misses and keep the targets ([decision 0011](../decisions/0011-gate-4-misses.md), D29).

## After dirty-page tracking, 2026-10-14

Same Mac and build settings, 5 runs. Snapshots now write only the pages changed since the last one (P4.12a).

| Measure | Median | Min | Max | Target | Result |
|---|---|---|---|---|---|
| Boot until a command runs | 415 ms | 377 ms | 444 ms | none | |
| `cave snapshot` of a running VM | 32 ms | 24 ms | 141 ms | under 500 ms | met |
| `cave restore` until `cave exec` runs a command | 247 ms | 148 ms | 253 ms | under 100 ms | missed |
| The same with `[snapshots] auto = false` | 198 ms | 195 ms | 202 ms | | |
| `cave fork --from` until the fork runs a command | 154 ms | 144 ms | 194 ms | under 150 ms | missed |
| Ten forks of one snapshot, all ran a command | 481 ms | 476 ms | 487 ms | under 1 s | met |
| Idle memory, booted VM | 285 MB | | | under 128 MB | missed |
| Idle memory, fork | 27 MB | | | under 32 MB | met |

The undo point no longer dominates a restore: what is left is stopping the old VMM, starting the new one and the agent handshake. Booted memory is unchanged, as expected; it needs freed guest pages handed back.

Keeping guest RAM in a shared file was tried first and dropped: about one restore in 40 hung (decision 0011). A separate, older hang also shows up under load. When the guest rewrites 200 MB just before each snapshot, about one fork in 60 never reaches its agent and falls back to a disk boot after 50 seconds. This happens on main too (2 of 120), so it is not caused by the tracking. It is fixed: a snapshot taken while a vCPU was inside an interrupt handler restored a vCPU that took no interrupt again, and snapshots now wait until no vCPU is in one.

## After faster restore and fork, 2026-10-14

Same Mac and build settings, 5 runs (P4.12b).

| Measure | Median | Min | Max | Target | Result |
|---|---|---|---|---|---|
| Boot until a command runs | 466 ms | 450 ms | 477 ms | none | |
| `cave snapshot` of a running VM | 38 ms | 26 ms | 97 ms | under 500 ms | met |
| `cave restore` until `cave exec` runs a command | 81 ms | 68 ms | 83 ms | under 100 ms | met |
| The same with `[snapshots] auto = false` | 47 ms | 47 ms | 48 ms | | |
| `cave fork --from` until the fork runs a command | 130 ms | 122 ms | 133 ms | under 150 ms | met |
| Ten forks of one snapshot, all ran a command | 476 ms | 450 ms | 502 ms | under 1 s | met |
| Idle memory, booted VM | 282 MB | | | under 128 MB | missed |
| Idle memory, fork | 27 MB | | | under 32 MB | met |

Where the restore time went:

- The daemon polled for the old VMM's exit every 20 ms. It now polls every millisecond.
- Starting a VMM process takes about 17 ms (sandbox-exec, exec, the dynamic loader). The next one now starts while the undo point is saved.
- About 100 ms went to a lost first agent connection. The guest drops connections when it handles the transport reset, and the first connection often arrived just before that. The vsock device now holds packets until the guest has handled the reset.

What remains is the undo point (3,000 to 5,000 changed pages at about 5 µs each), the guest flushing its file systems before it, and the agent handshake. Writing the pages from several threads was no faster.

A fork maps a clone of the snapshot's memory, a new file whose pages are not yet in the Mac's file cache, so the guest's first reads go to the drive. Mapping the snapshot's own memory file instead brought a fork to about 50 ms, but each fork then counted about 80 MB of that file's cached pages in its own memory (130 MB per fork), even after flushing the file and dropping it from the cache. The clone stays.

Only booted memory is still missed. The guest uses about 130 MB after boot but has touched about 280 MB, and freed guest pages are not handed back. That needs free page reporting or a balloon device (P6.1, pulled forward by D29).

## Free page reporting, 2026-10-14

A balloon device now takes the free memory the guest reports and hands it back to macOS (P4.12c). Same Mac, 3 runs each, old and new kernel back to back:

| Measure | Without | With | Target |
|---|---|---|---|
| `cave restore` until `cave exec` | 74 ms | 53 ms | under 100 ms |
| `cave fork --from` until the fork runs a command | 122 ms | 116 ms | under 150 ms |
| Ten forks | 422 ms | 358 ms | under 1 s |
| Idle memory, booted VM | 287 MB | 288 MB | under 128 MB |
| Idle memory, fork | 28 MB | 27 MB | under 32 MB |

What it changes is memory the guest frees later: writing 500 MB to `/dev/shm` and deleting it takes the VM from 765 MB back to about 290 MB within a few seconds. Without it, the VM stays at 765 MB.

It does not change booted memory, because right after boot the guest holds that memory. It has about 50 MB in processes, 52 MB of kernel reservations (mostly the page array for 2 GiB) and over 100 MB of file cache. Dropping the guest's file cache by hand (`echo 3 > /proc/sys/vm/drop_caches`) brings the VM to 165–190 MB. A kernel with 16 KiB pages saves about 15 MB more. Neither reaches 128 MB.

Measured along the way:

- `MADV_FREE_REUSABLE` alone frees nothing while the hypervisor maps the range. Unmapping, discarding and mapping again works.
- Reporting order 2 (16 KiB) slowed restores, because a booted guest soon reports all its free memory, one remap per block. Order 9 (2 MiB) left most freed memory unreported. Order 5 (128 KiB) keeps every time target and frees the most in these tests.

