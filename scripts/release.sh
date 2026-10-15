#!/bin/sh
# Publishes a cave-vm release: the steps decision 0016 lists, in one go.
#
#   scripts/release.sh 0.1.1 --dry-run   # build everything into target/dist, publish nothing
#   scripts/release.sh 0.1.1             # the same, then publish after asking
#
# Before running it, merge the usual "chore: release <version>" PR: the
# workspace version in Cargo.toml is <version>, and CHANGELOG.md has a
# "## [<version>]" section, which becomes the release notes.
#
# Publishing:
#   1. tags main as v<version> and pushes the tag;
#   2. creates the GitHub release with cave-vm-<version>-aarch64-apple-darwin.tar.gz
#      (cave, cave-daemon, cave-vmm, LICENSE, hypervisor.entitlements),
#      install.sh with @VERSION@ and @SHA256@ filled in, and images.json;
#   3. points Formula/cave-vm.rb in rolling7ho/homebrew-cave at the new tag's
#      source tarball (url and sha256) and pushes it.
#
# images.json is copied from the latest release, so first runs keep finding
# the same images: a release never drops older entries (decision 0010).
# Publishing new images stays a separate step (image/manifest.sh).
#
# It uses your own `gh` login and signs nothing: the formula and install.sh
# sign cave-vmm ad hoc on each Mac.
set -eu

REPO=rolling7ho/cave-vm
TAP=rolling7ho/homebrew-cave
FORMULA=Formula/cave-vm.rb
TARGET=aarch64-apple-darwin

fail() {
    echo "release.sh: $1" >&2
    exit 1
}

version="${1:-}"
dry_run=no
case "${2:-}" in
"") ;;
--dry-run) dry_run=yes ;;
*) fail "unknown option $2; the options are: <version> [--dry-run]" ;;
esac
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    fail "give the version to release, like: scripts/release.sh 0.1.1 --dry-run"
tag="v$version"

cd "$(git rev-parse --show-toplevel)"

# The tree is main as pushed, with the version bump merged.
[ -z "$(git status --porcelain)" ] || fail "the working tree has changes; commit or stash them first"
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
    fail "check out main as it is on GitHub first: git checkout main && git pull"
grep -q "^version = \"$version\"$" Cargo.toml ||
    fail "Cargo.toml's workspace version is not $version; merge the chore: release $version PR first"
grep -q "^## \[$version\]" CHANGELOG.md ||
    fail "CHANGELOG.md has no ## [$version] section; merge the chore: release $version PR first"
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
    fail "$tag is already on GitHub; pick the next version"
fi

dist="target/dist/$tag"
name="cave-vm-$version"
rm -rf "$dist"
mkdir -p "$dist/$name"

echo "Building $name"
cargo build --release --locked -p cave-vm-cli -p cave-vm-daemon -p cave-vm-vmm --bin cave --bin cave-daemon --bin cave-vmm
for f in cave cave-daemon cave-vmm; do
    cp "target/release/$f" "$dist/$name/$f"
done
cp LICENSE "$dist/$name/LICENSE"
cp scripts/hypervisor.entitlements "$dist/$name/hypervisor.entitlements"
tarball="$name-$TARGET.tar.gz"
COPYFILE_DISABLE=1 tar -czf "$dist/$tarball" -C "$dist" "$name"
sha="$(shasum -a 256 "$dist/$tarball" | cut -d' ' -f1)"

sed -e "s/@VERSION@/$version/" -e "s/@SHA256@/$sha/" scripts/install.sh >"$dist/install.sh"
if grep -q -e @VERSION@ -e @SHA256@ "$dist/install.sh"; then
    fail "install.sh still has a placeholder"
fi

gh release download --repo "$REPO" --pattern images.json --dir "$dist" --clobber ||
    fail "cannot fetch images.json from the latest release"
python3 -c 'import json,sys; m=json.load(open(sys.argv[1])); assert m["images"]' "$dist/images.json" ||
    fail "the latest release's images.json is not a manifest with images"

# The release notes: this version's changelog section.
awk -v v="## [$version]" '
    index($0, v) == 1 { on = 1; next }
    on && /^## \[/ { exit }
    on { print }
' CHANGELOG.md >"$dist/notes.md"
printf '\nThe full list of changes is in [CHANGELOG.md](https://github.com/%s/blob/%s/CHANGELOG.md).\n' \
    "$REPO" "$tag" >>"$dist/notes.md"

echo
echo "Built in $dist:"
echo "  $tarball  sha256 $sha"
echo "  install.sh (pinned to $version)"
echo "  images.json (copied from the latest release)"
echo "  notes.md (from CHANGELOG.md)"
if [ "$dry_run" = yes ]; then
    echo "Dry run: nothing was published"
    exit 0
fi

printf 'Publish cave-vm %s: push tag %s, create the release and update %s? [y/N] ' "$version" "$tag" "$TAP"
read -r answer
case "$answer" in
y | Y | yes) ;;
*) fail "nothing was published" ;;
esac

git tag -a "$tag" -m "cave-vm $version"
git push origin "$tag"
gh release create "$tag" --repo "$REPO" --verify-tag --title "cave-vm $version" \
    --notes-file "$dist/notes.md" \
    "$dist/$tarball" "$dist/install.sh" "$dist/images.json"
echo "Released $tag: https://github.com/$REPO/releases/tag/$tag"

# The formula builds from the tag's source tarball, so its checksum is
# GitHub's archive of the tag.
source_url="https://github.com/$REPO/archive/refs/tags/$tag.tar.gz"
curl -fsSL "$source_url" -o "$dist/source.tar.gz"
source_sha="$(shasum -a 256 "$dist/source.tar.gz" | cut -d' ' -f1)"
tap="$dist/tap"
gh repo clone "$TAP" "$tap" -- --quiet
sed -i '' \
    -e "s|^  url \".*\"$|  url \"$source_url\"|" \
    -e "s|^  sha256 \".*\"$|  sha256 \"$source_sha\"|" \
    "$tap/$FORMULA"
grep -q "$source_sha" "$tap/$FORMULA" || fail "could not update $FORMULA; edit its url and sha256 by hand"
git -C "$tap" commit -q -am "cave-vm $version"
git -C "$tap" push -q origin HEAD
echo "Updated $TAP: brew upgrade cave-vm now installs $version"
