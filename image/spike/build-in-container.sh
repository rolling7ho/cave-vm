#!/bin/sh
# Runs inside a Debian arm64 container. Builds the spike kernel Image and a
# busybox initramfs into /out. Called by build.sh; not meant to run on a Mac.
set -eu

KERNEL_VERSION=6.18.56
KERNEL_SHA256=4633bbda2ab95eaf2a7af4041236451d6759c72e6946265531af4b3aacefbf89

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    build-essential bc bison flex libelf-dev libssl-dev \
    ca-certificates curl xz-utils cpio gzip busybox-static >/dev/null

cd /tmp
curl -sSfLO "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"
echo "${KERNEL_SHA256}  linux-${KERNEL_VERSION}.tar.xz" | sha256sum -c -
tar xf "linux-${KERNEL_VERSION}.tar.xz"
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

root=/tmp/initramfs
mkdir -p "$root/bin" "$root/proc" "$root/sys" "$root/dev" "$root/tmp"
cp /bin/busybox "$root/bin/busybox"
cp /work/init "$root/init"
(cd "$root" && find . | cpio -o -H newc --quiet | gzip -9) > /out/initramfs.cpio.gz

ls -l /out
