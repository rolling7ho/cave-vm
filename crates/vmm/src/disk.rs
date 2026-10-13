//! The project disk (ROADMAP P3.4): a raw sparse image in `.cave/`, created
//! as an APFS clone of the shared base image so it costs almost nothing
//! until the guest writes to it [A9].
//!
//! `.cave/` holds the disk under a fixed relative name and no absolute
//! paths, so moving the project folder keeps it working. Growing the file
//! only adds sparse space at the end; the guest grows its file system into
//! it at boot.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::host;

/// The design's default disk size.
pub const DEFAULT_SIZE: u64 = 20 << 30;

/// The disk's name inside `.cave/`.
pub const DISK_FILE: &str = "disk.img";

/// Why a project disk could not be created or grown. The messages say how
/// to fix the problem.
#[derive(Debug)]
pub enum DiskError {
    /// The project folder is not on APFS, or not on the same volume as the
    /// image cache, so the base image cannot be cloned.
    CannotClone {
        project: PathBuf,
    },
    AlreadyExists(PathBuf),
    BaseMissing(PathBuf),
    /// The requested size is below the base image or the current disk.
    TooSmall {
        requested: u64,
        minimum: u64,
    },
    Io {
        path: PathBuf,
        error: io::Error,
    },
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskError::CannotClone { project } => write!(
                f,
                "cannot clone the base image into {}: the folder must be on an APFS volume, the same one as your home folder\nMove the project to a folder on your Mac's main disk",
                project.display()
            ),
            DiskError::AlreadyExists(path) => write!(
                f,
                "{} already exists\nRemove the VM with cave rm first if you want a fresh disk",
                path.display()
            ),
            DiskError::BaseMissing(path) => write!(
                f,
                "the base image {} is missing\nRun cave doctor, which fetches it again",
                path.display()
            ),
            DiskError::TooSmall { requested, minimum } => write!(
                f,
                "a disk of {requested} bytes is smaller than the {minimum} bytes it already needs\nChoose a larger size; disks can grow but not shrink"
            ),
            DiskError::Io { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for DiskError {}

/// The project disk's path inside `cave_dir` (a project's `.cave/`).
pub fn path(cave_dir: &Path) -> PathBuf {
    cave_dir.join(DISK_FILE)
}

/// Creates the project disk in `cave_dir` as a clone of `base`, sized to
/// `size` bytes, and returns its path.
pub fn create(base: &Path, cave_dir: &Path, size: u64) -> Result<PathBuf, DiskError> {
    let disk = path(cave_dir);
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |error| DiskError::Io { path, error }
    };
    let base_size = match fs::metadata(base) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(DiskError::BaseMissing(base.to_path_buf()));
        }
        Err(e) => return Err(io_err(base)(e)),
    };
    if size < base_size {
        return Err(DiskError::TooSmall {
            requested: size,
            minimum: base_size,
        });
    }
    match host::clonefile(base, &disk) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(DiskError::AlreadyExists(disk));
        }
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            return Err(DiskError::CannotClone {
                project: cave_dir.parent().unwrap_or(cave_dir).to_path_buf(),
            });
        }
        Err(e) => return Err(io_err(&disk)(e)),
    }
    // The clone inherits the cached image's read-only mode.
    let finish = || -> io::Result<()> {
        fs::set_permissions(&disk, fs::Permissions::from_mode(0o644))?;
        OpenOptions::new().write(true).open(&disk)?.set_len(size)
    };
    if let Err(e) = finish() {
        let _ = fs::remove_file(&disk);
        return Err(io_err(&disk)(e));
    }
    Ok(disk)
}

/// Grows the disk at `disk` to `size` bytes. Shrinking is refused, since it
/// would cut off the guest's file system.
pub fn grow(disk: &Path, size: u64) -> Result<(), DiskError> {
    let io_err = |error| DiskError::Io {
        path: disk.to_path_buf(),
        error,
    };
    let file = OpenOptions::new().write(true).open(disk).map_err(io_err)?;
    let current = file.metadata().map_err(io_err)?.len();
    if size < current {
        return Err(DiskError::TooSmall {
            requested: size,
            minimum: current,
        });
    }
    file.set_len(size).map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("cave-disk-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A read-only base image of `len` bytes that do not compress or dedupe.
    fn base_image(dir: &Path, len: usize) -> PathBuf {
        let path = dir.join("base.img");
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        fs::write(&path, data).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        path
    }

    fn cave_dir(project: &Path) -> PathBuf {
        let cave = project.join(".cave");
        fs::create_dir_all(&cave).unwrap();
        cave
    }

    #[test]
    fn disk_is_a_writable_sparse_clone_of_the_base() {
        let t = Temp::new("create");
        let base = base_image(&t.0, 1 << 20);
        let cave = cave_dir(&t.0.join("project"));
        let disk = create(&base, &cave, DEFAULT_SIZE).unwrap();
        assert_eq!(disk, cave.join("disk.img"));

        let meta = fs::metadata(&disk).unwrap();
        assert_eq!(meta.len(), DEFAULT_SIZE);
        assert!(meta.blocks() * 512 < 64 << 20, "sparse, not 20 GB on disk");
        assert!(!meta.permissions().readonly());

        let base_bytes = fs::read(&base).unwrap();
        let mut start = vec![0u8; base_bytes.len()];
        std::os::unix::fs::FileExt::read_exact_at(&fs::File::open(&disk).unwrap(), &mut start, 0)
            .unwrap();
        assert_eq!(start, base_bytes);

        let file = OpenOptions::new().write(true).open(&disk).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, b"guest data", 0).unwrap();
        assert_eq!(fs::read(&base).unwrap(), base_bytes, "base untouched");
    }

    #[test]
    fn clone_shares_blocks_until_written() {
        let t = Temp::new("shared");
        let base = base_image(&t.0, 64 << 20);
        let cave = cave_dir(&t.0.join("project"));
        let before = host::available_space(&t.0).unwrap();
        create(&base, &cave, 64 << 20).unwrap();
        let after = host::available_space(&t.0).unwrap();
        let used = before.saturating_sub(after);
        assert!(used < 16 << 20, "cloning 64 MiB used {} MiB", used >> 20);
    }

    #[test]
    fn moving_the_project_folder_keeps_the_disk_working() {
        let t = Temp::new("move");
        let base = base_image(&t.0, 1 << 20);
        let (old, new) = (t.0.join("old-name"), t.0.join("new-name"));
        create(&base, &cave_dir(&old), 4 << 20).unwrap();
        fs::rename(&old, &new).unwrap();

        let cave = new.join(".cave");
        let names: Vec<_> = fs::read_dir(&cave)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["disk.img"], "nothing else that could hold a path");
        let disk = path(&cave);
        assert_eq!(
            fs::read(&disk).unwrap()[..16],
            fs::read(&base).unwrap()[..16]
        );
        grow(&disk, 8 << 20).unwrap();
        assert_eq!(fs::metadata(&disk).unwrap().len(), 8 << 20);
    }

    #[test]
    fn errors_say_what_to_do() {
        let t = Temp::new("errors");
        let base = base_image(&t.0, 1 << 20);
        let cave = cave_dir(&t.0.join("project"));

        let err = create(&t.0.join("missing.img"), &cave, DEFAULT_SIZE).unwrap_err();
        assert!(matches!(err, DiskError::BaseMissing(_)), "{err}");
        assert!(err.to_string().contains("cave doctor"));

        let err = create(&base, &cave, 1024).unwrap_err();
        assert!(matches!(err, DiskError::TooSmall { .. }), "{err}");
        assert!(!path(&cave).exists());

        let disk = create(&base, &cave, 4 << 20).unwrap();
        let err = create(&base, &cave, 4 << 20).unwrap_err();
        assert!(matches!(err, DiskError::AlreadyExists(_)), "{err}");
        assert!(err.to_string().contains("cave rm"));

        let err = grow(&disk, 2 << 20).unwrap_err();
        assert!(matches!(err, DiskError::TooSmall { .. }), "{err}");
        assert!(err.to_string().contains("can grow but not shrink"));

        let not_apfs = DiskError::CannotClone {
            project: PathBuf::from("/Volumes/USB/p"),
        };
        assert!(not_apfs.to_string().contains("APFS"));
    }
}
