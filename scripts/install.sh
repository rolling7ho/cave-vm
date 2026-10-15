#!/bin/sh
# Installs cave from a GitHub release (design, "Bootstrap skill": a plain
# `curl | sh` one-liner for people who prefer it; P6.5):
#
#   curl -fsSL https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh | sh
#
# Each release carries a copy of this script with VERSION and SHA256 filled
# in, so it installs exactly that release's binaries and checks them before
# anything is written. It fetches nothing else.
#
# CAVE_PREFIX picks where it installs (default ~/.local, so binaries go in
# ~/.local/bin). cave-vmm is signed ad hoc on this Mac with the
# Hypervisor.framework entitlement; no Apple account or certificate is used.
set -eu

VERSION="@VERSION@"
SHA256="@SHA256@"
PREFIX="${CAVE_PREFIX:-$HOME/.local}"
REPO="https://github.com/rolling7ho/cave-vm"

fail() {
    echo "install.sh: $1" >&2
    exit 1
}

case "$VERSION$SHA256" in
*@*) fail "this copy has no release pinned; use the install.sh attached to a release: $REPO/releases" ;;
esac
[ "$(uname -s)" = Darwin ] || fail "cave runs on macOS only"
[ "$(uname -m)" = arm64 ] || fail "cave needs an Apple Silicon Mac"
major="$(sw_vers -productVersion | cut -d. -f1)"
[ "$major" -ge 26 ] || fail "cave needs macOS 26 or later; this Mac has $(sw_vers -productVersion)"

name="cave-vm-$VERSION-aarch64-apple-darwin"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading cave $VERSION"
curl -fsSL "$REPO/releases/download/v$VERSION/$name.tar.gz" -o "$tmp/cave.tar.gz"
got="$(shasum -a 256 "$tmp/cave.tar.gz" | cut -d' ' -f1)"
[ "$got" = "$SHA256" ] || fail "the download's checksum is $got, not $SHA256; nothing was installed"
tar -xzf "$tmp/cave.tar.gz" -C "$tmp"

mkdir -p "$PREFIX/bin"
for f in cave cave-daemon cave-vmm; do
    install -m 0755 "$tmp/cave-vm-$VERSION/$f" "$PREFIX/bin/$f"
done
/usr/bin/codesign --sign - --force \
    --entitlements "$tmp/cave-vm-$VERSION/hypervisor.entitlements" "$PREFIX/bin/cave-vmm"

echo "Installed cave $VERSION in $PREFIX/bin"
case ":$PATH:" in
*":$PREFIX/bin:"*) ;;
*) echo "Add it to your PATH: echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.zprofile, then open a new terminal" ;;
esac
echo "Next: cd my-project && cave run"
