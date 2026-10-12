#!/usr/bin/env python3
"""Spike numbers for 1 and 2 vCPUs (addendum to decision 0005).

    scripts/spike/vcpu-bench.py [--memory 2048] [--runs 10] [--cpus 1,2]

Needs the spike kernel and initramfs in target/spike (image/spike/build.sh)
and a release build of cave-vmm-spike signed by scripts/hypervisor-runner.sh
(running it once through the runner signs it). Every timing is wall clock
from process exec, as in decision 0005. Standard library only.
"""

import argparse
import os
import re
import select
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

BIN = os.environ.get("CAVE_SPIKE_BIN", "target/release/cave-vmm-spike")
KERNEL = "target/spike/Image"
INITRD = "target/spike/initramfs.cpio.gz"
PROMPT = b"~ # "


def wait_for(proc, needle, timeout=10.0):
    """Reads the process's stdout until `needle` appears. Returns the time
    it appeared, or raises."""
    seen = b""
    deadline = time.perf_counter() + timeout
    fd = proc.stdout.fileno()
    while time.perf_counter() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            seen += chunk
            if needle in seen:
                return time.perf_counter()
    raise RuntimeError(f"did not see {needle!r}; output ended with {seen[-200:]!r}")


def spawn(args):
    return subprocess.Popen(
        [BIN, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def stop(proc):
    proc.kill()
    proc.wait()


def boot_args(cpus, memory):
    return ["--kernel", KERNEL, "--initrd", INITRD, "--cpus", str(cpus), "--memory", str(memory)]


def boot_to_shell(cpus, memory):
    start = time.perf_counter()
    proc = spawn(boot_args(cpus, memory))
    end = wait_for(proc, PROMPT)
    stop(proc)
    return (end - start) * 1000


def snapshot(cpus, memory, path):
    proc = spawn(boot_args(cpus, memory) + ["--stop-at", PROMPT.decode(), "--snapshot", path])
    _, err = proc.communicate(timeout=30)
    m = re.search(rb"snapshot written to .* in ([0-9.]+)(ms|s)", err)
    if not m:
        raise RuntimeError(f"no snapshot: {err[-300:]!r}")
    value = float(m.group(1))
    return value * 1000 if m.group(2) == b"s" else value


def restore_to_output(path):
    """Restores, types a command at once, and returns (ms until its output,
    process) with the process still running."""
    start = time.perf_counter()
    proc = spawn(["--restore", path])
    proc.stdin.write(b"echo RE''ADY\n")
    proc.stdin.flush()
    end = wait_for(proc, b"READY")
    return (end - start) * 1000, proc


def footprint_mb(pid):
    """phys_footprint of a process in MB, from the footprint tool."""
    out = subprocess.run(["footprint", str(pid)], capture_output=True, text=True).stdout
    m = re.search(r"Footprint:\s+([0-9.]+)\s+(KB|MB|GB)", out)
    if not m:
        return None
    scale = {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(2)]
    return float(m.group(1)) * scale


def idle_cpu_percent(pid, seconds=2.0):
    def cpu_seconds():
        out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        parts = [float(p) for p in out.replace("-", ":").split(":")]
        total = 0.0
        for p in parts:
            total = total * 60 + p
        return total

    before = cpu_seconds()
    time.sleep(seconds)
    return (cpu_seconds() - before) / seconds * 100


def summary(values):
    values = sorted(values)
    return f"median {statistics.median(values):.1f} ms ({values[0]:.1f} to {values[-1]:.1f})"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--memory", type=int, default=2048, help="guest RAM in MiB")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--cpus", default="1,2")
    args = parser.parse_args()
    work = tempfile.mkdtemp(prefix="cave-vcpu-bench-")
    try:
        for cpus in [int(c) for c in args.cpus.split(",")]:
            print(f"== {cpus} vCPU(s), {args.memory} MiB")
            boots = [boot_to_shell(cpus, args.memory) for _ in range(args.runs)]
            print(f"boot to shell: {summary(boots)}")

            path = os.path.join(work, f"snap{cpus}")
            saves = []
            for i in range(3):
                shutil.rmtree(path, ignore_errors=True)
                saves.append(snapshot(cpus, args.memory, path))
            print(f"save a snapshot: {summary(saves)}")
            on_disk = int(subprocess.run(["du", "-sk", os.path.join(path, "memory")],
                                         capture_output=True, text=True).stdout.split()[0])
            print(f"memory file on disk: {on_disk / 1024:.0f} MiB")

            restores = []
            for _ in range(args.runs):
                ms, proc = restore_to_output(path)
                restores.append(ms)
                stop(proc)
            print(f"restore to the first command's output: {summary(restores)}")

            start = time.perf_counter()
            procs = []
            for _ in range(10):
                proc = spawn(["--restore", path])
                proc.stdin.write(b"echo RE''ADY\n")
                proc.stdin.flush()
                procs.append(proc)
            for proc in procs:
                wait_for(proc, b"READY")
            print(f"ten forks at once, all ran a command: {(time.perf_counter() - start) * 1000:.1f} ms")
            for proc in procs:
                stop(proc)

            _, proc = restore_to_output(path)
            time.sleep(1.5)
            print(f"idle restored VM: {footprint_mb(proc.pid)} MB phys_footprint, "
                  f"{idle_cpu_percent(proc.pid):.1f}% CPU")
            stop(proc)

            proc = spawn(boot_args(cpus, args.memory))
            wait_for(proc, PROMPT)
            time.sleep(1.5)
            print(f"idle booted VM: {footprint_mb(proc.pid)} MB phys_footprint, "
                  f"{idle_cpu_percent(proc.pid):.1f}% CPU")
            stop(proc)
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
