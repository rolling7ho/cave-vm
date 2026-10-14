//! The daemon API client. The daemon is started on demand (docs/cli.md,
//! "The daemon and the data directory").

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_proto::daemon::{Reply, Request};
use cave_vm_proto::{FrameError, Hello, VERSIONS, read_frame, write_frame};

use crate::error::{CliError, Result};

/// How long a freshly started daemon gets to open its socket.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

/// The data directory: `CAVE_HOME`, or `~/Library/Application Support/cave-vm`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CAVE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = crate::project::home()
        .ok_or_else(|| CliError::new("HOME is not set").fix("Set HOME to your home folder"))?;
    Ok(home.join("Library/Application Support/cave-vm"))
}

/// The daemon executable: `CAVE_DAEMON`, or `cave-daemon` next to `cave`.
pub fn daemon_program() -> Result<PathBuf> {
    if let Some(program) = std::env::var_os("CAVE_DAEMON") {
        return Ok(PathBuf::from(program));
    }
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("cave-daemon"))
}

/// One operation's connection to the daemon.
pub struct Connection {
    stream: UnixStream,
}

fn daemon_gone(e: impl std::fmt::Display) -> CliError {
    CliError::new(format!("lost the connection to the cave daemon: {e}"))
        .fix("Run the command again; if it keeps happening, run `cave doctor`")
}

impl Connection {
    /// Connects, starting the daemon first if it is not running, and does
    /// the handshake.
    pub fn open() -> Result<Connection> {
        let dir = data_dir()?;
        let socket = dir.join("daemon.sock");
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) => start_daemon(&dir, &socket)?,
        };
        let mut connection = Connection { stream };
        connection.send(&Request::Hello(Hello::client(VERSIONS, cli_software())))?;
        match connection.receive()? {
            Reply::Hello(reply) => {
                reply.agreed(VERSIONS)?;
                Ok(connection)
            }
            Reply::Error(e) => {
                Err(CliError::from(e).fix("Update cave so the command line and the daemon match"))
            }
            other => Err(daemon_gone(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn send(&mut self, request: &Request) -> Result<()> {
        write_frame(&mut self.stream, request).map_err(daemon_gone)
    }

    /// A second handle that sends on this connection, for a thread that
    /// streams input while this one reads.
    pub fn sender(&self) -> Result<Connection> {
        Ok(Connection {
            stream: self.stream.try_clone()?,
        })
    }

    /// The connection itself, for a request after which it carries raw
    /// bytes (`ssh`).
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }

    /// The next message from the daemon.
    pub fn receive(&mut self) -> Result<Reply> {
        match read_frame::<_, Reply>(&mut self.stream) {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(daemon_gone("it closed the connection")),
            Err(FrameError::Io(e)) => Err(daemon_gone(e)),
            Err(e) => Err(daemon_gone(e)),
        }
    }

    /// Sends one request and returns its final reply, printing progress
    /// along the way. An `error` reply becomes a [`CliError`].
    pub fn call(request: &Request) -> Result<Reply> {
        let mut connection = Connection::open()?;
        connection.send(request)?;
        loop {
            match connection.receive()? {
                Reply::Progress { message, .. } => eprintln!("{message}"),
                Reply::Error(e) => return Err(e.into()),
                reply => return Ok(reply),
            }
        }
    }
}

fn start_daemon(dir: &std::path::Path, socket: &std::path::Path) -> Result<UnixStream> {
    fs::create_dir_all(dir)?;
    let program = daemon_program()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    Command::new(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| {
            CliError::new(format!(
                "cannot start the cave daemon ({}): {e}",
                program.display()
            ))
            .fix("Reinstall cave, or run `cave doctor`")
        })?;
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                return Err(
                    CliError::new(format!("the cave daemon did not start: {e}")).fix(format!(
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

fn cli_software() -> String {
    format!("cave {}", env!("CARGO_PKG_VERSION"))
}
