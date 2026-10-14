#!/usr/bin/env python3
"""Gate 4 benchmark: the measures decision 0005 set targets for, end to end
through `cave` and the daemon (ROADMAP P4.11).

    cargo build --release --workspace
    scripts/bench/gate4.py [--runs 5] [--bin target/release] [--image-dir target/image]

Runs the real `cave`, `cave-daemon` and `cave-vmm` from --bin against the
local image in --image-dir (image/debian/build.sh and image/manifest.sh),
in a scratch data directory, with the default 2 CPU / 2 GiB VM. Nothing
reaches the network. cave-vmm is signed ad hoc with the hypervisor
entitlement, as scripts/hypervisor-runner.sh does. Every time is wall
clock around the `cave` commands; memory is `phys_footprint` from the
footprint tool, as in decision 0005, once the VM is idle: IDLE_WAIT after
its last command, which covers the agent dropping the boot-time file
cache and the guest reporting the freed pages (decision 0012). Standard
library only.
"""

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# How long after its last command a VM counts as idle for its memory.
IDLE_WAIT = 8

# Decision 0005, with booted memory from decision 0012.
TARGETS = {
    "snapshot": 500,
    "restore": 100,
    "fork": 150,
    "ten_forks": 1000,
    "booted_mb": 200,
    "fork_mb": 32,
}


class Rig:
    def __init__(self, bin_dir, image_dir):
        # Short, so the daemon socket path fits in sun_path.
        self.root = tempfile.mkdtemp(prefix="cvb", dir="/tmp")
        self.bin = os.path.join(self.root, "bin")
        self.project = os.path.join(self.root, "home", "proj")
        for d in (self.bin, self.project, os.path.join(self.root, "data")):
            os.makedirs(d, exist_ok=True)
        for name in ("cave", "cave-daemon", "cave-vmm"):
            src = os.path.join(bin_dir, name)
            if not os.path.isfile(src):
                sys.exit(f"{src} is missing; run `cargo build --release --workspace` first")
            shutil.copy2(src, self.bin)
        subprocess.run(
            ["/usr/bin/codesign", "--sign", "-", "--force", "--entitlements",
             os.path.join(REPO, "scripts", "hypervisor.entitlements"),
             os.path.join(self.bin, "cave-vmm")],
            check=True, capture_output=True,
        )
        self.env = dict(
            os.environ,
            HOME=os.path.join(self.root, "home"),
            CAVE_HOME=os.path.join(self.root, "data"),
            CAVE_IMAGE_DIR=os.path.abspath(image_dir),
        )

    def cave(self, *args, check=True):
        out = subprocess.run(
            [os.path.join(self.bin, "cave"), *args], cwd=self.project, env=self.env,
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
        )
        if check and out.returncode != 0:
            raise RuntimeError(f"cave {' '.join(args)} failed: {out.stderr.strip()}")
        return out

    def timed(self, *commands):
        """Runs `cave` commands one after another; returns milliseconds."""
        start = time.perf_counter()
        for args in commands:
            self.cave(*args)
        return (time.perf_counter() - start) * 1000

    def vmm_pids(self):
        vms = os.path.join(self.root, "data", "vms")
        pids = set()
        for d in os.listdir(vms) if os.path.isdir(vms) else []:
            try:
                with open(os.path.join(vms, d, "vmm.pid")) as f:
                    pids.add(int(f.read().strip()))
            except (OSError, ValueError):
                pass
        return pids

    def close(self, keep):
        self.cave("stop", check=False)
        subprocess.run(["pkill", "-f", os.path.join(self.bin, "cave-daemon")])
        if keep:
            print(f"kept {self.root}")
        else:
            shutil.rmtree(self.root, ignore_errors=True)


def footprint_mb(pid):
    """phys_footprint of a process in MB, from the footprint tool."""
    out = subprocess.run(["footprint", str(pid)], capture_output=True, text=True).stdout
    m = re.search(r"phys_footprint:\s+([0-9.]+)\s+(KB|MB|GB)", out)
    if not m:
        return None
    scale = {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(2)]
    return float(m.group(1)) * scale


def ten_forks(rig, snapshot):
    """Forks `snapshot` ten times at once; returns milliseconds until every
    fork has run a command."""
    errors = []

    def one(i):
        try:
            rig.cave("fork", f"many-{i}", "--from", snapshot)
            rig.cave("exec", "--fork", f"many-{i}", "--", "true")
        except RuntimeError as e:
            errors.append(e)

    threads = [threading.Thread(target=one, args=(i,)) for i in range(10)]
    start = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    took = (time.perf_counter() - start) * 1000
    for i in range(10):
        rig.cave("rm", "--yes", f"many-{i}", check=False)
    if errors:
        raise errors[0]
    return took


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--bin", default=os.path.join(REPO, "target", "release"))
    p.add_argument("--image-dir", default=os.path.join(REPO, "target", "image"))
    p.add_argument("--keep", action="store_true", help="keep the scratch folder")
    args = p.parse_args()

    rig = Rig(args.bin, args.image_dir)
    results = {}
    try:
        rig.cave("init", "--sealed")
        # The first boot fetches the image into the cache; not counted.
        rig.cave("exec", "--", "true")
        boots = []
        for _ in range(args.runs):
            rig.cave("rm", "--yes")
            boots.append(rig.timed(("exec", "--", "true")))
        results["boot"] = boots
        # A running process, as an agent's session would have.
        rig.cave("exec", "--", "sh", "-c", "setsid sleep 100000 >/dev/null 2>&1 &")
        time.sleep(IDLE_WAIT)
        (main_pid,) = rig.vmm_pids()
        results["booted_mb"] = [footprint_mb(main_pid)]

        results["snapshot"] = [rig.timed(("snapshot", f"s{i}")) for i in range(args.runs)]
        results["restore"] = [
            rig.timed(("restore", "s0"), ("exec", "--", "true")) for _ in range(args.runs)
        ]
        # The same without the undo point a restore saves first, which is
        # a snapshot in itself.
        toml = os.path.join(rig.project, "cave.toml")
        with open(toml) as f:
            config = f.read()
        with open(toml, "w") as f:
            f.write(config + "\n[snapshots]\nauto = false\n")
        results["restore_no_undo"] = [
            rig.timed(("restore", "s0"), ("exec", "--", "true")) for _ in range(args.runs)
        ]
        with open(toml, "w") as f:
            f.write(config)
        forks = []
        running = rig.vmm_pids()
        for i in range(args.runs):
            forks.append(rig.timed(
                ("fork", f"f{i}", "--from", "s0"), ("exec", "--fork", f"f{i}", "--", "true")))
            if i == 0:
                time.sleep(IDLE_WAIT)
                (fork_pid,) = rig.vmm_pids() - running
                results["fork_mb"] = [footprint_mb(fork_pid)]
            rig.cave("rm", "--yes", f"f{i}")
        results["fork"] = forks
        results["ten_forks"] = [ten_forks(rig, "s0") for _ in range(max(1, args.runs // 2))]
    finally:
        rig.close(args.keep)

    print(f"Gate 4 benchmark: {args.runs} runs, 2 CPUs, 2 GiB, release build")
    print(f"{'measure':<34}{'median':>10}{'min':>10}{'max':>10}{'target':>10}  result")
    names = {
        "boot": "boot until a command runs (ms)",
        "snapshot": "cave snapshot, running VM (ms)",
        "restore": "cave restore until exec (ms)",
        "restore_no_undo": "  the same, with auto = false (ms)",
        "fork": "cave fork until exec (ms)",
        "ten_forks": "ten forks, all ran a command (ms)",
        "booted_mb": "idle memory, booted VM (MB)",
        "fork_mb": "idle memory, fork (MB)",
    }
    misses = 0
    for key, label in names.items():
        values = [v for v in results.get(key, []) if v is not None]
        if not values:
            print(f"{label:<34}{'-':>10}")
            continue
        median = statistics.median(values)
        target = TARGETS.get(key)
        verdict = "" if target is None else ("met" if median < target else "MISSED")
        misses += verdict == "MISSED"
        print(f"{label:<34}{median:>10.1f}{min(values):>10.1f}{max(values):>10.1f}"
              f"{'' if target is None else target:>10}  {verdict}")
    return 1 if misses else 0


if __name__ == "__main__":
    sys.exit(main())
