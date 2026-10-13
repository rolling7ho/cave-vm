//! The image manifest, `images.json` ([D28]): every published image version
//! with its disk and kernel, newest first. Read from the latest GitHub
//! release, or from `CAVE_IMAGE_DIR` for a local build.
//!
//! [D28]: ../../../../docs/decisions/0010-image-manifest.md

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

use serde_json::Value;

use super::{Digest, ImageRef, Source};

/// Where cave reads the manifest of published images.
pub const RELEASE_MANIFEST: &str =
    "https://github.com/rolling7ho/cave-vm/releases/latest/download/images.json";

/// The manifest's file name, in a release or in `CAVE_IMAGE_DIR`.
pub const MANIFEST_FILE: &str = "images.json";

/// One file: a disk image or a kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// An `https` URL, or a file name in `CAVE_IMAGE_DIR`.
    pub url: String,
    pub digest: Digest,
    pub size: u64,
}

/// One published version of an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: String,
    pub disk: Artifact,
    pub kernel: Artifact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Newest first.
    pub images: Vec<Entry>,
}

/// Why the manifest could not be used. The messages say how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Not JSON, an unknown version, or a field missing or malformed.
    Malformed(String),
    /// No image by that name.
    UnknownName(String),
    /// A pinned digest no published version has.
    UnknownDigest(ImageRef),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Malformed(why) => write!(
                f,
                "the image manifest is not valid: {why}\nUpdate cave; if CAVE_IMAGE_DIR is set, rebuild the image there"
            ),
            ManifestError::UnknownName(name) => write!(
                f,
                "there is no image called {name}\nUse a name from cave images"
            ),
            ManifestError::UnknownDigest(image) => write!(
                f,
                "no published image matches {image}\nTo use the newest version, set image = \"{}\" in cave.toml",
                image.name
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

fn malformed(why: impl Into<String>) -> ManifestError {
    ManifestError::Malformed(why.into())
}

fn string<'a>(v: &'a Value, key: &str, at: &str) -> Result<&'a str, ManifestError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(format!("{at} has no {key} string")))
}

fn artifact(v: &Value, at: &str) -> Result<Artifact, ManifestError> {
    let v = v
        .as_object()
        .map(|_| v)
        .ok_or_else(|| malformed(format!("{at} is not an object")))?;
    let url = string(v, "url", at)?.to_string();
    let hex = string(v, "sha256", at)?;
    let digest = format!("sha256:{hex}")
        .parse()
        .map_err(|_| malformed(format!("{at} has a bad sha256")))?;
    let size = v
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed(format!("{at} has no size")))?;
    Ok(Artifact { url, digest, size })
}

impl Manifest {
    /// Reads `images.json`. Unknown keys are ignored, so later versions of
    /// the format can add some.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let doc: Value = serde_json::from_str(text).map_err(|e| malformed(e.to_string()))?;
        match doc.get("version").and_then(Value::as_u64) {
            Some(1) => {}
            Some(v) => return Err(malformed(format!("version {v} is newer than this cave"))),
            None => return Err(malformed("no version")),
        }
        let list = doc
            .get("images")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("no images list"))?;
        let mut images = Vec::new();
        for (i, v) in list.iter().enumerate() {
            let at = format!("image {}", i + 1);
            let name = string(v, "name", &at)?;
            let at = format!("image {} ({name})", i + 1);
            images.push(Entry {
                name: name.to_string(),
                description: v
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                disk: artifact(v.get("disk").unwrap_or(&Value::Null), &format!("{at} disk"))?,
                kernel: artifact(
                    v.get("kernel").unwrap_or(&Value::Null),
                    &format!("{at} kernel"),
                )?,
            });
        }
        Ok(Manifest { images })
    }

    /// The version `image` means: the one with its pinned disk digest, or
    /// the newest with its name.
    pub fn resolve(&self, image: &ImageRef) -> Result<&Entry, ManifestError> {
        let name = image.resolved_name();
        let mut named = self.images.iter().filter(|e| e.name == name).peekable();
        if named.peek().is_none() {
            return Err(ManifestError::UnknownName(image.name.clone()));
        }
        match &image.digest {
            None => Ok(named.next().expect("peeked")),
            Some(digest) => named
                .find(|e| e.disk.digest == *digest)
                .ok_or_else(|| ManifestError::UnknownDigest(image.clone())),
        }
    }

    /// The newest version of each name, for `cave images`.
    pub fn newest(&self) -> Vec<&Entry> {
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for e in &self.images {
            if !seen.contains(&&e.name) {
                seen.push(&e.name);
                out.push(e);
            }
        }
        out
    }
}

/// Where images come from: the latest GitHub release, or a local folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Release,
    Dir(PathBuf),
}

impl Origin {
    /// `CAVE_IMAGE_DIR` if set, otherwise the release.
    pub fn from_env() -> Origin {
        match std::env::var_os("CAVE_IMAGE_DIR") {
            Some(dir) if !dir.is_empty() => Origin::Dir(PathBuf::from(dir)),
            _ => Origin::Release,
        }
    }

    /// Reads the manifest.
    pub fn manifest(&self) -> io::Result<String> {
        let mut text = String::new();
        self.open(match self {
            Origin::Release => RELEASE_MANIFEST,
            Origin::Dir(_) => MANIFEST_FILE,
        })?
        .read_to_string(&mut text)?;
        Ok(text)
    }

    /// Opens `url`: an `https` URL, or a file name in the local folder.
    fn open(&self, url: &str) -> io::Result<Box<dyn Read + Send>> {
        if url.starts_with("https://") {
            return Ok(Box::new(Curl::get(url)?));
        }
        match self {
            Origin::Dir(dir) if !url.contains('/') && !url.contains("..") => {
                Ok(Box::new(File::open(dir.join(url))?))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{url} is not an https URL"),
            )),
        }
    }

    /// A [`Source`] for the files `manifest` lists.
    pub fn source(&self, manifest: &Manifest) -> ManifestSource {
        let files = manifest
            .images
            .iter()
            .flat_map(|e| [&e.disk, &e.kernel])
            .map(|a| (a.digest, (a.url.clone(), a.size)))
            .collect();
        ManifestSource {
            origin: self.clone(),
            files,
        }
    }
}

/// Serves the files a manifest lists, by digest.
pub struct ManifestSource {
    origin: Origin,
    files: HashMap<Digest, (String, u64)>,
}

impl Source for ManifestSource {
    fn open(&self, digest: &Digest) -> io::Result<(Box<dyn Read + Send>, Option<u64>)> {
        let (url, size) = self.files.get(digest).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("the image manifest does not list {digest}"),
            )
        })?;
        Ok((self.origin.open(url)?, Some(*size)))
    }
}

/// An HTTPS download through macOS's `curl`, so the daemon needs no HTTP
/// stack of its own. The body is read from its output; a failed download
/// is an error at the end.
struct Curl {
    /// Taken when the download ends, to check how curl exited.
    child: Option<Child>,
    out: ChildStdout,
    url: String,
}

impl Curl {
    fn get(url: &str) -> io::Result<Curl> {
        let mut child = Command::new("/usr/bin/curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--proto", "=https", "--max-redirs", "5"])
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let out = child.stdout.take().expect("piped");
        Ok(Curl {
            child: Some(child),
            out,
            url: url.to_string(),
        })
    }
}

impl Read for Curl {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.out.read(buf)?;
        if n == 0
            && !buf.is_empty()
            && let Some(child) = self.child.take()
        {
            let result = child.wait_with_output()?;
            if !result.status.success() {
                let why = String::from_utf8_lossy(&result.stderr).trim().to_string();
                return Err(io::Error::other(format!(
                    "cannot download {}: {why}",
                    self.url
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for Curl {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests;
