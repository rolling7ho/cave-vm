//! The shared image cache: one read-only file per digest under
//! `<data directory>/images/`.
//!
//! A download goes to a partial file first and is hashed as it is written.
//! Only a download whose size and SHA-256 match is renamed into place, so
//! the cache never holds a truncated or wrong image, and an interrupted
//! download leaves nothing behind but a partial file that the next fetch or
//! [`Cache::remove_partials`] deletes.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

use super::Digest;

/// Where images come from: the project's GitHub releases, or a local
/// folder in tests. Never the network in tests.
pub trait Source: Send + Sync {
    /// Opens the image with `digest`, and says how many bytes it has if the
    /// source knows.
    fn open(&self, digest: &Digest) -> io::Result<(Box<dyn Read + Send>, Option<u64>)>;
}

/// Why an image could not be fetched. The messages say how to fix it.
#[derive(Debug)]
pub enum FetchError {
    /// The source could not open or read the image.
    Source { digest: Digest, error: io::Error },
    /// Writing to the cache failed.
    Cache { path: PathBuf, error: io::Error },
    /// The download ended early or ran long.
    Size {
        digest: Digest,
        expected: u64,
        got: u64,
    },
    /// The bytes do not hash to the digest.
    Mismatch { expected: Digest, got: Digest },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Source { digest, error } => write!(
                f,
                "could not download image {}: {error}\nCheck your connection and try again",
                digest.short()
            ),
            FetchError::Cache { path, error } => write!(
                f,
                "could not write {}: {error}\nCheck free disk space with cave gc",
                path.display()
            ),
            FetchError::Size {
                digest,
                expected,
                got,
            } => write!(
                f,
                "the download of image {} has {got} bytes, not {expected}\nTry again; nothing was kept",
                digest.short()
            ),
            FetchError::Mismatch { expected, got } => write!(
                f,
                "the downloaded image does not match its pinned digest: expected {expected}, got {got}\nNothing was kept. If this repeats, the download source has changed; do not unpin the image without checking why"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// Copy buffer size.
const CHUNK: usize = 1 << 20;

/// Folder for partial downloads, inside the cache folder.
const PARTIAL_DIR: &str = ".partial";

/// Distinguishes partial files of fetches in one process.
static NEXT_PARTIAL: AtomicU64 = AtomicU64::new(0);

/// The image cache folder.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// The cache in `dir`, usually `<data directory>/images`. Nothing is
    /// created until the first fetch.
    pub fn new(dir: impl Into<PathBuf>) -> Cache {
        Cache { dir: dir.into() }
    }

    /// Where the image with `digest` is, or will be, stored.
    pub fn path(&self, digest: &Digest) -> PathBuf {
        self.dir.join(format!("sha256-{}.img", digest.hex()))
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.path(digest).is_file()
    }

    /// Makes sure the image with `digest` is in the cache, downloading it
    /// from `source` if needed, and returns its path. `progress` is called
    /// with the bytes done and the total, if known.
    pub fn fetch(
        &self,
        digest: &Digest,
        source: &dyn Source,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<PathBuf, FetchError> {
        let path = self.path(digest);
        if path.is_file() {
            return Ok(path);
        }
        let partial_dir = self.dir.join(PARTIAL_DIR);
        fs::create_dir_all(&partial_dir).map_err(|error| FetchError::Cache {
            path: partial_dir.clone(),
            error,
        })?;
        let partial = Partial(partial_dir.join(format!(
            "sha256-{}.{}.{}",
            digest.hex(),
            std::process::id(),
            NEXT_PARTIAL.fetch_add(1, Ordering::Relaxed)
        )));
        let (reader, size) = source.open(digest).map_err(|error| FetchError::Source {
            digest: *digest,
            error,
        })?;
        let (got, written) = download(reader, &partial.0, size, digest, progress)?;
        if let Some(expected) = size.filter(|&s| s != written) {
            return Err(FetchError::Size {
                digest: *digest,
                expected,
                got: written,
            });
        }
        if got != *digest {
            return Err(FetchError::Mismatch {
                expected: *digest,
                got,
            });
        }
        let cache_err = |error| FetchError::Cache {
            path: path.clone(),
            error,
        };
        fs::set_permissions(&partial.0, fs::Permissions::from_mode(0o444)).map_err(cache_err)?;
        fs::rename(&partial.0, &path).map_err(cache_err)?;
        Ok(path)
    }

    /// Deletes partial downloads left by a crash. The daemon calls this at
    /// startup, before any fetch.
    pub fn remove_partials(&self) -> io::Result<()> {
        match fs::remove_dir_all(self.dir.join(PARTIAL_DIR)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// A partial download, deleted unless it was renamed into place.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Copies `reader` to `path`, hashing as it goes. Returns the digest and
/// the bytes written. Stops early once more than `size` bytes arrive.
fn download(
    mut reader: Box<dyn Read + Send>,
    path: &Path,
    size: Option<u64>,
    digest: &Digest,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(Digest, u64), FetchError> {
    let cache_err = |error| FetchError::Cache {
        path: path.to_path_buf(),
        error,
    };
    let mut file = File::create(path).map_err(cache_err)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(FetchError::Source {
                    digest: *digest,
                    error,
                });
            }
        };
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(cache_err)?;
        written += n as u64;
        progress(written, size);
        if size.is_some_and(|s| written > s) {
            break;
        }
    }
    file.sync_all().map_err(cache_err)?;
    Ok((Digest::from_bytes(hasher.finalize().into()), written))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A source serving images from memory, optionally failing after some
    /// bytes or lying about the size.
    #[derive(Default)]
    struct Memory {
        images: Vec<(Digest, Vec<u8>)>,
        fail_after: Option<usize>,
        claimed_size: Option<u64>,
        opens: Mutex<u32>,
    }

    struct FailingReader {
        data: io::Cursor<Vec<u8>>,
        fail_after: Option<usize>,
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let pos = self.data.position() as usize;
            if let Some(limit) = self.fail_after {
                if pos >= limit {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
                }
                let n = buf.len().min(limit - pos).min(4096);
                return self.data.read(&mut buf[..n]);
            }
            let n = buf.len().min(4096);
            self.data.read(&mut buf[..n])
        }
    }

    impl Source for Memory {
        fn open(&self, digest: &Digest) -> io::Result<(Box<dyn Read + Send>, Option<u64>)> {
            *self.opens.lock().unwrap() += 1;
            let (_, data) = self
                .images
                .iter()
                .find(|(d, _)| d == digest)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such image"))?;
            let size = self.claimed_size.unwrap_or(data.len() as u64);
            Ok((
                Box::new(FailingReader {
                    data: io::Cursor::new(data.clone()),
                    fail_after: self.fail_after,
                }),
                Some(size),
            ))
        }
    }

    fn digest_of(data: &[u8]) -> Digest {
        Digest::from_bytes(Sha256::digest(data).into())
    }

    fn image(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7) as u8).collect()
    }

    fn temp_cache(name: &str) -> Cache {
        let dir =
            std::env::temp_dir().join(format!("cave-image-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Cache::new(dir)
    }

    fn partials(cache: &Cache) -> usize {
        fs::read_dir(cache.dir.join(PARTIAL_DIR))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    fn no_progress() -> impl FnMut(u64, Option<u64>) {
        |_, _| {}
    }

    #[test]
    fn known_sha256_vector() {
        assert_eq!(
            digest_of(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fetch_stores_a_verified_read_only_image_once() {
        let cache = temp_cache("ok");
        let data = image(3 << 20);
        let digest = digest_of(&data);
        let source = Memory {
            images: vec![(digest, data.clone())],
            ..Memory::default()
        };
        let mut seen = Vec::new();
        let path = cache
            .fetch(&digest, &source, &mut |done, total| {
                seen.push((done, total))
            })
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), data);
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(
            seen.last(),
            Some(&(data.len() as u64, Some(data.len() as u64)))
        );
        assert!(cache.contains(&digest));
        assert_eq!(partials(&cache), 0);

        cache.fetch(&digest, &source, &mut no_progress()).unwrap();
        assert_eq!(
            *source.opens.lock().unwrap(),
            1,
            "cached, not downloaded again"
        );
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn interrupted_download_keeps_nothing_and_the_next_one_works() {
        let cache = temp_cache("partial");
        let data = image(100_000);
        let digest = digest_of(&data);
        let mut source = Memory {
            images: vec![(digest, data.clone())],
            fail_after: Some(40_000),
            ..Memory::default()
        };
        let err = cache
            .fetch(&digest, &source, &mut no_progress())
            .unwrap_err();
        assert!(matches!(err, FetchError::Source { .. }), "{err}");
        assert!(err.to_string().contains("try again"));
        assert!(!cache.contains(&digest));
        assert_eq!(partials(&cache), 0);

        source.fail_after = None;
        let path = cache.fetch(&digest, &source, &mut no_progress()).unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn short_and_long_downloads_are_refused() {
        let cache = temp_cache("size");
        let data = image(10_000);
        let digest = digest_of(&data);
        for claimed in [20_000, 5_000] {
            let source = Memory {
                images: vec![(digest, data.clone())],
                claimed_size: Some(claimed),
                ..Memory::default()
            };
            let err = cache
                .fetch(&digest, &source, &mut no_progress())
                .unwrap_err();
            assert!(
                matches!(err, FetchError::Size { expected, .. } if expected == claimed),
                "{err}"
            );
            assert!(!cache.contains(&digest));
            assert_eq!(partials(&cache), 0);
        }
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn digest_mismatch_is_refused_and_nothing_is_kept() {
        let cache = temp_cache("mismatch");
        let pinned = digest_of(b"the image that was pinned");
        let source = Memory {
            images: vec![(pinned, b"something else".to_vec())],
            ..Memory::default()
        };
        let err = cache
            .fetch(&pinned, &source, &mut no_progress())
            .unwrap_err();
        match &err {
            FetchError::Mismatch { expected, got } => {
                assert_eq!(*expected, pinned);
                assert_eq!(*got, digest_of(b"something else"));
            }
            other => panic!("{other}"),
        }
        assert!(err.to_string().contains("pinned digest"));
        assert!(!cache.contains(&pinned));
        assert_eq!(partials(&cache), 0);
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn missing_image_is_a_source_error() {
        let cache = temp_cache("missing");
        let err = cache
            .fetch(&digest_of(b"x"), &Memory::default(), &mut no_progress())
            .unwrap_err();
        assert!(matches!(err, FetchError::Source { .. }));
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn leftover_partials_are_removed_at_startup() {
        let cache = temp_cache("leftover");
        cache.remove_partials().unwrap();
        let dir = cache.dir.join(PARTIAL_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("sha256-dead.1.0"), b"half an image").unwrap();
        cache.remove_partials().unwrap();
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&cache.dir);
    }
}
