//! `exec`: runs a program and streams its output and exit status.

use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use cave_vm_proto::agent::{
    EXIT_CANNOT_RUN, EXIT_NOT_FOUND, Exec, Message, Signal, Stream as Which,
};
use cave_vm_proto::{Data, ErrorCode, read_frame};

use crate::session::Config;
use crate::stream::{Out, Stream};
use crate::sys;

/// Bytes read from the program per `output` message.
const READ_CHUNK: usize = 64 * 1024;

/// The child, and whether it has exited (but is not yet reaped). Signals go
/// out only while it has not, so they never reach a reused pid.
pub(crate) struct Running {
    pid: u32,
    exited: Mutex<bool>,
    stdin: Mutex<Option<ChildStdin>>,
}

impl Running {
    pub(crate) fn new(pid: u32, stdin: Option<ChildStdin>) -> Running {
        Running {
            pid,
            exited: Mutex::new(false),
            stdin: Mutex::new(stdin),
        }
    }

    pub(crate) fn signal(&self, signal: Signal) {
        let exited = self.exited.lock().expect("exited lock");
        if !*exited {
            let _ = sys::kill(self.pid, signal);
        }
    }

    /// Signals the child's whole process group, for a session leader.
    pub(crate) fn signal_group(&self, signal: Signal) {
        let exited = self.exited.lock().expect("exited lock");
        if !*exited {
            let _ = sys::kill_group(self.pid, signal);
        }
    }
}

/// Resolves a request path against the default working directory.
pub(crate) fn resolve(path: &str, config: &Config) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.default_cwd.join(path)
    }
}

/// The working directory for a request, or `None` after telling the client
/// it does not exist.
pub(crate) fn working_dir(
    cwd: Option<&str>,
    out: &Out,
    config: &Config,
) -> io::Result<Option<PathBuf>> {
    let cwd = cwd.map_or_else(|| config.default_cwd.clone(), |c| resolve(c, config));
    if cwd.is_dir() {
        Ok(Some(cwd))
    } else {
        out.error(
            ErrorCode::NotFound,
            format!("no such directory in the VM: {}", cwd.display()),
        )?;
        Ok(None)
    }
}

pub fn run(req: Exec, mut reader: Box<dyn Stream>, out: Out, config: &Config) -> io::Result<()> {
    let Some(cwd) = working_dir(req.cwd.as_deref(), &out, config)? else {
        return Ok(());
    };
    let mut command = Command::new(&req.argv[0]);
    if let Some(user) = &config.user {
        user.apply(&mut command);
    }
    command
        .args(&req.argv[1..])
        .current_dir(&cwd)
        .envs(&req.env)
        .stdin(if req.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return cannot_start(&req.argv[0], &e, &out),
    };

    let running = Arc::new(Running::new(child.id(), child.stdin.take()));
    let pumps = [
        pump(child.stdout.take(), Which::Stdout, out.clone()),
        pump(child.stderr.take(), Which::Stderr, out.clone()),
    ];
    let input_running = Arc::clone(&running);
    thread::spawn(move || input(&mut reader, &input_running));

    let status = wait(&mut child, &running)?;
    for p in pumps.into_iter().flatten() {
        let _ = p.join();
    }
    out.finish(&exit_message(status))
}

/// The `exit` message for how a program ended.
pub(crate) fn exit_message(status: ExitStatus) -> Message {
    match (status.code(), status.signal()) {
        (Some(code), _) => Message::Exit {
            code: Some(code),
            signal: None,
        },
        (None, Some(sig)) => Message::Exit {
            code: None,
            signal: Some(sys::signal_name(sig)),
        },
        (None, None) => Message::Exit {
            code: Some(EXIT_CANNOT_RUN),
            signal: None,
        },
    }
}

/// Waits for the child: first without reaping, then marks it exited under
/// the lock signals take, then reaps it.
pub(crate) fn wait(child: &mut Child, running: &Running) -> io::Result<ExitStatus> {
    sys::wait_exit_keep(running.pid)?;
    *running.exited.lock().expect("exited lock") = true;
    child.wait()
}

/// A program that could not start: a line on stderr and exit 127 or 126,
/// as a shell would.
pub(crate) fn cannot_start(program: &str, e: &io::Error, out: &Out) -> io::Result<()> {
    let (code, why) = if e.kind() == io::ErrorKind::NotFound {
        (EXIT_NOT_FOUND, "command not found".to_string())
    } else {
        (EXIT_CANNOT_RUN, e.to_string())
    };
    out.send(&Message::Output {
        stream: Which::Stderr,
        data: Data(format!("cave: {program}: {why}\n").into_bytes()),
    })?;
    out.send(&Message::Exit {
        code: Some(code),
        signal: None,
    })
}

fn pump<R: Read + Send + 'static>(
    source: Option<R>,
    stream: Which,
    out: Out,
) -> Option<thread::JoinHandle<()>> {
    let mut source = source?;
    Some(thread::spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match source.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let message = Message::Output {
                        stream,
                        data: Data(buf[..n].to_vec()),
                    };
                    if out.send(&message).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }))
}

/// Client messages during the run. If the client goes away, the program is
/// killed.
fn input(reader: &mut Box<dyn Stream>, running: &Running) {
    loop {
        match read_frame::<_, Message>(reader) {
            Ok(Some(Message::Input { data })) => {
                let mut stdin = running.stdin.lock().expect("stdin lock");
                if let Some(pipe) = stdin.as_mut()
                    && pipe.write_all(&data.0).is_err()
                {
                    *stdin = None;
                }
            }
            Ok(Some(Message::InputEnd)) => {
                *running.stdin.lock().expect("stdin lock") = None;
            }
            Ok(Some(Message::Signal { signal })) => running.signal(signal),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                running.signal(Signal::Kill);
                return;
            }
        }
    }
}
