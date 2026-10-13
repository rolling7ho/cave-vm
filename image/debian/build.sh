#!/bin/sh
# Builds the Debian base image in an Apple `container` (ROADMAP.md P3.9, D6).
# The guest agent is built first with image/agent/build.sh.
#
#   image/debian/build.sh [output-dir]     default: target/image
#
# Writes debian.img, a raw ext4 file system that the VM boots from as
# /dev/vda, and debian.img.sha256. Needs Apple's container CLI with
# `container system start` run once. Publishing the image and its digest is
# release automation, which stays with the maintainers.
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
    --volume "$here:/work" \
    --volume "$repo/target/agent:/agent" \
    --volume "$out:/out" \
    docker.io/library/debian:trixie \
    sh /work/build-in-container.sh

echo "Built $out/debian.img ($(cat "$out/debian.img.sha256"))"
