//! `pty_open`: a program, usually a login shell, on a pseudo-terminal.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use cave_vm_proto::agent::{Message, PtyOpen, Signal};
use cave_vm_proto::{Data, read_frame};

use crate::exec::{self, Running};
use crate::session::Config;
use crate::stream::{Out, Stream};
use crate::sys;

/// Bytes read from the terminal per `pty_data` message.
const READ_CHUNK: usize = 16 * 1024;

/// How long output may keep coming after the program exits, from processes
/// it left running on the terminal, before `exit` is sent.
const DRAIN: Duration = Duration::from_millis(200);

pub fn run(req: PtyOpen, mut reader: Box<dyn Stream>, out: Out, config: &Config) -> io::Result<()> {
    let argv = req.argv.clone().unwrap_or_else(|| login_shell(config));
    let cwd = match exec::working_dir(req.cwd.as_deref(), &out, config)? {
        Some(cwd) => cwd,
        None => return Ok(()),
    };
    let (controller, terminal) = sys::open_pty(req.cols, req.rows)?;
    let mut command = Command::new(&argv[0]);
    if let Some(user) = &config.user {
        // The terminal belongs to whoever logs in on it, as login(1) does.
        std::os::unix::fs::fchown(&terminal, Some(user.uid), None)?;
        user.apply(&mut command);
    }
    command
        .args(&argv[1..])
        .current_dir(&cwd)
        .env("TERM", req.term.as_deref().unwrap_or("xterm-256color"))
        .envs(&req.env)
        .stdin(Stdio::from(terminal.try_clone()?))
        .stdout(Stdio::from(terminal.try_clone()?))
        .stderr(Stdio::from(terminal));
    sys::new_session_on_stdin(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            drop(command);
            return exec::cannot_start(&argv[0], &e, &out);
        }
    };
    // The child has its copies; dropping ours lets reads end when it exits.
    drop(command);

    let controller = File::from(controller);
    let running = Arc::new(Running::new(child.id(), None));
    let (drained_tx, drained) = mpsc::channel();
    {
        let mut terminal = controller.try_clone()?;
        let out = out.clone();
        thread::spawn(move || {
            pump(&mut terminal, &out);
            let _ = drained_tx.send(());
        });
    }
    {
        let terminal = controller;
        let running = Arc::clone(&running);
        thread::spawn(move || input(&mut reader, terminal, &running));
    }

    let status = exec::wait(&mut child, &running)?;
    let _ = drained.recv_timeout(DRAIN);
    // Output after this would follow `exit`, so `finish` drops it.
    out.finish(&exec::exit_message(status))
}

/// The user's shell from /etc/passwd, as a login shell.
fn login_shell(config: &Config) -> Vec<String> {
    if let Some(user) = &config.user {
        return vec![user.shell.clone(), "-l".into()];
    }
    let uid = sys::uid().to_string();
    let shell = fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|passwd| {
            passwd.lines().find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.len() == 7 && fields[2] == uid && !fields[6].is_empty())
                    .then(|| fields[6].to_string())
            })
        })
        .unwrap_or_else(|| "/bin/sh".into());
    vec![shell, "-l".into()]
}

fn pump(terminal: &mut File, out: &Out) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match terminal.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let message = Message::PtyData {
                    data: Data(buf[..n].to_vec()),
                };
                if out.send(&message).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Linux returns EIO once no process has the terminal open.
            Err(_) => return,
        }
    }
}

/// Client messages during the session. If the client goes away, the whole
/// session's process group is killed.
fn input(reader: &mut Box<dyn Stream>, mut terminal: File, running: &Running) {
    loop {
        match read_frame::<_, Message>(reader) {
            Ok(Some(Message::PtyData { data })) => {
                let _ = terminal.write_all(&data.0);
            }
            Ok(Some(Message::PtyResize { cols, rows })) => {
                let _ = sys::set_window_size(terminal.as_fd(), cols, rows);
            }
            Ok(Some(Message::Signal { signal })) => running.signal(signal),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                running.signal_group(Signal::Kill);
                return;
            }
        }
    }
}
