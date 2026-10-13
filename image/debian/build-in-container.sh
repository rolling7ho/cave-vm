#!/bin/sh
# Runs inside a Debian arm64 container. Builds the base image's root file
# system with mmdebstrap and writes it to /out/debian.img as ext4, without
# loop devices. Called by build.sh; not meant to run on a Mac.
set -eu

SUITE=trixie
# Room the image starts with. Project disks are 20 GB clones of it, and the
# guest grows the file system to fill the disk at boot (see customize.sh).
FREE_MIB=1024

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends mmdebstrap e2fsprogs ca-certificates >/dev/null

packages="systemd-sysv,udev,dbus,sudo,ca-certificates,curl,git,vim,less,procps"
packages="$packages,iproute2,iputils-ping,netbase,e2fsprogs"
packages="$packages,python3,python3-pip,python3-venv,nodejs,npm,openssh-server"

root=/tmp/root
rm -rf "$root"
mmdebstrap --mode=root --variant=apt --include="$packages" \
    --customize-hook='sh /work/customize.sh "$1"' \
    "$SUITE" "$root"

used_mib=$(du -sm "$root" | cut -f1)
size_mib=$(( (used_mib * 12 / 10 + FREE_MIB + 63) / 64 * 64 ))
rm -f /out/debian.img
mkfs.ext4 -q -L cave-root -d "$root" -E root_owner=0:0 /out/debian.img "${size_mib}M"
e2fsck -fn /out/debian.img >/dev/null
(cd /out && sha256sum debian.img | cut -d' ' -f1 > debian.img.sha256)
echo "debian.img: ${size_mib} MiB file system, sha256 $(cat /out/debian.img.sha256)"
