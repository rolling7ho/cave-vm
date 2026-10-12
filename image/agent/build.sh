#!/bin/sh
# Builds the guest agent as a static aarch64 musl binary in an Apple
# `container` running Alpine, whose native target is aarch64 musl (ROADMAP.md
# D6: until cave can build inside its own VMs). No cross toolchain is needed
# on the Mac.
#
#   image/agent/build.sh [output-dir]     default: target/agent
#
# Needs Apple's container CLI with `container system start` run once. Crates
# are fetched inside the container and cached in target/agent/cargo-home.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${1:-$repo/target/agent}"
mkdir -p "$out/cargo-home" "$out/target"
out="$(cd "$out" && pwd)"

command -v container >/dev/null || {
    echo "build.sh: Apple's container CLI is not installed; see https://github.com/apple/container" >&2
    exit 1
}

toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"

container run --rm \
    --cpus 8 --memory 4G \
    --volume "$repo:/src" \
    --volume "$out:/out" \
    --env CARGO_HOME=/out/cargo-home \
    --env CARGO_TARGET_DIR=/out/target \
    "docker.io/library/rust:${toolchain}-alpine" \
    sh -c 'cd /src && cargo build --locked --release -p cave-vm-agent && cp /out/target/release/cave-vm-agent /out/cave-agent'

file "$out/cave-agent" 2>/dev/null || true
echo "Built $out/cave-agent"
