#!/bin/sh
# Builds the Ubuntu base image in an Apple `container` (ROADMAP.md P6.8),
# the same way image/debian/build.sh builds the Debian one and with the same
# customization. The guest agent is built first with image/agent/build.sh.
#
#   image/ubuntu/build.sh [output-dir]     default: target/image
#
# Writes ubuntu.img, a raw ext4 file system that the VM boots from as
# /dev/vda, and ubuntu.img.sha256. Needs Apple's container CLI with
# `container system start` run once. Publishing the image and its digest is
# release automation, which stays with the maintainers.
# UBUNTU_MIRROR, if set, is the ports mirror to fetch packages from.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${1:-$repo/target/image}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

command -v container >/dev/null || {
    echo "build.sh: Apple's container CLI is not installed; see https://github.com/apple/container" >&2
    exit 1
}

"$repo/image/agent/build.sh" "$repo/target/agent"

container run --rm \
    --cpus 8 --memory 4G \
    --volume "$repo/image/debian:/work" \
    --volume "$here:/ubuntu" \
    --volume "$repo/target/agent:/agent" \
    --volume "$out:/out" \
    --env "UBUNTU_MIRROR=${UBUNTU_MIRROR:-}" \
    docker.io/library/ubuntu:26.04 \
    sh /ubuntu/build-in-container.sh

echo "Built $out/ubuntu.img ($(cat "$out/ubuntu.img.sha256"))"
