//! `cave gc`: disk usage, and the things that are safe to delete
//! (docs/cli.md).
//!
//! Sizes come from where each file's blocks sit on the volume, so blocks
//! that APFS clones share are counted once, in the first row that uses
//! them [A9]: a project disk cloned from a base image counts only what the
//! VM wrote, and a snapshot only what changed since. What deleting
//! something frees is what no other file still uses.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cave_vm_proto::daemon::{ImageUsage, ProjectUsage, Reclaimable, Usage};
use cave_vm_vmm::host;

use crate::config;
use crate::project::{KERNEL_RECORD, NAME_RECORD, STATE_DIR};
use crate::snapshot::{self, FORKS_DIR, MANIFEST_FILE, SNAPSHOTS_DIR, SUSPEND_DIR};

/// A snapshot folder without a manifest younger than this may still be
/// being written, so it is not offered.
const UNFINISHED_AGE: Duration = Duration::from_secs(10 * 60);

/// What can be reclaimed, in the order `cave gc` lists it.
const UNUSED_IMAGES: &str = "unused images";
const OLD_SNAPSHOTS: &str = "old automatic snapshots";
const UNFINISHED: &str = "unfinished snapshots";

/// A project to survey.
pub struct Project {
    pub path: PathBuf,
    /// Its VM is being started, stopped or snapshotted, so nothing of it is
    /// offered for deletion.
    pub busy: bool,
}

/// Usage, and the files behind each reclaimable entry.
pub struct Survey {
    pub usage: Usage,
    delete: Vec<PathBuf>,
}

impl Survey {
    /// Deletes everything reclaimable and returns the bytes freed. Paths
    /// that cannot be deleted are left; the next `gc` offers them again.
    pub fn apply(mut self) -> Usage {
        for path in &self.delete {
            let _ = match fs::symlink_metadata(path) {
                Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
                Ok(_) => fs::remove_file(path),
                Err(_) => Ok(()),
            };
        }
        self.usage.reclaimed = Some(self.usage.reclaimable.iter().map(|r| r.size).sum());
        self.usage
    }
}

/// Byte ranges on the volume, counted once however often they are added.
#[derive(Default)]
struct Blocks(BTreeMap<u64, u64>);

impl Blocks {
    /// Adds `[start, start + len)` and returns how many of its bytes were
    /// not there yet.
    fn add(&mut self, start: u64, len: u64) -> u64 {
        if len == 0 {
            return 0;
        }
        let end = start.saturating_add(len);
        let (mut lo, mut hi) = (start, end);
        let mut touching: Vec<u64> = Vec::new();
        if let Some((&s, &e)) = self.0.range(..start).next_back()
            && e >= start
        {
            touching.push(s);
        }
        touching.extend(self.0.range(start..=end).map(|(&s, _)| s));
        let mut covered = 0;
        for s in touching {
            let e = self.0.remove(&s).expect("listed");
            covered += e.min(end).saturating_sub(s.max(start));
            lo = lo.min(s);
            hi = hi.max(e);
        }
        self.0.insert(lo, hi);
        len - covered
    }

    fn add_all(&mut self, runs: &[(u64, u64)]) -> u64 {
        runs.iter().map(|&(start, len)| self.add(start, len)).sum()
    }
}

/// Every file's blocks, measured once.
#[derive(Default)]
struct Measured {
    runs: HashMap<PathBuf, Vec<(u64, u64)>>,
    /// Where made-up ranges start, for files whose blocks cannot be
    /// located: past any real volume offset, so they are never shared.
    next_private: u64,
}

impl Measured {
    /// The files under `path` (or `path` itself), measured.
    fn files(&mut self, path: &Path) -> Vec<PathBuf> {
        let files = files_under(path);
        for file in &files {
            if !self.runs.contains_key(file) {
                let runs = self.locate(file);
                self.runs.insert(file.clone(), runs);
            }
        }
        files
    }

    fn locate(&mut self, file: &Path) -> Vec<(u64, u64)> {
        if let Ok(runs) = File::open(file).and_then(|f| host::physical_extents(&f)) {
            return runs;
        }
        // Not on APFS, or not readable: count its blocks as its own.
        let bytes = fs::metadata(file).map_or(0, |m| m.blocks() * 512);
        if self.next_private == 0 {
            self.next_private = 1 << 62;
        }
        let start = self.next_private;
        self.next_private += bytes;
        vec![(start, bytes)]
    }

    /// The bytes the union of every measured file but `except` takes.
    fn total_without(&self, except: &[PathBuf]) -> u64 {
        let mut blocks = Blocks::default();
        self.runs
            .iter()
            .filter(|(path, _)| !except.iter().any(|e| path.starts_with(e)))
            .map(|(_, runs)| blocks.add_all(runs))
            .sum()
    }

    /// The bytes deleting `paths` would free.
    fn freed(&self, paths: &[PathBuf]) -> u64 {
        self.total_without(&[]) - self.total_without(paths)
    }
}

/// Every regular file at or under `path`. Symbolic links are not followed.
fn files_under(path: &Path) -> Vec<PathBuf> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Vec::new();
    };
    if meta.is_file() {
        return vec![path.to_path_buf()];
    }
    if !meta.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(path)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries.iter().flat_map(|p| files_under(p)).collect()
}

/// A cached disk image: `sha256-<hex>.img` with a kernel record next to
/// it, or any other cached file nothing names as a kernel.
struct Image {
    hex: String,
    file: PathBuf,
    kernel: Option<PathBuf>,
    name: String,
}

fn cached_images(images: &Path) -> Vec<Image> {
    let mut files: Vec<PathBuf> = fs::read_dir(images)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    let hex_of = |path: &Path| -> Option<String> {
        let name = path.file_name()?.to_str()?;
        name.strip_prefix("sha256-")?
            .strip_suffix(".img")
            .map(String::from)
    };
    let kernel_of = |file: &Path| -> Option<String> {
        fs::read_to_string(file.with_extension(KERNEL_RECORD))
            .ok()
            .map(|k| k.trim().to_string())
    };
    let kernels: Vec<String> = files.iter().filter_map(|f| kernel_of(f)).collect();
    files
        .iter()
        .filter_map(|file| {
            let hex = hex_of(file)?;
            if kernels.contains(&hex) {
                return None;
            }
            let kernel = kernel_of(file)
                .map(|k| images.join(format!("sha256-{k}.img")))
                .filter(|k| k.is_file());
            let name = fs::read_to_string(file.with_extension(NAME_RECORD))
                .map(|n| n.trim().to_string())
                .unwrap_or_else(|_| format!("sha256:{}", hex.get(..8).unwrap_or(&hex)));
            Some(Image {
                hex,
                file: file.clone(),
                kernel,
                name,
            })
        })
        .collect()
}

/// Snapshot, fork and suspension folders without a manifest, left by a
/// VMM or daemon that stopped while writing them.
fn unfinished(state: &Path, now: SystemTime) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for parent in [state.join(SNAPSHOTS_DIR), state.join(FORKS_DIR)] {
        if let Ok(entries) = fs::read_dir(&parent) {
            dirs.extend(entries.flatten().map(|e| e.path()));
        }
    }
    let forks: Vec<PathBuf> = dirs
        .iter()
        .filter(|d| d.starts_with(state.join(FORKS_DIR)))
        .map(|d| d.join(SUSPEND_DIR))
        .collect();
    dirs.push(state.join(SUSPEND_DIR));
    dirs.extend(forks);
    dirs.sort();
    dirs.into_iter()
        .filter(|dir| {
            let Ok(meta) = fs::symlink_metadata(dir) else {
                return false;
            };
            let old = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= UNFINISHED_AGE);
            meta.is_dir() && old && !dir.join(MANIFEST_FILE).is_file()
        })
        .collect()
}

/// Surveys the image cache in `images` and `projects`.
pub fn survey(images: &Path, projects: &[Project], now: SystemTime) -> Survey {
    let mut measured = Measured::default();
    let mut counted = Blocks::default();
    let mut count = |measured: &mut Measured, paths: &[PathBuf]| -> u64 {
        paths
            .iter()
            .flat_map(|p| measured.files(p))
            .collect::<Vec<_>>()
            .iter()
            .map(|f| counted.add_all(&measured.runs[f]))
            .sum()
    };

    let pinned: Vec<String> = projects
        .iter()
        .filter_map(|p| Some(config::load(&p.path).ok()?.image_digest()?.to_string()))
        .collect();
    let cached = cached_images(images);
    let in_use = |image: &Image| pinned.contains(&image.hex);
    let mut image_rows = Vec::new();
    for image in &cached {
        let files: Vec<PathBuf> = std::iter::once(image.file.clone())
            .chain(image.kernel.clone())
            .collect();
        image_rows.push(ImageUsage {
            name: image.name.clone(),
            digest: format!("sha256:{}", image.hex),
            size: count(&mut measured, &files),
            in_use: in_use(image),
        });
    }

    let mut project_rows = Vec::new();
    let mut old_snapshots = Vec::new();
    let mut leftovers = Vec::new();
    for project in projects {
        let state = project.path.join(STATE_DIR);
        if !state.is_dir() {
            continue;
        }
        let snapshots = [state.join(SNAPSHOTS_DIR), state.join(SUSPEND_DIR)];
        let forks = [state.join(FORKS_DIR)];
        let disk: Vec<PathBuf> = fs::read_dir(&state)
            .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !snapshots.contains(p) && !forks.contains(p))
            .collect();
        let mut disk = disk;
        disk.sort();
        project_rows.push(ProjectUsage {
            project: project.path.to_string_lossy().into_owned(),
            disk: count(&mut measured, &disk),
            snapshots: count(&mut measured, &snapshots),
            forks: count(&mut measured, &forks),
        });
        if project.busy {
            continue;
        }
        let keep = config::load(&project.path).map_or(10, |c| c.snapshots_keep);
        old_snapshots.extend(
            snapshot::beyond_retention(&state, keep)
                .into_iter()
                .map(|name| snapshot::dir(&state, &name)),
        );
        leftovers.extend(unfinished(&state, now));
    }

    // An unused image's kernel goes with it, unless an image in use or
    // kept has the same kernel.
    let mut unused = Vec::new();
    for image in cached.iter().filter(|i| !in_use(i)) {
        unused.push(image.file.clone());
        unused.push(image.file.with_extension(KERNEL_RECORD));
        unused.push(image.file.with_extension(NAME_RECORD));
        if let Some(kernel) = &image.kernel {
            let shared = cached
                .iter()
                .any(|other| in_use(other) && other.kernel.as_ref() == Some(kernel));
            if !shared && !unused.contains(kernel) {
                unused.push(kernel.clone());
            }
        }
    }
    // Each entry is what deleting it frees on top of the entries before
    // it, so blocks they share are counted once and the entries add up to
    // what deleting all of them frees.
    let mut reclaimable = Vec::new();
    let mut delete: Vec<PathBuf> = Vec::new();
    let mut freed = 0;
    for (what, paths) in [
        (UNUSED_IMAGES, unused),
        (OLD_SNAPSHOTS, old_snapshots),
        (UNFINISHED, leftovers),
    ] {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| fs::symlink_metadata(p).is_ok())
            .collect();
        if paths.is_empty() {
            continue;
        }
        for path in &paths {
            measured.files(path);
        }
        delete.extend(paths);
        let now_freed = measured.freed(&delete);
        reclaimable.push(Reclaimable {
            what: what.into(),
            size: now_freed - freed,
        });
        freed = now_freed;
    }
    Survey {
        usage: Usage {
            images: image_rows,
            projects: project_rows,
            reclaimable,
            reclaimed: None,
        },
        delete,
    }
}

#[cfg(test)]
mod tests;
