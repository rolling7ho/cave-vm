# 0005: Phase 2 spike report and gate 4 targets

Date: 2026-10-12 · Decided by: Ivan · Status: accepted

## Context

Phase 2 (ROADMAP P2.1 to P2.10) built enough of the VMM to boot Linux through Hypervisor.framework, then measured the things the design leaves to the spike. This record answers the six spike questions with measurements and sets the gate 4 targets, which Ivan approved as proposed.

**Machine and method.** Apple M4 Pro (12 cores), 24 GiB, macOS 27.0.1 (26A434). Ivan chose not to rerun on a clean macOS 26 machine. Instead, everything used is available on macOS 26: Hypervisor.framework VM, vCPU and memory calls (macOS 11 and later), the `hv_gic_*` calls (macOS 15 and later), and `vmnet_start_interface`. All timings are release builds with a warm page cache, measured from process exec with Python's `perf_counter`. The guest is the P2.7 kernel (Linux 6.18.56, `allnoconfig` plus a fragment) and a busybox initramfs, with **1 vCPU**. The details and raw numbers are in `docs/spike/`.

## Answers

1. **Ad hoc signing (A5): holds.** `codesign -s -` with `com.apple.security.hypervisor` creates VMs with no paid account. It still works after copying into a `bin` folder, after Homebrew-style re-signing and with hardened runtime. A copy carrying a browser quarantine flag is killed at launch. Source: `docs/spike/p2.8-signing.md`.
2. **vmnet (A6): holds.** A normal user gets `VMNET_FAILURE`, and an ad hoc binary with `com.apple.vm.networking` is killed at launch. The userspace NAT stays the default. Same source.
3. **Interrupt-controller state (A1): holds.** The 126,314-byte blob from `hv_gic_state_get_data`, plus nine `ICC_*` registers per vCPU, restores in a new process. Restore order matters: create the vCPUs, set `MPIDR_EL1`, then write the GIC state. Source: `docs/spike/p2.9-snapshot.md`.
4. **vCPU state (A2): holds.** General, SIMD and 37 EL1/EL0 system registers, plus the timer mask and offset, give an exact restore. Saving the guest's counter, not the raw offset, keeps guest time continuous. Same source.
5. **Copy-on-write memory (A3): holds.** A `MAP_PRIVATE` mapping of the memory file restores a 2 GiB guest in 5.8 ms (wall, to the first command's output). Two or ten forks of one snapshot run at once and diverge independently. `clonefile` of the snapshot takes 2 ms. Source: `docs/spike/p2.10-fork.md`.
6. **Baseline numbers:** in the table below. These are for 1 vCPU, not the 2 CPU default, because secondary CPU start (PSCI `CPU_ON`) is not built yet.

### Baseline

| Measure | 512 MiB | 2 GiB |
|---|---|---|
| Boot to shell, wall clock (spike kernel and initramfs) | median 38.5 ms | median 43.4 ms |
| Save a snapshot (state 2.5 ms; the rest is writing the sparse memory file) | 158 ms | 619 ms |
| Memory file on disk (sparse) | 25 MiB | 61 MiB |
| Restore to the first command's output, wall clock | median 6.7 ms | median 5.8 ms |
| Ten forks of one snapshot at once, all ran a command | 33.8 ms | 35.0 ms |
| `clonefile` of the snapshot | 2.2 ms | 2.0 ms |
| Idle memory, booted VM (`phys_footprint`) | 35 MB | 66 MB |
| Idle memory, restored VM (`phys_footprint`, file pages shared) | 4.8 MB | 4.9 MB |
| Idle CPU (shell prompt) | about 1% of one core | about 1% of one core |

### Other findings

- Once a VM has a GIC, Hypervisor.framework handles guest WFI inside `hv_vcpu_run`. A device thread with work has to kick the vCPU with `hv_vcpus_exit`. P2.6 does this for console input, and phase 3's virtio devices need the same.
- Saving is the slow part, at about 300 ms per GiB, because it scans every page for zeros. Restoring and forking copy nothing.
- Whether a GIC state blob loads on a different macOS release is untested. The design's fallback (boot from disk when a memory snapshot cannot load) covers that case.
- Gate 2 is met. A kernel boots to a shell, signing is confirmed, and a snapshot of a running guest restores in a new process and keeps running.

## Decision

Gate 4 targets, measured on an M-series Mac with the default 2 CPU / 2 GiB VM, the real base image and the guest agent, end to end through `cave` and the daemon:

| Gate 4 measure | Target | Spike baseline (1 vCPU, VMM only) |
|---|---|---|
| `cave snapshot` of a running VM | under 500 ms | 619 ms |
| `cave restore`, until `cave exec` can run a command | under 100 ms | 5.8 ms |
| `cave fork`, until the fork can run a command | under 150 ms | 2.0 ms clone + 5.8 ms restore |
| Ten forks of one snapshot, all running | under 1 s | 35 ms |
| Idle memory per running VM (`phys_footprint`) | under 128 MB booted, under 32 MB for a fork | 66 MB booted, 4.9 MB fork |

These leave room for the daemon, the agent handshake, virtio devices, disk cloning and 2 vCPUs, which the spike does not have. Only the snapshot target needs real work: dirty-page tracking, or keeping RAM in its file from the start, so a save writes only what changed.

## Consequences

- Phase 4 is checked against these targets at gate 4. A miss is reported, and the targets change only by a new decision record.
- The 2 CPU numbers get measured once phase 3 adds secondary CPU start. If they differ much from the 1 vCPU numbers, this record gets an addendum.
- A1 to A3, A5, A6 and A11 are answered as above.

## Addendum: 2 vCPUs (phase 3)

Date: 2026-10-12. Phase 3 added secondary vCPU start (PSCI `CPU_ON`, each vCPU on its own thread), so the numbers were measured again with the default 2 vCPUs. They match 1 vCPU closely, so **the gate 4 targets stand unchanged**.

Same machine and guest as above, 2 GiB, release build, warm cache, wall clock from exec. Both columns come from one run of `scripts/spike/vcpu-bench.py`:

| Measure | 1 vCPU | 2 vCPUs |
|---|---|---|
| Boot to shell | median 41.5 ms | median 43.1 ms |
| Save a snapshot | median 620 ms | median 621 ms |
| Memory file on disk | 61 MiB | 62 MiB |
| Restore to the first command's output | median 6.0 ms | median 5.5 ms |
| Ten forks of one snapshot at once, all ran a command | 16.0 ms | 15.6 ms |
| Idle memory, booted VM (`footprint`) | 14.0 MB | 14.0 MB |
| Idle memory, restored VM (`footprint`) | 3.2 MB | 3.3 MB |
| Idle CPU, restored VM | 1.5% of one core | 2.0% of one core |

Memory is measured with the `footprint` tool here, so the booted figure is lower than the 66 MB in the baseline table, which used a different measure. Compare the two columns with each other, not with the baseline. The same goes for the ten-fork time, which no longer includes powering the forks off.

**Correction to answer 3 (A1).** The GIC state blob does not carry each redistributor's SGI and PPI registers. Without them, a restored guest had every SGI and PPI disabled, so its timer interrupt never arrived. It ran a command typed right after restore, which is all P2.9 tested, then hung at 100% CPU once it went idle. Snapshots now also save group, enable, pending, active, priority and trigger state for each vCPU's redistributor, and write it back after the GIC state. Restored guests now idle at about 1 to 2% CPU and keep answering. A1 holds with that addition.
