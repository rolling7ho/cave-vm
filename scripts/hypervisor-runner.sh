#!/bin/sh
# Cargo runner for macOS (see .cargo/config.toml): signs a freshly built test
# or binary ad hoc with the hypervisor entitlement, then runs it. Local
# development only; this is not release signing.
set -eu
binary="$1"
entitlements="$(dirname "$0")/hypervisor.entitlements"
codesign --sign - --force --entitlements "$entitlements" "$binary" 2>/dev/null
exec "$@"
