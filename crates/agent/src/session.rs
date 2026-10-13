//! One connection: the handshake, then one operation.

use std::io;
use std::path::PathBuf;

use cave_vm_proto::agent::Message;
use cave_vm_proto::{ErrorCode, VERSIONS, read_frame};

use crate::stream::{Out, Stream};
use crate::{control, exec, files, forward};

/// What every operation needs to know.
#[derive(Debug, Clone)]
pub struct Config {
    /// Working directory for requests that leave `cwd` out; `/workspace`
    /// in the guest.
    pub default_cwd: PathBuf,
    /// This agent's name and version, sent in the handshake.
    pub software: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            default_cwd: PathBuf::from(cave_vm_proto::agent::DEFAULT_CWD),
            software: format!("cave-agent {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Serves one connection until its operation ends. Protocol errors are
/// reported to the peer; the returned error is only for the agent's log.
pub fn serve(stream: Box<dyn Stream>, config: &Config) -> io::Result<()> {
    let mut reader = stream.try_clone_stream()?;
    let out = Out::new(stream);
    let read = |reader: &mut Box<dyn Stream>| -> io::Result<Option<Message>> {
        match read_frame::<_, Message>(reader) {
            Ok(message) => Ok(message),
            Err(e) => {
                let _ = out.send(&Message::Error(e.to_error_message()));
                Err(io::Error::other(e))
            }
        }
    };

    let Some(first) = read(&mut reader)? else {
        return Ok(());
    };
    let Message::Hello(hello) = first else {
        return out.error(ErrorCode::BadRequest, "the first message must be hello");
    };
    match hello.accept(VERSIONS, &config.software) {
        Ok(reply) => out.send(&Message::Hello(reply))?,
        Err(e) => return out.send(&Message::Error(e)),
    }

    let Some(request) = read(&mut reader)? else {
        return Ok(());
    };
    match request {
        Message::Exec(req) => exec::run(req, reader, out, config),
        Message::FileRead { path } => files::read(&path, &out, config),
        Message::FileWrite {
            path,
            mode,
            create_dirs,
        } => files::write(&path, mode, create_dirs, reader, &out, config),
        Message::PortForward { port } => forward::run(port, reader, out),
        Message::Control => control::run(reader, &out),
        Message::PtyOpen(_) => out.error(
            ErrorCode::BadRequest,
            "this agent does not support pty_open yet",
        ),
        other => out.error(
            ErrorCode::BadRequest,
            format!("expected a request, got {}", kind(&other)),
        ),
    }
}

/// The variant name of a message, such as `Output`, for errors.
pub(crate) fn kind(message: &Message) -> String {
    let debug = format!("{message:?}");
    let end = debug
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(debug.len());
    debug[..end].to_string()
}
