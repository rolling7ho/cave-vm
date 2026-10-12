#!/bin/sh
# P2.8 signing check (ROADMAP.md A5). Builds a probe that creates a VM, then
# tries it signed and installed different ways, printing WORKS or FAILS for
# each. Exit 137 means macOS killed the process at launch. Everything happens in a temporary folder; nothing is installed.
#
# Run on macOS 26 or later with no paid Apple developer account:
#   scripts/spike/signing-check.sh
set -u
repo="$(cd "$(dirname "$0")/../.." && pwd)"
ent="$repo/scripts/hypervisor.entitlements"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo build -q -p cave-vm-hvf --example hvf_probe --manifest-path "$repo/Cargo.toml" || exit 1
src="$repo/target/debug/examples/hvf_probe"

echo "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(uname -m)"
echo

check() { # name command...
    name="$1"; shift
    output="$("$@" 2>&1)"; status=$?
    if [ "$status" -eq 0 ]; then got=WORKS; else got=FAILS; fi
    printf '%-6s %-60s (exit %s)\n' "$got" "$name" "$status"
}

cp "$src" "$work/a"
check "linker signature only, no entitlement" "$work/a"

cp "$src" "$work/b"
codesign --sign - --force --entitlements "$ent" "$work/b" 2>/dev/null
check "ad hoc signed with the hypervisor entitlement" "$work/b"

cp "$work/b" "$work/c"
xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$work/c"
check "as above, plus a browser-download quarantine flag" "$work/c"

mkdir -p "$work/prefix/bin"
cp "$work/b" "$work/prefix/bin/cave"
check "as above, copied into a bin folder (install script)" "$work/prefix/bin/cave"

cp "$work/b" "$work/e"
install_name_tool -add_rpath /opt/homebrew/lib "$work/e" 2>/dev/null
codesign --sign - --force --preserve-metadata=entitlements,requirements,flags,runtime "$work/e" 2>/dev/null
check "modified then re-signed the way Homebrew relocates bottles" "$work/e"

cp "$src" "$work/f"
codesign --sign - --force --options runtime --entitlements "$ent" "$work/f" 2>/dev/null
check "ad hoc, hardened runtime, with the entitlement" "$work/f"

echo
echo "vmnet (A6): can an ad hoc signed binary start a shared-mode interface as this user?"
if command -v swiftc >/dev/null; then
    swiftc -O -module-name vmnet_probe -o "$work/vmnet" "$repo/scripts/spike/vmnet_probe.swift" || exit 1
    check "vmnet, linker signature only" "$work/vmnet"
    cp "$work/vmnet" "$work/vmnet-ent"
    codesign --sign - --force --entitlements "$repo/scripts/spike/vmnet.entitlements" "$work/vmnet-ent" 2>/dev/null
    check "vmnet, ad hoc with com.apple.vm.networking" "$work/vmnet-ent"
else
    echo "SKIP  swiftc not found; install the Xcode command line tools for the vmnet check"
fi
