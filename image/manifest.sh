#!/bin/sh
# Writes images.json (D28) next to a locally built image and kernel, so the
# daemon boots them with CAVE_IMAGE_DIR set to that folder.
#
#   image/manifest.sh [dir]     default: target/image, holding debian.img and Image
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
cat > images.json <<JSON
{
  "version": 1,
  "images": [
    {
      "name": "debian",
      "description": "Debian 13 (trixie), local build",
      "disk": $(entry debian.img),
      "kernel": $(entry Image)
    }
  ]
}
JSON
echo "Wrote $dir/images.json; boot it with CAVE_IMAGE_DIR=$dir"
