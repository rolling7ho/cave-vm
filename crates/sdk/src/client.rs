//! Connections to the daemon, which is started on demand (docs/cli.md,
//! "The daemon and the data directory").

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_proto::daemon::{Reply, Request};
use cave_vm_proto::{Hello, VERSIONS, read_frame, write_frame};

use crate::error::{Error, Result};

/// How long a freshly started daemon gets to open its socket.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the daemon is, and how to start it when it is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    dir: PathBuf,
    /// The `cave-daemon` executable, or `None` to never start one.
    program: Option<PathBuf>,
}

impl Daemon {
    /// The daemon `cave` uses: its data directory is `CAVE_HOME`, or
    /// `~/Library/Application Support/cave-vm`, and it is started when it
    /// is not running, from `CAVE_DAEMON`, `cave-daemon` next to this
    /// program, or `cave-daemon` on the `PATH`.
    pub fn locate() -> Result<Daemon> {
        let dir = match std::env::var_os("CAVE_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => crate::project::home()
                .ok_or_else(|| Error::new("HOME is not set").fix("Set HOME to your home folder"))?
                .join("Library/Application Support/cave-vm"),
        };
        Ok(Daemon {
            dir,
            program: Some(daemon_program()),
        })
    }

    /// A daemon already running with data directory `dir`. The SDK never
    /// starts one there.
    pub fn at(dir: impl Into<PathBuf>) -> Daemon {
        Daemon {
            dir: dir.into(),
            program: None,
        }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("daemon.sock")
    }

    /// Connects, starting the daemon first if it is not running and this
    /// daemon may be started, and does the handshake.
    pub(crate) fn connect(&self) -> Result<Connection> {
        let socket = self.socket();
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(e) => match &self.program {
                Some(program) => start(&self.dir, &socket, program)?,
                None => {
                    return Err(Error::new(format!(
                        "cannot reach the cave daemon at {}: {e}",
                        socket.display()
                    ))
                    .fix("Start it by running any cave command, such as `cave ls`"));
                }
            },
        };
        let mut connection = Connection { stream };
        connection.send(&Request::Hello(Hello::client(VERSIONS, software())))?;
        match connection.receive()? {
            Reply::Hello(reply) => {
                reply.agreed(VERSIONS)?;
                Ok(connection)
            }
            Reply::Error(e) => {
                Err(Error::from(e).fix("Update cave so the SDK and the daemon match"))
            }
            other => Err(Error::unexpected("hello", other)),
        }
    }

    /// Sends one request on a new connection and returns its final reply,
    /// skipping progress messages. An `error` reply becomes an [`Error`].
    pub(crate) fn call(&self, request: &Request) -> Result<Reply> {
        let mut connection = self.connect()?;
        connection.send(request)?;
        connection.reply()
    }
}

/// One operation's connection to the daemon.
pub(crate) struct Connection {
    stream: UnixStream,
}

impl Connection {
    pub(crate) fn send(&mut self, request: &Request) -> Result<()> {
        write_frame(&mut self.stream, request).map_err(Error::daemon_gone)
    }

    /// A second handle that sends on this connection, for a thread that
    /// streams input while this one reads.
    pub(crate) fn sender(&self) -> Result<Connection> {
        Ok(Connection {
            stream: self.stream.try_clone()?,
        })
    }

    /// The next message from the daemon.
    pub(crate) fn receive(&mut self) -> Result<Reply> {
        match read_frame::<_, Reply>(&mut self.stream)? {
            Some(reply) => Ok(reply),
            None => Err(Error::daemon_gone("it closed the connection")),
        }
    }

    /// The next message that is not progress. An `error` reply becomes an
    /// [`Error`].
    pub(crate) fn reply(&mut self) -> Result<Reply> {
        loop {
            match self.receive()? {
                Reply::Progress { .. } => {}
                Reply::Error(e) => return Err(e.into()),
                reply => return Ok(reply),
            }
        }
    }
}

/// `CAVE_DAEMON`, `cave-daemon` next to this program, or `cave-daemon` on
/// the `PATH`.
fn daemon_program() -> PathBuf {
    if let Some(program) = std::env::var_os("CAVE_DAEMON") {
        return PathBuf::from(program);
    }
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("cave-daemon"))
        .filter(|program| program.is_file())
        .unwrap_or_else(|| PathBuf::from("cave-daemon"))
}

fn start(dir: &Path, socket: &Path, program: &Path) -> Result<UnixStream> {
    fs::create_dir_all(dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| {
            Error::new(format!(
                "cannot start the cave daemon ({}): {e}",
                program.display()
            ))
            .fix("Install cave, or set CAVE_DAEMON to the cave-daemon program")
        })?;
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                return Err(
                    Error::new(format!("the cave daemon did not start: {e}")).fix(format!(
                        "See {} for its output",
                        dir.join("daemon.log").display()
                    )),
                );
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e.into()),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn software() -> String {
    format!("cave-vm-sdk {}", env!("CARGO_PKG_VERSION"))
}
