//! `exec` and `shell`: the client's request goes to the guest agent over a
//! fresh agent connection, and the stream messages are relayed both ways
//! until the agent sends `exit` (docs/protocol.md, "exec and shell").
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
/// belong in `exec` or `shell`.
fn from_client(request: Request) -> Option<Message> {
    Some(match request {
        Request::Input { data } => Message::Input { data },
        Request::InputEnd => Message::InputEnd,
        Request::Signal { signal } => Message::Signal { signal },
        Request::PtyData { data } => Message::PtyData { data },
        Request::PtyResize { cols, rows } => Message::PtyResize { cols, rows },
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
        Message::Error(e) => (Reply::Error(e), true),
        other => (
            Reply::Error(agent_failed(format!("it sent {other:?} during a command"))),
            true,
        ),
    }
}

/// Runs `request` (an agent `exec` or `pty_open`) on `agent` and relays it
/// to and from `client` until it ends. Errors before the program starts go
/// to the client as an `error` reply.
pub fn run(client: &mut UnixStream, mut agent: UnixStream, request: &Message) -> io::Result<()> {
    if let Err(e) = open(&mut agent, request) {
        return write_frame(client, &Reply::Error(e)).map_err(io::Error::other);
    }
    // Client to agent, on its own thread. It ends when the client closes
    // its side, or when the operation ends and its side is shut below.
    let mut client_in = client.try_clone()?;
    let mut agent_out = agent.try_clone()?;
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

    let result = loop {
        let message = read_agent(&mut agent);
        let (reply, last) = match message {
            Ok(message) => to_client(message),
            Err(e) => (Reply::Error(e), true),
        };
        if let Err(e) = write_frame(client, &reply) {
            break Err(io::Error::other(e));
        }
        if last {
            break Ok(());
        }
    };
    let _ = agent.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Read);
    let _ = upstream.join();
    result
}

#[cfg(test)]
mod tests;
