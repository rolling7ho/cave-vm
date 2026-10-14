#!/bin/sh
# Runs inside an Ubuntu arm64 container. Builds the base image's root file
# system with mmdebstrap, customized by image/debian/customize.sh (mounted
# at /work), and writes it to /out/ubuntu.img as ext4, without loop devices.
# Called by build.sh; not meant to run on a Mac.
set -eu

# Ubuntu 26.04 LTS. arm64 packages are on the ports mirror; nodejs and npm
# are in universe. UBUNTU_MIRROR picks another ports mirror; apt checks
# every file against the Ubuntu archive key either way.
SUITE=resolute
MIRROR="${UBUNTU_MIRROR:-http://ports.ubuntu.com/ubuntu-ports}"
# Room the image starts with, as for Debian.
FREE_MIB=1024

export DEBIAN_FRONTEND=noninteractive
# The container fetches its own tools from the same mirror.
if [ -n "${UBUNTU_MIRROR:-}" ]; then
    sed -i "s|^URIs: .*|URIs: $MIRROR|" /etc/apt/sources.list.d/ubuntu.sources
fi
apt-get update -qq
apt-get install -y -qq --no-install-recommends mmdebstrap e2fsprogs ca-certificates ubuntu-keyring >/dev/null

packages="systemd-sysv,udev,dbus,sudo,ca-certificates,curl,git,vim,less,procps"
packages="$packages,iproute2,iputils-ping,netbase,e2fsprogs"
packages="$packages,python3,python3-pip,python3-venv,nodejs,npm,openssh-server"

root=/tmp/root
rm -rf "$root"
mmdebstrap --mode=root --variant=apt --components=main,universe --include="$packages" \
    --customize-hook='sh /work/customize.sh "$1"' \
    "$SUITE" "$root" "$MIRROR"

used_mib=$(du -sm "$root" | cut -f1)
size_mib=$(( (used_mib * 12 / 10 + FREE_MIB + 63) / 64 * 64 ))
rm -f /out/ubuntu.img
mkfs.ext4 -q -L cave-root -d "$root" -E root_owner=0:0 /out/ubuntu.img "${size_mib}M"
e2fsck -fn /out/ubuntu.img >/dev/null
(cd /out && sha256sum ubuntu.img | cut -d' ' -f1 > ubuntu.img.sha256)
echo "ubuntu.img: ${size_mib} MiB file system, sha256 $(cat /out/ubuntu.img.sha256)"
