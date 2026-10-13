# 0010: Where base images and the kernel come from (D28)

Date: 2026-10-13 · Decided by: Ivan · Status: accepted

## Context

`cave.toml` names a base image (`image = "base"`), and on first run cave pins it to a digest (D22). The design hosts the image and the kernel in the project's GitHub releases, but nothing said how a name becomes a digest, how the kernel for a pinned image is found, or how a developer boots a locally built image. Two options were put to Ivan in the project thread: a manifest on each release, or fixed file names on the latest release. Ivan chose the manifest.

## Decision

Each release carries an `images.json` asset. cave reads it from the stable URL `https://github.com/rolling7ho/cave-vm/releases/latest/download/images.json`, which needs no GitHub API call.

```json
{
  "version": 1,
  "images": [
    {
      "name": "base",
      "description": "Debian 13 (trixie)",
      "disk":   { "url": "https://.../debian.img",  "sha256": "...", "size": 2080374784 },
      "kernel": { "url": "https://.../Image",       "sha256": "...", "size": 10029064 }
    }
  ]
}
```

- `images` lists every published version, newest first, and a release never drops older entries. The newest entry for a name is what an unpinned `image = "base"` resolves to.
- The pin in `cave.toml` is the disk's digest (`base@sha256:<disk sha256>`). The kernel is found from the pin through the same manifest, so the one pin rebuilds the environment anywhere, as the design asks.
- Files are fetched into the image cache by digest, hashed as they are written, and kept only when size and SHA-256 match (P3.12a).
- `CAVE_IMAGE_DIR`, when set, is a local folder used in place of the release: its `images.json` may give `url` as a file name in that folder. The image build scripts write one, so a locally built image boots with `CAVE_IMAGE_DIR=target/image`.

## Consequences

- Publishing an image means uploading the files and an `images.json` that keeps the earlier entries. That is release automation, which stays with Ivan.
- `cave images` lists the names in the manifest.
- A pinned digest missing from the latest manifest is an error that names the digest and says to re-pin with `image = "base"`.
- Fetching the manifest and images is the first-run setup the design describes; it is separate from the update detector (P6.7), which only looks up whether a newer version exists.
