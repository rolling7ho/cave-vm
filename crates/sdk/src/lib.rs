//! Rust SDK over the daemon's socket API.
//!
//! A [`Sandbox`] is a project's VM, or one of its forks, found from a folder
//! the way `cave` finds it. Each call is one request to the daemon, which
//! is started when it is not running.
//!
//! ```no_run
//! use cave_vm_sdk::Sandbox;
//!
//! let sb = Sandbox::open(".")?;
//! sb.snapshot("before")?;
//! let out = sb.exec(["make", "test"])?;
//! if !out.success() {
//!     sb.restore("before")?;
//! }
//! let fork = sb.fork_from("before")?.sandbox;
//! fork.exec(["uname", "-a"])?;
//! fork.destroy()?;
//! # Ok::<(), cave_vm_sdk::Error>(())
//! ```

#![deny(unsafe_code)]

mod client;
mod error;
mod project;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;

use cave_vm_proto::MAX_CHUNK;
use cave_vm_proto::agent::{Exec, Stream};
use cave_vm_proto::daemon::{Reply, Request, VmId};

pub use cave_vm_proto::ErrorCode;
pub use cave_vm_proto::daemon::Policy;
pub use client::Daemon;
pub use error::{Error, Result};

/// A project's VM, or one of its forks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    daemon: Daemon,
    project: PathBuf,
    fork: Option<String>,
}

/// A command to run in the VM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    argv: Vec<String>,
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    stdin: Option<Vec<u8>>,
}

impl Command {
    pub fn new(program: impl Into<String>) -> Command {
        Command {
            argv: vec![program.into()],
            ..Command::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Command {
        self.argv.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.extend(args.into_iter().map(Into::into));
        self
    }

    /// The working directory in the VM; `/workspace` when not set.
    pub fn cwd(mut self, cwd: impl Into<String>) -> Command {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Command {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Bytes for the command's stdin. Without them, stdin is `/dev/null`.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Command {
        self.stdin = Some(data.into());
        self
    }
}

/// What a finished command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// The exit code, when the command exited.
    pub code: Option<i32>,
    /// The signal that ended the command, such as `KILL`, when one did.
    pub signal: Option<String>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    /// Whether the command exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A saved snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    /// Bytes on disk.
    pub size: u64,
}

/// The result of a restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The snapshot restored.
    pub name: String,
    /// `false` when the memory could not be restored, so the VM booted
    /// from the snapshot's disk and its processes were not kept.
    pub memory_restored: bool,
    /// Why the memory was not restored.
    pub reason: Option<String>,
    /// The automatic snapshot of the state before the restore, which
    /// undoes it.
    pub saved: Option<String>,
}

/// The result of a fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    /// The new fork.
    pub sandbox: Sandbox,
    /// The snapshot it was made from, or the VM whose current state was
    /// forked.
    pub from: String,
    /// Why the fork's memory was not restored, so it booted from the disk
    /// and its processes were not kept, if it was not.
    pub reason: Option<String>,
}

impl Sandbox {
    /// The VM of the project `path` is in: the folder with `cave.toml`, or
    /// its nearest parent with one, as `cave` finds it. Talks to the daemon
    /// `cave` uses ([`Daemon::locate`]).
    pub fn open(path: impl AsRef<Path>) -> Result<Sandbox> {
        Sandbox::open_with(Daemon::locate()?, path)
    }

    /// [`Sandbox::open`] with a given daemon.
    pub fn open_with(daemon: Daemon, path: impl AsRef<Path>) -> Result<Sandbox> {
        Ok(Sandbox {
            daemon,
            project: project::require(path.as_ref())?,
            fork: None,
        })
    }

    /// The project folder.
    pub fn project(&self) -> &Path {
        &self.project
    }

    /// The fork's name, or `None` for the project's own VM.
    pub fn fork_name(&self) -> Option<&str> {
        self.fork.as_deref()
    }

    fn vm(&self) -> VmId {
        VmId {
            project: self.project.to_string_lossy().into_owned(),
            fork: self.fork.clone(),
        }
    }

    /// Starts the VM if it is not running, and returns what it can reach.
    pub fn start(&self) -> Result<Policy> {
        match self.daemon.call(&Request::Start {
            vm: self.vm(),
            net: None,
            ports: Vec::new(),
        })? {
            Reply::Policy(policy) => Ok(policy),
            other => Err(Error::unexpected("start", other)),
        }
    }

    /// Runs `argv` in the VM, starting it first if needed, and waits for it
    /// to finish.
    pub fn exec<I, S>(&self, argv: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        let Some((program, args)) = argv.split_first() else {
            return Err(Error::new("the command is empty")
                .fix("Pass a program, such as [\"uname\", \"-a\"]"));
        };
        self.run(&Command::new(program.clone()).args(args.iter().cloned()))
    }

    /// Runs `command` in the VM, starting it first if needed, and waits for
    /// it to finish.
    pub fn run(&self, command: &Command) -> Result<Output> {
        if command.argv.first().is_none_or(String::is_empty) {
            return Err(Error::new("the command is empty")
                .fix("Pass a program, such as Command::new(\"ls\")"));
        }
        self.start()?;
        let mut connection = self.daemon.connect()?;
        connection.send(&Request::Exec {
            vm: self.vm(),
            exec: Exec {
                argv: command.argv.clone(),
                cwd: command.cwd.clone(),
                env: command.env.clone(),
                stdin: command.stdin.is_some(),
            },
            credentials: Vec::new(),
        })?;
        if let Some(stdin) = command.stdin.clone() {
            // Stdin goes up on its own thread, so output the command prints
            // meanwhile is read and nothing waits on a full socket.
            let mut sender = connection.sender()?;
            thread::spawn(move || {
                for chunk in stdin.chunks(MAX_CHUNK) {
                    let data = chunk.to_vec().into();
                    if sender.send(&Request::Input { data }).is_err() {
                        return;
                    }
                }
                let _ = sender.send(&Request::InputEnd);
            });
        }
        let mut output = Output::default();
        loop {
            match connection.reply()? {
                Reply::Output { stream, data } => match stream {
                    Stream::Stdout => output.stdout.extend_from_slice(&data.0),
                    Stream::Stderr => output.stderr.extend_from_slice(&data.0),
                },
                Reply::Exit { code, signal } => {
                    output.code = code;
                    output.signal = signal;
                    return Ok(output);
                }
                other => return Err(Error::unexpected("exec", other)),
            }
        }
    }

    /// Saves a snapshot of the VM named `name`.
    pub fn snapshot(&self, name: &str) -> Result<Snapshot> {
        match self.daemon.call(&Request::Snapshot {
            vm: self.vm(),
            name: Some(name.into()),
        })? {
            Reply::Snapshot { name, size } => Ok(Snapshot { name, size }),
            other => Err(Error::unexpected("snapshot", other)),
        }
    }

    /// Puts the VM back to snapshot `name`, saving the state before it
    /// first.
    pub fn restore(&self, name: &str) -> Result<Restored> {
        match self.daemon.call(&Request::Restore {
            vm: self.vm(),
            name: Some(name.into()),
        })? {
            Reply::Restored {
                name,
                memory_restored,
                reason,
                saved,
            } => Ok(Restored {
                name,
                memory_restored,
                reason,
                saved,
            }),
            other => Err(Error::unexpected("restore", other)),
        }
    }

    /// A new fork of the VM's current state, with a generated name.
    pub fn fork(&self) -> Result<Forked> {
        self.fork_request(None)
    }

    /// A new fork made from snapshot `snapshot`, with a generated name.
    pub fn fork_from(&self, snapshot: &str) -> Result<Forked> {
        self.fork_request(Some(snapshot.into()))
    }

    fn fork_request(&self, from: Option<String>) -> Result<Forked> {
        match self.daemon.call(&Request::Fork {
            vm: self.vm(),
            name: None,
            from,
        })? {
            Reply::Forked { name, from, reason } => Ok(Forked {
                sandbox: Sandbox {
                    daemon: self.daemon.clone(),
                    project: self.project.clone(),
                    fork: Some(name),
                },
                from,
                reason,
            }),
            other => Err(Error::unexpected("fork", other)),
        }
    }

    /// Removes the VM. For a fork, that is the fork alone. For the
    /// project's own VM, it is its disk, snapshots and forks, as with
    /// `cave rm --yes`: in a sealed project, code that is only in the VM
    /// is lost. `cave.toml` stays.
    pub fn destroy(self) -> Result<()> {
        match self.daemon.call(&Request::Remove {
            vm: VmId {
                project: self.project.to_string_lossy().into_owned(),
                fork: None,
            },
            name: self.fork,
        })? {
            Reply::Ok => Ok(()),
            other => Err(Error::unexpected("remove", other)),
        }
    }
}
