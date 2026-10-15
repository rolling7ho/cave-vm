//! The image update detector (design, "Image updates"; P6.7). When a VM
//! starts in a project whose `cave.toml` leaves `[updates] check` on, the
//! daemon reads the image manifest at most once a day, in the background,
//! and records the newest digest of each image. `cave doctor` reads the
//! record and says when a cached image has a newer version. No image is
//! ever updated by itself. The record also keeps the newest cave release,
//! which `cave` installs by itself (D45).
//!
//! The lookup is a plain GET of the manifest (D28) with no query, cookies
//! or other identifying data, the only lookup cave makes on its own. With
//! `check = false` the detector is not called.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::image::manifest::Manifest;

/// The record's file name, in the daemon's data folder.
pub const FILE: &str = "updates.json";

/// How long a lookup counts as fresh, in seconds.
pub const INTERVAL: u64 = 24 * 60 * 60;

/// Reads the manifest's text. The daemon's reads it from the release, and
/// tests inject their own.
pub type Fetch = Box<dyn Fn() -> io::Result<String> + Send + Sync>;

pub struct Detector {
    inner: Arc<Inner>,
    running: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    file: PathBuf,
    fetch: Fetch,
    /// When this daemon last tried, so an offline Mac is not asked again
    /// on every start.
    tried: Mutex<Option<u64>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Detector {
    /// A detector that keeps its record in `file`.
    pub fn new(file: PathBuf, fetch: Fetch) -> Detector {
        Detector {
            inner: Arc::new(Inner {
                file,
                fetch,
                tried: Mutex::new(None),
            }),
            running: Mutex::new(None),
        }
    }

    /// Looks up the newest images in the background if `enabled` and the
    /// last lookup is older than [`INTERVAL`]. Does nothing at all when not
    /// `enabled`.
    pub fn check_soon(&self, enabled: bool) {
        if !enabled {
            return;
        }
        let mut running = self.running.lock().expect("updates lock");
        if running.as_ref().is_some_and(|r| !r.is_finished()) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        *running = Some(thread::spawn(move || inner.check(now_unix())));
    }

    /// Waits for a lookup [`check_soon`](Self::check_soon) started, in tests.
    pub fn wait(&self) {
        if let Some(r) = self.running.lock().expect("updates lock").take() {
            let _ = r.join();
        }
    }
}

impl Inner {
    fn check(&self, now: u64) {
        {
            let mut tried = self.tried.lock().expect("updates lock");
            let last = tried.into_iter().chain(checked(&self.file)).max();
            if last.is_some_and(|t| now.saturating_sub(t) < INTERVAL) {
                return;
            }
            *tried = Some(now);
        }
        // Best effort: a failed lookup is tried again a day later.
        let Ok(text) = (self.fetch)() else {
            return;
        };
        let Ok(manifest) = Manifest::parse(&text) else {
            return;
        };
        let newest: BTreeMap<String, String> = manifest
            .newest()
            .into_iter()
            .map(|e| (e.name.clone(), e.disk.digest.to_string()))
            .collect();
        let mut doc = json!({ "checked": now, "newest": newest });
        if let Some(cave) = manifest.cave {
            doc["cave"] = json!(cave);
        }
        let tmp = self.file.with_extension("json.tmp");
        if fs::write(&tmp, doc.to_string() + "\n").is_ok() {
            let _ = fs::rename(&tmp, &self.file);
        }
    }
}

fn read(file: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()
}

/// When the recorded lookup was made.
fn checked(file: &Path) -> Option<u64> {
    read(file)?.get("checked")?.as_u64()
}

/// The newest disk digest of each image, by name, as last recorded; empty
/// if there is no record.
pub fn newest(file: &Path) -> BTreeMap<String, String> {
    read(file)
        .and_then(|doc| {
            let map = doc.get("newest")?.as_object()?.clone();
            Some(
                map.into_iter()
                    .filter_map(|(name, digest)| Some((name, digest.as_str()?.to_string())))
                    .collect(),
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
