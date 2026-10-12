#!/bin/sh
# Builds the spike kernel and initramfs in an Apple `container` (ROADMAP.md
# D6: a container for the kernel until cave can build inside its own VMs).
#
#   image/spike/build.sh [output-dir]     default: target/spike
#
# Needs Apple's container CLI with `container system start` run once.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$here/../../target/spike}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

command -v container >/dev/null || {
    echo "build.sh: Apple's container CLI is not installed; see https://github.com/apple/container" >&2
    exit 1
}

container run --rm \
    --cpus 8 --memory 8G \
    --volume "$here:/work" \
    --volume "$out:/out" \
    docker.io/library/debian:trixie \
    sh /work/build-in-container.sh

echo "Built $out/Image and $out/initramfs.cpio.gz. Boot with:"
echo "  cargo run -p cave-vm-vmm --bin cave-vmm-spike -- --kernel $out/Image --initrd $out/initramfs.cpio.gz"
