//! `cave exec`: one command in the VM, with its output streamed back.

use std::io::{self, Read, Write};
use std::thread;

use cave_vm_proto::agent::{Exec, Stream};
use cave_vm_proto::daemon::{Reply, Request};

use crate::client::Connection;
use crate::commands::{ensure_running, vm_id};
use crate::error::{CliError, EXIT_USAGE, Result};
use crate::init;

/// Exit code when cave fails before or while running the command
/// (docs/cli.md, "Exit codes").
pub const EXIT_CAVE_FAILED_RUNNING: u8 = 125;

/// Bytes of stdin per `input` message.
const INPUT_CHUNK: usize = 64 << 10;

/// `cave exec`. Returns the command's exit code.
pub fn run(fork: Option<String>, argv: Vec<String>) -> Result<u8> {
    if argv.is_empty() {
        return Err(CliError {
            code: EXIT_USAGE,
            ..CliError::new("missing the command to run").fix("cave exec uname -a")
        });
    }
    let failed = |e: CliError| CliError {
        code: EXIT_CAVE_FAILED_RUNNING,
        ..e
    };
    let project = init::project_or_set_up().map_err(failed)?;
    let vm = vm_id(&project, fork);
    ensure_running(&vm, None, Vec::new(), false).map_err(failed)?;

    let mut connection = Connection::open().map_err(failed)?;
    connection
        .send(&Request::Exec {
            vm,
            exec: Exec {
                argv,
                stdin: true,
                ..Exec::default()
            },
            credentials: Vec::new(),
        })
        .map_err(failed)?;
    let mut sender = connection.sender().map_err(failed)?;
    // Stdin goes up on its own thread; the process exits when the command
    // does, whether or not stdin has ended.
    thread::spawn(move || {
        let mut buf = vec![0u8; INPUT_CHUNK];
        let mut stdin = io::stdin().lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec().into();
                    if sender.send(&Request::Input { data }).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sender.send(&Request::InputEnd);
    });

    loop {
        match connection.receive().map_err(failed)? {
            Reply::Output { stream, data } => {
                let written = match stream {
                    Stream::Stdout => write_all(&mut io::stdout().lock(), &data.0),
                    Stream::Stderr => write_all(&mut io::stderr().lock(), &data.0),
                };
                // A closed pipe on our side, as in `cave exec -- yes | head`.
                if written.is_err() {
                    return Ok(EXIT_CAVE_FAILED_RUNNING);
                }
            }
            Reply::Exit { code, signal } => return Ok(exit_code(code, signal.as_deref())),
            Reply::Progress { message, .. } => eprintln!("{message}"),
            Reply::Error(e) => return Err(failed(e.into())),
            other => {
                return Err(failed(CliError::new(format!(
                    "the daemon sent an unexpected reply to exec: {other:?}"
                ))));
            }
        }
    }
}

fn write_all(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    out.write_all(data)?;
    out.flush()
}

/// The command's code, or 128 plus the signal number when a signal ended
/// it, as a shell reports it.
pub fn exit_code(code: Option<i32>, signal: Option<&str>) -> u8 {
    if let Some(code) = code {
        return u8::try_from(code).unwrap_or(EXIT_CAVE_FAILED_RUNNING);
    }
    let number = match signal {
        Some("HUP") => 1,
        Some("INT") => 2,
        Some("QUIT") => 3,
        Some("ABRT") => 6,
        Some("KILL") => 9,
        Some("SEGV") => 11,
        Some("PIPE") => 13,
        Some("TERM") => 15,
        _ => return EXIT_CAVE_FAILED_RUNNING,
    };
    128 + number
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_map_to_shell_exit_codes() {
        assert_eq!(exit_code(Some(0), None), 0);
        assert_eq!(exit_code(Some(3), None), 3);
        assert_eq!(exit_code(None, Some("KILL")), 137);
        assert_eq!(exit_code(None, Some("TERM")), 143);
        assert_eq!(exit_code(None, Some("WINCH")), EXIT_CAVE_FAILED_RUNNING);
        assert_eq!(exit_code(Some(300), None), EXIT_CAVE_FAILED_RUNNING);
    }
}
