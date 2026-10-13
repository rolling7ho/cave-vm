#!/bin/sh
# Runs the guest agent's tests as root in an Apple `container` running
# Alpine, with a `cave` user in a `docker` group, so the tests that check
# commands run as that user (not root) take part. On the Mac they skip.
#
#   image/agent/test-as-root.sh
#
# Shares target/agent with build.sh for the crate cache and build output.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$repo/target/agent"
mkdir -p "$out/cargo-home" "$out/target"

command -v container >/dev/null || {
    echo "test-as-root.sh: Apple's container CLI is not installed; see https://github.com/apple/container" >&2
    exit 1
}

toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"

container run --rm \
    --cpus 8 --memory 4G \
    --volume "$repo:/src" \
    --volume "$out:/out" \
    --env CARGO_HOME=/out/cargo-home \
    --env CARGO_TARGET_DIR=/out/target \
    --env CAVE_AGENT_ROOT_TESTS=1 \
    "docker.io/library/rust:${toolchain}-alpine" \
    sh -c 'addgroup -S docker && adduser -D -s /bin/sh cave && addgroup cave docker && cd /src && cargo test --locked -p cave-vm-agent'
