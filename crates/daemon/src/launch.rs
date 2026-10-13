//! Starting VMM processes.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A VMM process and the daemon's end of its control link.
pub struct Launched {
    pub child: Child,
    pub control: UnixStream,
}

/// Starts VMM processes, so tests can start a fake one.
pub trait Launcher: Send + Sync {
    /// Starts a VMM whose runtime state lives in `vm_dir`.
    fn launch(&self, vm_dir: &Path) -> io::Result<Launched>;

    /// The executable's file name, to recognize a VMM left running by an
    /// earlier daemon.
    fn program_name(&self) -> Option<String>;
}

/// Runs a VMM executable with its control link as standard input and its
/// output in `vmm.log`.
#[derive(Debug, Clone)]
pub struct ProcessLauncher {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub envs: Vec<(OsString, OsString)>,
}

impl ProcessLauncher {
    pub fn new(program: impl Into<PathBuf>) -> ProcessLauncher {
        ProcessLauncher {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }
}

/// The file the VMM's standard output and error go to.
pub const LOG_FILE: &str = "vmm.log";

impl Launcher for ProcessLauncher {
    fn launch(&self, vm_dir: &Path) -> io::Result<Launched> {
        let (control, vmm_end) = UnixStream::pair()?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(vm_dir.join(LOG_FILE))?;
        let child = Command::new(&self.program)
            .args(&self.args)
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(OwnedFd::from(vmm_end)))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        Ok(Launched { child, control })
    }

    fn program_name(&self) -> Option<String> {
        self.program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }
}
