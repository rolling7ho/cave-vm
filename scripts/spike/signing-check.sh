#!/bin/sh
# P2.8 signing check (ROADMAP.md A5). Builds a probe that creates a VM, then
# tries it signed and installed different ways, printing PASS or FAIL for
# each. Everything happens in a temporary folder; nothing is installed.
#
# Run on a clean macOS 26 Mac with no paid Apple developer account:
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

check() { # expected-result name binary
    expected="$1"; name="$2"; shift 2
    output="$("$@" 2>&1)"; status=$?
    if [ "$status" -eq 0 ]; then got=works; else got=fails; fi
    if [ "$got" = "$expected" ]; then verdict=PASS; else verdict=FAIL; fi
    printf '%s  %-58s %s (exit %s)\n' "$verdict" "$name" "$got" "$status"
    [ "$verdict" = PASS ] || printf '      %s\n' "$output"
}

cp "$src" "$work/a"
check fails "linker signature only, no entitlement" "$work/a"

cp "$src" "$work/b"
codesign --sign - --force --entitlements "$ent" "$work/b" 2>/dev/null
check works "ad hoc signed with the hypervisor entitlement" "$work/b"

cp "$work/b" "$work/c"
xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$work/c"
check works "as above, plus a browser-download quarantine flag" "$work/c"

mkdir -p "$work/prefix/bin"
cp "$work/b" "$work/prefix/bin/cave"
check works "as above, copied into a bin folder (install script)" "$work/prefix/bin/cave"

cp "$work/b" "$work/e"
install_name_tool -add_rpath /opt/homebrew/lib "$work/e" 2>/dev/null
codesign --sign - --force --preserve-metadata=entitlements,requirements,flags,runtime "$work/e" 2>/dev/null
check works "modified then re-signed the way Homebrew relocates bottles" "$work/e"

cp "$src" "$work/f"
codesign --sign - --force --options runtime --entitlements "$ent" "$work/f" 2>/dev/null
check works "ad hoc, hardened runtime, with the entitlement" "$work/f"
