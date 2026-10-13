#!/bin/sh
# Runs inside a Debian arm64 container. Builds the guest kernel Image into
# /out. Called by build.sh; not meant to run on a Mac.
set -eu

KERNEL_VERSION=6.18.56
KERNEL_SHA256=4633bbda2ab95eaf2a7af4041236451d6759c72e6946265531af4b3aacefbf89

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    build-essential bc bison flex libelf-dev libssl-dev \
    ca-certificates curl xz-utils >/dev/null

# The source tarball is kept in /out/cache between builds.
mkdir -p /out/cache
tarball="/out/cache/linux-${KERNEL_VERSION}.tar.xz"
[ -f "$tarball" ] || curl -sSfL -o "$tarball" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"
echo "${KERNEL_SHA256}  $tarball" | sha256sum -c -
cd /tmp
tar xf "$tarball"
cd "linux-${KERNEL_VERSION}"

make -s ARCH=arm64 allnoconfig
scripts/kconfig/merge_config.sh -m .config /work/kernel.config >/dev/null
make -s ARCH=arm64 olddefconfig

# merge_config drops options whose dependencies are unmet; fail loudly.
for want in $(grep '^CONFIG_.*=y' /work/kernel.config); do
    grep -qx "$want" .config || { echo "missing after olddefconfig: $want" >&2; exit 1; }
done

make -s ARCH=arm64 -j"$(nproc)" Image
cp arch/arm64/boot/Image /out/Image
cp .config /out/kernel.config.full
sha256sum /out/Image
