# 0011: Gate 4 misses are fixed, not re-targeted (D29)

Date: 2026-10-14 · Decided by: Ivan · Status: accepted

## Context

The first gate 4 benchmark (P4.11, [docs/bench/gate4.md](../bench/gate4.md)) missed five of the six targets in [decision 0005](0005-spike-report.md): snapshot 595 ms (under 500), restore 922 ms (under 100), fork 162 ms (under 150), booted memory 275 MB (under 128) and fork memory 35 MB (under 32). Decision 0005 says targets change only through a new decision record, so the choice went to Ivan as Q1. The recommendation was to fix snapshot and restore and to re-baseline booted memory for a Debian guest.

## Decision

Fix the misses and keep every target as decision 0005 set it. Gate 4 stays open until the benchmark meets them all on the self-hosted Mac. Phase 5 waits for it.

## How snapshots are made fast

Decision 0005 named two ways to stop a snapshot writing all of guest memory: keep RAM in a file from boot, or track dirty pages.

Keeping RAM in a shared file (`MAP_SHARED`), flushed with `msync` and cloned, was tried first. A snapshot took 25 ms, and data the guest wrote just before one came through intact. But about one restore in 40 never reached the guest agent: 10 of 270 forks, against 0 of 210 for anonymous memory in the same loop. Writing the snapshot page by page from the same shared mapping failed just as often (8 of 150), so the problem is Hypervisor.framework running a guest on shared file-backed memory, not the clone. That approach was dropped.

Guest RAM stays anonymous. The VMM write-protects it in the hypervisor (`hv_vm_protect`) and catches each page's first guest write as a data abort. Host writes through `GuestMemory::write`, which every device uses, mark pages directly. A snapshot clones the previous snapshot's memory file (held open, so deleting that snapshot does not matter) and writes only the dirty pages. A booted VM's first snapshot starts from an all-zero file. The on-disk format does not change.
