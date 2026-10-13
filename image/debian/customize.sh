#!/bin/sh
# mmdebstrap customize hook: turns the bootstrapped tree in $1 into the cave
# base image. Runs inside the build container.
set -eu
root="$1"
files=/work/files

# The guest agent, started by systemd as root so it can run commands as
# the cave user.
install -m 0755 /agent/cave-agent "$root/usr/local/bin/cave-agent"
install -m 0644 "$files/cave-agent.service" "$root/etc/systemd/system/"
chroot "$root" systemctl enable cave-agent.service

# The user commands run as, with passwordless sudo. The docker group exists
# so a Docker install inside the VM works for the cave user at once.
chroot "$root" groupadd --system docker
chroot "$root" useradd --create-home --shell /bin/bash --groups sudo,docker cave
echo 'cave ALL=(ALL) NOPASSWD:ALL' > "$root/etc/sudoers.d/cave"
chmod 0440 "$root/etc/sudoers.d/cave"

# The project directory: commands start here and the project's files are
# copied here (docs/cli.md).
install -d -m 0755 "$root/workspace"
chroot "$root" chown cave:cave /workspace

# The root file system is the project disk, which the kernel mounts read-write
# from its command line. systemd grows it to fill the disk at boot
# (x-systemd.growfs), since project disks are 20 GB clones of this image.
# The address and gateway also come from the command line, and the gateway is
# the DNS server.
echo 'LABEL=cave-root / ext4 defaults,x-systemd.growfs 0 0' > "$root/etc/fstab"
echo cave > "$root/etc/hostname"
printf '127.0.0.1 localhost\n127.0.1.1 cave\n::1 localhost ip6-localhost ip6-loopback\n' > "$root/etc/hosts"
rm -f "$root/etc/resolv.conf"
echo 'nameserver 192.168.127.1' > "$root/etc/resolv.conf"

# Nothing in the image may be the same in every VM: each VM generates its
# own machine ID and SSH host keys on first boot.
: > "$root/etc/machine-id"
rm -f "$root"/etc/ssh/ssh_host_*
mkdir -p "$root/etc/systemd/system/ssh.service.d"
install -m 0644 "$files/ssh-host-keys.conf" "$root/etc/systemd/system/ssh.service.d/"

# Faster boots: no login prompt on the console (the agent is the way in),
# and no background apt runs nobody asked for.
chroot "$root" systemctl mask serial-getty@ttyAMA0.service getty@tty1.service \
    apt-daily.timer apt-daily-upgrade.timer e2scrub_all.timer fstrim.timer

# Smaller image.
rm -rf "$root"/var/lib/apt/lists/* "$root"/var/cache/apt/*.bin
