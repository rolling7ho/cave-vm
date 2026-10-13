//! Starting VMM processes.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use cave_vm_proto::vmm::VmmConfig;

/// A VMM process and the daemon's end of its control link.
pub struct Launched {
    pub child: Child,
    pub control: UnixStream,
}

/// Starts VMM processes, so tests can start a fake one.
pub trait Launcher: Send + Sync {
    /// Starts a VMM whose runtime state lives in `vm_dir`, to boot `config`.
    fn launch(&self, vm_dir: &Path, config: &VmmConfig) -> io::Result<Launched>;

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
    /// The shared image cache. When set, the VMM runs under the sandbox
    /// profile in `vmm.sb` and can reach only its project's `.cave/` and
    /// this folder (design, "Security").
    pub sandbox: Option<PathBuf>,
}

/// The VMM's sandbox profile.
pub const SANDBOX_PROFILE: &str = include_str!("vmm.sb");

/// macOS's sandbox runner.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

impl ProcessLauncher {
    pub fn new(program: impl Into<PathBuf>) -> ProcessLauncher {
        ProcessLauncher {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            sandbox: None,
        }
    }
}

/// The file the VMM's standard output and error go to.
pub const LOG_FILE: &str = "vmm.log";

impl Launcher for ProcessLauncher {
    fn launch(&self, vm_dir: &Path, config: &VmmConfig) -> io::Result<Launched> {
        let (control, vmm_end) = UnixStream::pair()?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(vm_dir.join(LOG_FILE))?;
        let mut command = match &self.sandbox {
            Some(images) => self.sandboxed(images, config)?,
            None => Command::new(&self.program),
        };
        let child = command
            .args(&self.args)
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(OwnedFd::from(vmm_end)))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        Ok(Launched { child, control })
    }

    // The VMM keeps the process (sandbox-exec execs it), so its name is
    // still the program's.
    fn program_name(&self) -> Option<String> {
        self.program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }
}

impl ProcessLauncher {
    /// `sandbox-exec` with the profile, ready for the VMM's arguments. The
    /// project's `.cave/` folder is the one holding the disk.
    fn sandboxed(&self, images: &Path, config: &VmmConfig) -> io::Result<Command> {
        let state = Path::new(&config.disk).parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the disk path has no folder")
        })?;
        // Sandbox rules match resolved paths: /tmp is /private/tmp.
        let real = |p: &Path| p.canonicalize();
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(SANDBOX_PROFILE);
        for (name, path) in [
            ("VMM", real(&self.program)?),
            ("PROJECT_STATE", real(state)?),
            ("IMAGE_CACHE", real(images)?),
            ("KERNEL", real(Path::new(&config.kernel))?),
        ] {
            let mut define = OsString::from(format!("{name}="));
            define.push(path);
            command.arg("-D").arg(define);
        }
        command.arg(&self.program);
        Ok(command)
    }
}
