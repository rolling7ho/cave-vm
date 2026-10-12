#!/bin/sh
# P2.10 fork experiment: restores one snapshot into two VMs at once and
# checks they diverge independently. Also times a clonefile copy of the
# snapshot, the other way to give a fork its own memory file.
#
#   scripts/spike/fork-check.sh <snapshot-dir>
#
# Make the snapshot first (see docs/spike/p2.9-snapshot.md). Needs a release
# build of cave-vmm-spike signed by scripts/hypervisor-runner.sh.
set -eu
snap="$1"
bin="${CAVE_SPIKE_BIN:-target/release/cave-vmm-spike}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fork() {
    name="$1"
    {
        sleep 0.5
        echo "echo $name > /tmp/who; sleep 1; echo SEE-\$(cat /tmp/who)"
        sleep 2
        echo "poweroff -f"
    } | "$bin" --restore "$snap" >"$work/$name.out" 2>"$work/$name.err" &
}

echo "== two forks of one snapshot, running at the same time"
start=$(python3 -c 'import time; print(time.time())')
fork alpha
fork beta
wait
end=$(python3 -c 'import time; print(time.time())')
for name in alpha beta; do
    seen=$(grep -a -o "SEE-[a-z]*" "$work/$name.out" | tail -1)
    restored=$(grep -a "restored in" "$work/$name.err" | sed 's/.*restored in //')
    stopped=$(grep -a "stopped:" "$work/$name.err" | sed 's/.*stopped: //')
    if [ "$seen" = "SEE-$name" ]; then verdict=WORKS; else verdict=FAILS; fi
    echo "$name: $verdict (saw ${seen:-nothing}, restored in $restored, stopped: $stopped)"
done
python3 -c "print('both forks ran start to poweroff in %.0f ms' % (($end - $start) * 1000))"

echo "== clonefile copy of the snapshot"
python3 - "$snap" "$work/clone" <<'PY'
import subprocess, sys, time, statistics
src, dst = sys.argv[1], sys.argv[2]
times = []
for i in range(10):
    d = f"{dst}{i}"
    t = time.perf_counter()
    subprocess.run(["cp", "-c", "-R", src, d], check=True)
    times.append((time.perf_counter() - t) * 1000)
times.sort()
print("cp -c (clonefile) of the snapshot dir: median %.1f ms, min %.1f, max %.1f"
      % (statistics.median(times), times[0], times[-1]))
PY
