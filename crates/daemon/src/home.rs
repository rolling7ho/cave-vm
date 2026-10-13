//! The data directory: the daemon socket, the image cache and per-VM state
//! (docs/cli.md, "The daemon and the data directory").

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use cave_vm_proto::daemon::VmId;
use sha2::{Digest, Sha256};

/// Where shared state lives.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

/// Creates `dir` and its parents if needed, readable by this user only.
fn private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// The file in a VM folder naming its project folder, so the daemon can
/// find the projects it has seen after a restart. It lives in the data
/// directory, never in `.cave/`.
pub const PROJECT_FILE: &str = "project";

impl Home {
    /// `CAVE_HOME` if set, otherwise `~/Library/Application Support/cave-vm`.
    pub fn from_env() -> io::Result<Home> {
        let root = match std::env::var_os("CAVE_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = std::env::var_os("HOME")
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
                PathBuf::from(home).join("Library/Application Support/cave-vm")
            }
        };
        Home::at(root)
    }

    /// The data directory at `root`, created readable by this user only.
    pub fn at(root: impl Into<PathBuf>) -> io::Result<Home> {
        let root = root.into();
        private_dir(&root)?;
        Ok(Home { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The daemon API socket.
    pub fn socket(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }

    /// The shared image cache.
    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    /// The folder for one VM's runtime state, created if needed. It is named
    /// by a hash of the project path and fork, so `.cave/` never holds the
    /// absolute path.
    pub fn vm_dir(&self, vm: &VmId) -> io::Result<PathBuf> {
        let mut hash = Sha256::new();
        hash.update(vm.project.as_bytes());
        if let Some(fork) = &vm.fork {
            hash.update([0]);
            hash.update(fork.as_bytes());
        }
        let name: String = hash.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = self.vms().join(name);
        private_dir(&dir)?;
        Ok(dir)
    }

    /// The project folders of every VM this data directory has seen, from
    /// the [`PROJECT_FILE`] in each VM folder, each listed once.
    pub fn projects(&self) -> io::Result<Vec<PathBuf>> {
        let mut projects = Vec::new();
        for dir in self.vm_dirs()? {
            if let Ok(project) = fs::read_to_string(dir.join(PROJECT_FILE)) {
                let project = PathBuf::from(project);
                if !projects.contains(&project) {
                    projects.push(project);
                }
            }
        }
        projects.sort();
        Ok(projects)
    }

    /// Every VM folder.
    pub fn vm_dirs(&self) -> io::Result<Vec<PathBuf>> {
        match fs::read_dir(self.vms()) {
            Ok(entries) => Ok(entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}
