# 0012: Booted memory target is 200 MB for the Debian guest (D30)

Date: 2026-10-14 · Decided by: Ivan · Status: accepted

## Context

Decision 0005 set idle memory for a booted VM at under 128 MB, measured on the spike's minimal guest at 66 MB. Decision 0011 kept that target for the Debian guest. With every other gate 4 target met, the benchmark still measured about 285 MB ([docs/bench/gate4.md](../bench/gate4.md)).

Free page reporting (P4.12c) hands back memory the guest frees, but right after boot the Debian guest really holds its memory:

- about 50 MB in processes (systemd, journald, udevd, logind, sshd, dbus);
- 52 MB of kernel reservations, mostly the page array for 2 GiB;
- over 100 MB of file cache from booting.

With the cache dropped, the VM measured 165 to 190 MB. A kernel with 16 KiB pages saved about 15 MB more. Neither reached 128 MB.

## Decision

The booted memory target for the Debian guest is **under 200 MB**. Every other target in decision 0005 stands.

To meet it, the guest agent drops the clean page cache once, 3 seconds after it starts, when boot has settled (`cave-agent`'s `memory` module). The Mac already caches the same disk blocks, so later reads stay cheap. The gate 4 benchmark measures idle memory 8 seconds after the VM's last command rather than 2, so the dropped cache has been reported back.

## Consequences

Gate 4 closes when `scripts/bench/gate4.py` meets every target on the self-hosted Mac. Trimming the guest further (fewer services, a smaller journal, 16 KiB pages) is left for phase 6 density work.
