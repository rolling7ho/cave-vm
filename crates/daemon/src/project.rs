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
use crate::progress;
use crate::snapshot;
use crate::updates::Detector;
use crate::vms::{AutoSnapshots, Prepared, VmSource};

/// The project's machine-specific folder.
pub const STATE_DIR: &str = ".cave";

/// The marker in `.cave/` for the first-boot copy (D13): set when the
/// project disk is made, cleared once the folder's files are in the VM.
pub const COPY_PENDING_FILE: &str = "copy-pending";

/// The extension of the file next to a cached disk image that holds the
/// image's name.
pub const NAME_RECORD: &str = "name";

/// The extension of the file next to a cached disk image that holds its
/// kernel's digest.
pub const KERNEL_RECORD: &str = "kernel";

/// Reads projects from disk, and images from `origin` into `cache`.
pub struct ProjectSource {
    pub cache: Cache,
    pub origin: Origin,
    /// Looks for newer images when a VM starts, unless the project turns
    /// it off.
    pub updates: Option<Detector>,
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
        self.cache.path(disk).with_extension(KERNEL_RECORD)
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
        // The first run downloads both, which the client shows as progress.
        let fetch = |digest: &Digest, what: String| {
            let mut download = progress::Download::new(what);
            self.cache
                .fetch(digest, &source, &mut |done, total| {
                    download.update(done, total)
                })
                .map_err(|e| error(ErrorCode::Io, e))
        };
        let disk = fetch(&entry.disk.digest, format!("the {} image", entry.name))?;
        let kernel = fetch(
            &entry.kernel.digest,
            format!("the kernel for {}", entry.name),
        )?;
        // Best effort: without the record the next start reads the manifest.
        let _ = fs::write(
            self.kernel_record(&entry.disk.digest),
            entry.kernel.digest.hex(),
        );
        // Best effort too: `cave gc` names the image by it.
        let _ = fs::write(
            self.cache
                .path(&entry.disk.digest)
                .with_extension(NAME_RECORD),
            &entry.name,
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
        .with_fix(format!(
            "Check that you can write to {}, then try again",
            file.display()
        ))
    };
    let text = fs::read_to_string(&file).map_err(io_error)?;
    let pinned = config::pin_image(&text, &image.to_string()).map_err(config_error)?;
    fs::write(&file, pinned).map_err(io_error)
}

/// Creates the project disk on first boot, marking it for the first-boot
/// copy, and grows it when `cave.toml`
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
                .with_fix("Check that you can write to the project folder, then try again")
            })?;
            disk::create(base, state, size).map_err(disk_error)?;
            // A new disk is a first boot: the folder's files go in once.
            fs::write(state.join(COPY_PENDING_FILE), b"").map_err(|e| {
                ErrorMessage::new(
                    ErrorCode::Io,
                    format!("cannot write to {}: {e}", state.display()),
                )
                .with_fix("Check that you can write to the project folder, then try again")
            })?;
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

/// The startup policy of `vm` with `settings`, its image as `image`, and
/// the flags `net` and `ports`.
fn policy(
    vm: &VmId,
    settings: &config::Config,
    image: String,
    net: Option<NetMode>,
    ports: &[PortMap],
) -> Policy {
    let net = net.unwrap_or(settings.net);
    let mut widened = Vec::new();
    if net == NetMode::Lan {
        widened.push("--net lan".to_string());
    }
    widened.extend(ports.iter().map(|&p| crate::forward::flag(p)));
    let project = Path::new(&vm.project);
    let name = vm.fork.clone().unwrap_or_else(|| {
        project
            .file_name()
            .map_or_else(|| vm.project.clone(), |n| n.to_string_lossy().into_owned())
    });
    Policy {
        name,
        image,
        storage: settings.storage,
        net,
        ports: ports.to_vec(),
        cpus: settings.cpus,
        memory_bytes: settings.memory_bytes,
        ttl_seconds: settings.ttl.map(|t| t.as_secs()),
        widened,
    }
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
        if let Some(updates) = &self.updates {
            updates.check_soon(settings.updates_check);
        }
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

        let policy = policy(vm, &settings, pinned.to_string(), net, ports);
        Ok(Prepared {
            config: VmmConfig {
                kernel: absolute(&kernel),
                disk: absolute(&disk),
                memory_bytes: settings.memory_bytes,
                cpus: settings.cpus,
                net: policy.net,
                restore: None,
            },
            policy,
            idle_suspend: settings.idle_suspend,
        })
    }

    fn policy(&self, vm: &VmId) -> Result<Policy, ErrorMessage> {
        let settings = config::load(Path::new(&vm.project)).map_err(config_error)?;
        let image = settings.image.clone();
        Ok(policy(vm, &settings, image, None, &[]))
    }

    fn auto_snapshots(&self, vm: &VmId) -> Result<AutoSnapshots, ErrorMessage> {
        let settings = config::load(Path::new(&vm.project)).map_err(config_error)?;
        Ok(AutoSnapshots {
            auto: settings.snapshots_auto,
            keep: settings.snapshots_keep,
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
