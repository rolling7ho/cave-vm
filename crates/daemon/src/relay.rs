//! `exec`, `shell`, `file_read` and `file_write`: the client's request goes
//! to the guest agent over a fresh agent connection, and the stream
//! messages are relayed both ways until the agent ends the operation with
//! `exit`, `file_end`, `file_written` or `error` (docs/protocol.md, "exec
//! and shell").
//!
//! If the client goes away first, the agent connection is closed, and the
//! agent kills the program.

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use cave_vm_proto::agent::Message;
use cave_vm_proto::daemon::{Reply, Request};
use cave_vm_proto::handshake::Hello;
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};

use crate::link::daemon_software;

/// How long the agent has to answer `hello` on a new connection.
const AGENT_HELLO_TIMEOUT: Duration = Duration::from_secs(10);

fn agent_failed(e: impl std::fmt::Display) -> ErrorMessage {
    ErrorMessage::new(
        ErrorCode::Io,
        format!("lost the connection to the guest agent: {e}"),
    )
    .with_fix(
        "Run `cave stop`, then `cave run`; the guest's console is in vmm.log in the VM's folder",
    )
}

pub(crate) fn read_agent(agent: &mut UnixStream) -> Result<Message, ErrorMessage> {
    match read_frame::<_, Message>(agent) {
        Ok(Some(message)) => Ok(message),
        Ok(None) => Err(agent_failed("it closed the connection")),
        Err(e) => Err(agent_failed(e)),
    }
}

/// The handshake on a new agent connection, then `request`.
pub(crate) fn open(agent: &mut UnixStream, request: &Message) -> Result<(), ErrorMessage> {
    agent
        .set_read_timeout(Some(AGENT_HELLO_TIMEOUT))
        .map_err(agent_failed)?;
    write_frame(
        agent,
        &Message::Hello(Hello::client(VERSIONS, daemon_software())),
    )
    .map_err(agent_failed)?;
    match read_agent(agent)? {
        Message::Hello(reply) => {
            reply.agreed(VERSIONS)?;
        }
        Message::Error(e) => return Err(e),
        other => return Err(agent_failed(format!("it answered hello with {other:?}"))),
    }
    agent.set_read_timeout(None).map_err(agent_failed)?;
    write_frame(agent, request).map_err(agent_failed)
}

/// A client stream message for the agent, or `None` for one that does not
/// belong in a relayed operation.
fn from_client(request: Request) -> Option<Message> {
    Some(match request {
        Request::Input { data } => Message::Input { data },
        Request::InputEnd => Message::InputEnd,
        Request::Signal { signal } => Message::Signal { signal },
        Request::PtyData { data } => Message::PtyData { data },
        Request::PtyResize { cols, rows } => Message::PtyResize { cols, rows },
        Request::FileData { data } => Message::FileData { data },
        Request::FileEnd { sha256 } => Message::FileEnd { sha256 },
        _ => return None,
    })
}

/// An agent stream message for the client, and whether it ends the
/// operation.
fn to_client(message: Message) -> (Reply, bool) {
    match message {
        Message::Output { stream, data } => (Reply::Output { stream, data }, false),
        Message::PtyData { data } => (Reply::PtyData { data }, false),
        Message::Exit { code, signal } => (Reply::Exit { code, signal }, true),
        Message::FileInfo { size, mode } => (Reply::FileInfo { size, mode }, false),
        Message::FileData { data } => (Reply::FileData { data }, false),
        Message::FileEnd { sha256 } => (Reply::FileEnd { sha256 }, true),
        Message::FileWritten { size } => (Reply::FileWritten { size }, true),
        Message::Error(e) => (Reply::Error(e), true),
        other => (
            Reply::Error(agent_failed(format!("it sent {other:?} during a command"))),
            true,
        ),
    }
}

/// How a relayed operation ended.
#[derive(Debug)]
pub struct Ended {
    /// The reply that ended it, such as `exit` or `file_written`, whether
    /// or not the client was still there to get it. `None` when the client
    /// went away before the operation ended.
    pub last: Option<Reply>,
    /// Whether every reply reached the client.
    pub result: io::Result<()>,
}

/// Runs `request` (an agent `exec`, `pty_open`, `file_read` or
/// `file_write`) on `agent` and relays it
/// to and from `client` until it ends. Errors before the program starts go
/// to the client as an `error` reply.
pub fn run(client: &mut UnixStream, mut agent: UnixStream, request: &Message) -> Ended {
    if let Err(e) = open(&mut agent, request) {
        let reply = Reply::Error(e);
        let result = write_frame(client, &reply).map_err(io::Error::other);
        return Ended {
            last: Some(reply),
            result,
        };
    }
    // Client to agent, on its own thread. It ends when the client closes
    // its side, or when the operation ends and its side is shut below.
    let clones = client.try_clone().and_then(|c| Ok((c, agent.try_clone()?)));
    let (mut client_in, mut agent_out) = match clones {
        Ok(clones) => clones,
        Err(e) => {
            return Ended {
                last: None,
                result: Err(e),
            };
        }
    };
    let upstream = thread::spawn(move || {
        loop {
            match read_frame::<_, Request>(&mut client_in) {
                Ok(Some(request)) => {
                    if let Some(message) = from_client(request)
                        && write_frame(&mut agent_out, &message).is_err()
                    {
                        return;
                    }
                }
                // The client went away: closing the connection makes the
                // agent kill the program.
                Ok(None) | Err(_) => {
                    let _ = agent_out.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
    });

    let ended = loop {
        let message = read_agent(&mut agent);
        let (reply, last) = match message {
            Ok(message) => to_client(message),
            Err(e) => (Reply::Error(e), true),
        };
        let result = write_frame(client, &reply).map_err(io::Error::other);
        if last {
            break Ended {
                last: Some(reply),
                result,
            };
        }
        if result.is_err() {
            break Ended { last: None, result };
        }
    };
    let _ = agent.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Read);
    let _ = upstream.join();
    ended
}

#[cfg(test)]
mod tests;
