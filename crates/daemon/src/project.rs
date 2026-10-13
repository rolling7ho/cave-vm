//! Preparing a project's VM to boot: read `cave.toml`, fetch the image and
//! its kernel into the shared cache, pin the image on first use (D22), and
//! create the project disk in `.cave/` as a clone of the image.
//!
//! The manifest is read only when the image is not pinned yet or its kernel
//! is not known: once pinned, the kernel's digest is kept next to the disk
//! image in the cache, so a pinned project starts with no network.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cave_vm_proto::daemon::{ImageInfo, NetMode, Policy, PortMap, VmId};
use cave_vm_proto::vmm::VmmConfig;
use cave_vm_proto::{ErrorCode, ErrorMessage};
use cave_vm_vmm::disk;

use crate::config;
use crate::image::manifest::{Entry, Manifest, Origin};
use crate::image::{BASE_ALIAS_TARGET, Cache, Digest, ImageRef};
use crate::snapshot;
use crate::vms::{Prepared, VmSource};

/// The project's machine-specific folder.
pub const STATE_DIR: &str = ".cave";

/// Reads projects from disk, and images from `origin` into `cache`.
pub struct ProjectSource {
    pub cache: Cache,
    pub origin: Origin,
}

/// An error whose text is a message, a newline and how to fix it, as the
/// image and config errors are written.
fn error(code: ErrorCode, text: impl std::fmt::Display) -> ErrorMessage {
    let text = text.to_string();
    match text.split_once('\n') {
        Some((message, fix)) => ErrorMessage::new(code, message).with_fix(fix),
        None => ErrorMessage::new(code, text),
    }
}

fn config_error(e: config::ConfigError) -> ErrorMessage {
    ErrorMessage::new(ErrorCode::BadRequest, e.to_string()).with_fix(e.fix)
}

impl ProjectSource {
    /// The kernel digest recorded for the disk image `disk`.
    fn kernel_record(&self, disk: &Digest) -> PathBuf {
        self.cache.path(disk).with_extension("kernel")
    }

    fn known_kernel(&self, disk: &Digest) -> Option<Digest> {
        let text = fs::read_to_string(self.kernel_record(disk)).ok()?;
        format!("sha256:{}", text.trim()).parse().ok()
    }

    fn read_manifest(&self) -> Result<Manifest, ErrorMessage> {
        let text = self.origin.manifest().map_err(|e| {
            ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot read the image manifest: {e}"),
            )
            .with_fix("Check your connection and try again")
        })?;
        Manifest::parse(&text).map_err(|e| error(ErrorCode::Internal, e))
    }

    /// Makes sure the image and its kernel are cached, and returns the image
    /// pinned to its digest with the paths of both.
    fn fetch(&self, image: &ImageRef) -> Result<(ImageRef, PathBuf, PathBuf), ErrorMessage> {
        if let Some(digest) = image.digest
            && let Some(kernel) = self.known_kernel(&digest)
            && self.cache.contains(&digest)
            && self.cache.contains(&kernel)
        {
            return Ok((
                image.clone(),
                self.cache.path(&digest),
                self.cache.path(&kernel),
            ));
        }
        let manifest = self.read_manifest()?;
        let entry = manifest
            .resolve(image)
            .map_err(|e| error(ErrorCode::NotFound, e))?;
        let source = self.origin.source(&manifest);
        let fetch = |digest: &Digest| {
            self.cache
                .fetch(digest, &source, &mut |_, _| {})
                .map_err(|e| error(ErrorCode::Io, e))
        };
        let disk = fetch(&entry.disk.digest)?;
        let kernel = fetch(&entry.kernel.digest)?;
        // Best effort: without the record the next start reads the manifest.
        let _ = fs::write(
            self.kernel_record(&entry.disk.digest),
            entry.kernel.digest.hex(),
        );
        Ok((image.pinned(entry.disk.digest), disk, kernel))
    }
}

/// Writes the pin into `cave.toml`, keeping the rest of the file.
fn pin(project: &Path, image: &ImageRef) -> Result<(), ErrorMessage> {
    let file = project.join(config::FILE);
    let io_error = |e: io::Error| {
        ErrorMessage::new(
            ErrorCode::Io,
            format!("cannot pin the image in {}: {e}", file.display()),
        )
    };
    let text = fs::read_to_string(&file).map_err(io_error)?;
    let pinned = config::pin_image(&text, &image.to_string()).map_err(config_error)?;
    fs::write(&file, pinned).map_err(io_error)
}

/// Creates the project disk on first boot, and grows it when `cave.toml`
/// asks for more room. A smaller size is left alone: disks do not shrink.
fn project_disk(base: &Path, state: &Path, size: u64) -> Result<PathBuf, ErrorMessage> {
    let path = disk::path(state);
    let disk_error = |e: disk::DiskError| error(ErrorCode::Io, e);
    match fs::metadata(&path) {
        Ok(meta) if meta.len() < size => disk::grow(&path, size).map_err(disk_error)?,
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(state).map_err(|e| {
                ErrorMessage::new(
                    ErrorCode::Io,
                    format!("cannot create {}: {e}", state.display()),
                )
            })?;
            disk::create(base, state, size).map_err(disk_error)?;
        }
        Err(e) => {
            return Err(ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    }
    Ok(path)
}

fn absolute(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The disk digest each project uses: its pin, or for an unpinned project
/// the newest version of its image, which it pins on its next start.
fn project_digests(manifest: &Manifest, projects: &[PathBuf]) -> Vec<Digest> {
    projects
        .iter()
        .filter_map(|project| {
            let image: ImageRef = config::load(project).ok()?.image.parse().ok()?;
            match image.digest {
                Some(digest) => Some(digest),
                None => manifest.resolve(&image).ok().map(|e| e.disk.digest),
            }
        })
        .collect()
}

/// The rows of `cave images`: the newest version of each image, and any
/// older version that is downloaded or in use, marked with the update.
fn image_rows(
    manifest: &Manifest,
    used: &[Digest],
    downloaded: impl Fn(&Entry) -> bool,
) -> Vec<ImageInfo> {
    let newest = manifest.newest();
    manifest
        .images
        .iter()
        .filter_map(|entry| {
            let latest = newest.iter().find(|n| n.name == entry.name)?;
            let is_latest = std::ptr::eq(*latest, entry);
            let used_by = used.iter().filter(|d| **d == entry.disk.digest).count();
            let downloaded = downloaded(entry);
            if !is_latest && used_by == 0 && !downloaded {
                return None;
            }
            Some(ImageInfo {
                name: entry.name.clone(),
                digest: entry.disk.digest.to_string(),
                downloaded,
                used_by: u32::try_from(used_by).unwrap_or(u32::MAX),
                base: is_latest && entry.name == BASE_ALIAS_TARGET,
                update: (!is_latest).then(|| latest.disk.digest.to_string()),
            })
        })
        .collect()
}

impl VmSource for ProjectSource {
    fn prepare(
        &self,
        vm: &VmId,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Prepared, ErrorMessage> {
        let project = Path::new(&vm.project);
        if !project.is_absolute() {
            return Err(ErrorMessage::new(
                ErrorCode::BadRequest,
                format!("{} is not an absolute path", vm.project),
            ));
        }
        let settings = config::load(project).map_err(config_error)?;
        let image: ImageRef = settings
            .image
            .parse()
            .map_err(|e| error(ErrorCode::BadRequest, e))?;
        let (pinned, base, kernel) = self.fetch(&image)?;
        if image.digest.is_none() {
            pin(project, &pinned)?;
        }
        // A fork's disk is in its folder, made when it was forked.
        let disk_dir = match &vm.fork {
            None => project.join(STATE_DIR),
            Some(name) => {
                let dir = snapshot::fork_dir(&project.join(STATE_DIR), name);
                if snapshot::check_name(name).is_err() || !disk::path(&dir).is_file() {
                    return Err(ErrorMessage::new(
                        ErrorCode::NotFound,
                        format!("no fork named {name} in this project"),
                    )
                    .with_fix("List them with `cave ls`, or make one with `cave fork`"));
                }
                dir
            }
        };
        let disk = project_disk(&base, &disk_dir, settings.disk_bytes)?;

        let net_mode = net.unwrap_or(settings.net);
        let mut widened = Vec::new();
        if net_mode == NetMode::Lan {
            widened.push("--net lan".to_string());
        }
        widened.extend(ports.iter().map(|&p| crate::forward::flag(p)));
        let name = vm.fork.clone().unwrap_or_else(|| {
            project
                .file_name()
                .map_or_else(|| vm.project.clone(), |n| n.to_string_lossy().into_owned())
        });
        Ok(Prepared {
            config: VmmConfig {
                kernel: absolute(&kernel),
                disk: absolute(&disk),
                memory_bytes: settings.memory_bytes,
                cpus: settings.cpus,
                net: net_mode,
                restore: None,
            },
            policy: Policy {
                name,
                image: pinned.to_string(),
                storage: settings.storage,
                net: net_mode,
                ports: ports.to_vec(),
                cpus: settings.cpus,
                memory_bytes: settings.memory_bytes,
                ttl_seconds: settings.ttl.map(|t| t.as_secs()),
                widened,
            },
            idle_suspend: settings.idle_suspend,
        })
    }

    fn images(&self, projects: &[PathBuf]) -> Result<Vec<ImageInfo>, ErrorMessage> {
        let manifest = self.read_manifest()?;
        let used = project_digests(&manifest, projects);
        Ok(image_rows(&manifest, &used, |entry| {
            self.cache.contains(&entry.disk.digest) && self.cache.contains(&entry.kernel.digest)
        }))
    }
}

#[cfg(test)]
mod tests;
