//! `cave shell`: an interactive shell in the VM on a pseudo-terminal that
//! follows window resizes.
//!
//! The Mac's terminal is put in raw mode with `/bin/stty`, so keys such as
//! Ctrl-C reach the shell in the VM as bytes, and the window size is read
//! the same way. This crate has no `unsafe` (D24), so it cannot call
//! `ioctl` or catch `SIGWINCH`; the size is polled instead.
//!
//! `exit` or Ctrl-D ends the shell. `~.` at the start of a line leaves at
//! once, as in ssh, for when a program in the VM holds the keyboard.

use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cave_vm_proto::agent::PtyOpen;
use cave_vm_proto::daemon::{Reply, Request, VmId};

use crate::client::Connection;
use crate::commands::{ensure_running, vm_id};
use crate::error::{CliError, Result};
use crate::exec::{EXIT_CAVE_FAILED_RUNNING, exit_code};
use crate::init;

/// Bytes of keyboard input per `pty_data` message.
const INPUT_CHUNK: usize = 16 << 10;

/// How often the window size is checked.
const RESIZE_POLL: Duration = Duration::from_millis(250);

/// The size used when the terminal does not report one.
const FALLBACK_SIZE: (u16, u16) = (80, 24);

/// Runs `/bin/stty` on the terminal (standard input) and returns what it
/// prints.
fn stty(args: &[&str]) -> io::Result<String> {
    let out = Command::new("/bin/stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("stty {} failed", args.join(" "))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Parses `stty size` output, `<rows> <cols>`, into `(cols, rows)`.
fn parse_size(text: &str) -> Option<(u16, u16)> {
    let (rows, cols) = text.split_once(' ')?;
    let (cols, rows) = (cols.trim().parse().ok()?, rows.trim().parse().ok()?);
    (cols > 0 && rows > 0).then_some((cols, rows))
}

/// The window's `(cols, rows)`.
fn window_size() -> Option<(u16, u16)> {
    parse_size(&stty(&["size"]).ok()?)
}

/// Raw mode on the terminal until dropped.
struct RawMode {
    saved: String,
}

impl RawMode {
    fn enter() -> io::Result<RawMode> {
        let saved = stty(&["-g"])?;
        stty(&["raw", "-echo"])?;
        Ok(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = stty(&[&self.saved]);
    }
}

/// `cave shell`. Returns the shell's exit code.
pub fn run(fork: Option<String>) -> Result<u8> {
    let failed = |e: CliError| CliError {
        code: EXIT_CAVE_FAILED_RUNNING,
        ..e
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(failed(
            CliError::new("cave shell needs a terminal")
                .fix("Use `cave exec -- <cmd>` from scripts"),
        ));
    }
    let project = init::project_or_set_up().map_err(failed)?;
    let vm = vm_id(&project, fork);
    ensure_running(&vm, None, Vec::new(), false).map_err(failed)?;
    eprintln!(
        "Type exit or press Ctrl-D to leave the shell, or ~. at the start of a line to leave at once. The VM keeps running"
    );
    session(vm, None)
}

/// What the keyboard thread does with the bytes it reads: ssh's escape,
/// `~.` at the start of a line, leaves; `~~` sends one `~`.
#[derive(Default)]
struct Escape {
    /// The last byte sent ended a line, or nothing was sent yet.
    mid_line: bool,
    /// A `~` at the start of a line is held until the next byte.
    tilde: bool,
}

impl Escape {
    /// Returns the bytes to send, and whether to leave.
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.tilde {
                self.tilde = false;
                match b {
                    b'.' => return (out, true),
                    b'~' => {
                        out.push(b'~');
                        self.mid_line = true;
                        continue;
                    }
                    _ => out.push(b'~'),
                }
            } else if b == b'~' && !self.mid_line {
                self.tilde = true;
                continue;
            }
            out.push(b);
            self.mid_line = !matches!(b, b'\r' | b'\n');
        }
        (out, false)
    }
}

/// Runs `argv`, or the guest user's login shell, on a pseudo-terminal in
/// running VM `vm`, connected to this terminal. Returns its exit code.
/// The `~.` escape works for the login shell only, so an agent gets every
/// key it is sent.
pub fn session(vm: VmId, argv: Option<Vec<String>>) -> Result<u8> {
    let escapes = argv.is_none();
    let failed = |e: CliError| CliError {
        code: EXIT_CAVE_FAILED_RUNNING,
        ..e
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(failed(
            CliError::new("this needs a terminal").fix("Run it in Terminal"),
        ));
    }
    let (cols, rows) = window_size().unwrap_or(FALLBACK_SIZE);
    let mut connection = Connection::open().map_err(failed)?;
    connection
        .send(&Request::Shell {
            vm,
            pty: PtyOpen {
                argv,
                cwd: None,
                env: Default::default(),
                term: std::env::var("TERM").ok(),
                cols,
                rows,
            },
        })
        .map_err(failed)?;
    // Both the keyboard and the resize threads send, one frame at a time.
    let sender = Arc::new(Mutex::new(connection.sender().map_err(failed)?));
    let raw = RawMode::enter().map_err(|e| {
        failed(
            CliError::new(format!("cannot set up the terminal: {e}"))
                .fix("Use `cave exec -- <cmd>`"),
        )
    })?;

    let keys = Arc::clone(&sender);
    let saved = raw.saved.clone();
    thread::spawn(move || {
        let mut buf = vec![0u8; INPUT_CHUNK];
        let mut stdin = io::stdin().lock();
        let mut escape = Escape::default();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let (bytes, leave) = if escapes {
                        escape.feed(&buf[..n])
                    } else {
                        (buf[..n].to_vec(), false)
                    };
                    if leave {
                        // The main thread is waiting on the daemon, so leave
                        // from here: the terminal back to normal, then exit.
                        // The daemon ends the shell when the connection goes.
                        let _ = stty(&[&saved]);
                        eprintln!("\nLeft the shell. The VM keeps running");
                        std::process::exit(0);
                    }
                    if bytes.is_empty() {
                        continue;
                    }
                    let data = bytes.into();
                    let sent = keys
                        .lock()
                        .expect("sender lock")
                        .send(&Request::PtyData { data });
                    if sent.is_err() {
                        return;
                    }
                }
            }
        }
    });
    let resizes = Arc::clone(&sender);
    thread::spawn(move || {
        let mut last = (cols, rows);
        loop {
            thread::sleep(RESIZE_POLL);
            let Some(size) = window_size() else { continue };
            if size == last {
                continue;
            }
            last = size;
            let (cols, rows) = size;
            let request = Request::PtyResize { cols, rows };
            if resizes.lock().expect("sender lock").send(&request).is_err() {
                return;
            }
        }
    });

    let result = loop {
        match connection.receive() {
            Ok(Reply::PtyData { data }) => {
                let mut out = io::stdout().lock();
                if out.write_all(&data.0).and_then(|()| out.flush()).is_err() {
                    break Ok(EXIT_CAVE_FAILED_RUNNING);
                }
            }
            Ok(Reply::Exit { code, signal }) => break Ok(exit_code(code, signal.as_deref())),
            Ok(Reply::Error(e)) => break Err(failed(e.into())),
            Ok(Reply::Progress { .. }) => {}
            Ok(other) => {
                break Err(failed(CliError::new(format!(
                    "the daemon sent an unexpected reply to shell: {other:?}"
                ))));
            }
            Err(e) => break Err(failed(e)),
        }
    };
    // Back to the normal terminal before anything is printed.
    drop(raw);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_dot_at_the_start_of_a_line_leaves() {
        assert_eq!(Escape::default().feed(b"~."), (vec![], true));
        assert_eq!(Escape::default().feed(b"ls\r~."), (b"ls\r".to_vec(), true));
        let mut e = Escape::default();
        assert_eq!(e.feed(b"echo a~."), (b"echo a~.".to_vec(), false));
        assert_eq!(e.feed(b"\r~"), (b"\r".to_vec(), false));
        assert_eq!(e.feed(b"."), (vec![], true));
    }

    #[test]
    fn other_tildes_go_through() {
        let mut e = Escape::default();
        assert_eq!(e.feed(b"~~"), (b"~".to_vec(), false));
        assert_eq!(e.feed(b"/x\n~/"), (b"/x\n~/".to_vec(), false));
    }

    #[test]
    fn stty_size_is_rows_then_cols() {
        assert_eq!(parse_size("40 120"), Some((120, 40)));
        assert_eq!(parse_size("0 0"), None);
        assert_eq!(parse_size("garbage"), None);
    }
}
