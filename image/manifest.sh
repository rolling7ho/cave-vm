#!/bin/sh
# Writes images.json (D28) next to a locally built image and kernel, so the
# daemon boots them with CAVE_IMAGE_DIR set to that folder.
#
#   image/manifest.sh [dir]     default: target/image, holding debian.img and Image
#
# ubuntu.img, from image/ubuntu/build.sh, is listed too when it is there.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
dir="$(cd "${1:-$here/../target/image}" && pwd)"
cd "$dir"
for f in debian.img Image; do
    [ -f "$f" ] || { echo "manifest.sh: no $f in $dir; run image/kernel/build.sh and image/debian/build.sh" >&2; exit 1; }
done
entry() {
    printf '{ "url": "%s", "sha256": "%s", "size": %s }' \
        "$1" "$(shasum -a 256 "$1" | cut -d' ' -f1)" "$(stat -f %z "$1")"
}
image() {
    printf '    {\n      "name": "%s",\n      "description": "%s, local build",\n      "disk": %s,\n      "kernel": %s\n    }' \
        "$1" "$2" "$(entry "$1.img")" "$(entry Image)"
}
{
    printf '{\n  "version": 1,\n  "images": [\n'
    image debian "Debian 13 (trixie)"
    if [ -f ubuntu.img ]; then
        printf ',\n'
        image ubuntu "Ubuntu 26.04 LTS (resolute)"
    fi
    printf '\n  ]\n}\n'
} > images.json
echo "Wrote $dir/images.json; boot it with CAVE_IMAGE_DIR=$dir"
