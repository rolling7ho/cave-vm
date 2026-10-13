//! The VMM's end of the control link: the socket pair the daemon starts it
//! with, as its standard input (`docs/protocol.md`, "VMM control").
//!
//! The daemon sends one request at a time and waits for the reply. The
//! process does the handshake, waits for `boot`, boots, and then serves
//! requests until `stop` or until the link closes, which also stops the VM.

use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cave_vm_proto::agent::Message;
use cave_vm_proto::frame::encode;
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{
    ErrorCode, ErrorMessage, FrameError, Hello, VERSIONS, read_frame, write_frame,
};

use crate::host;

/// What the control loop needs from a booted VM.
pub trait RunningVm {
    /// A new connection to the guest agent.
    fn agent_connection(&self) -> io::Result<UnixStream>;
}

/// How [`serve`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    /// The daemon asked to stop, and was told `ok`.
    Stop,
    /// The link closed: the daemon went away.
    Closed,
}

fn software() -> String {
    format!("cave-vmm {}", env!("CARGO_PKG_VERSION"))
}

fn read(link: &mut UnixStream) -> io::Result<Option<ToVmm>> {
    loop {
        match read_frame::<_, ToVmm>(link) {
            Ok(message) => return Ok(message),
            Err(FrameError::Io(e)) => return Err(e),
            // A bad frame gets an error and the link stays up, unless the
            // stream itself is broken.
            Err(e @ (FrameError::TooLarge(_) | FrameError::Truncated)) => {
                let _ = reply(link, &FromVmm::Error(e.to_error_message()));
                return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
            }
            Err(e) => reply(link, &FromVmm::Error(e.to_error_message()))?,
        }
    }
}

/// Sends one reply.
pub fn reply(link: &mut UnixStream, message: &FromVmm) -> io::Result<()> {
    write_frame(link, message).map_err(|e| match e {
        FrameError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    })
}

fn not_booted() -> FromVmm {
    FromVmm::Error(ErrorMessage::new(
        ErrorCode::NotRunning,
        "the VM has not booted yet",
    ))
}

/// Does the handshake, then waits for `boot`, answering `ping` meanwhile.
/// `None` means the daemon sent `stop` or went away first.
pub fn wait_for_boot(link: &mut UnixStream) -> io::Result<Option<VmmConfig>> {
    match read(link)? {
        Some(ToVmm::Hello(hello)) => match hello.accept(VERSIONS, software()) {
            Ok(ours) => reply(link, &FromVmm::Hello(ours))?,
            Err(e) => {
                reply(link, &FromVmm::Error(e))?;
                return Ok(None);
            }
        },
        Some(_) => {
            reply(
                link,
                &FromVmm::Error(ErrorMessage::new(
                    ErrorCode::BadRequest,
                    "the first message must be hello",
                )),
            )?;
            return Ok(None);
        }
        None => return Ok(None),
    }
    loop {
        match read(link)? {
            Some(ToVmm::Boot { config }) => return Ok(Some(config)),
            Some(ToVmm::Ping) => reply(link, &FromVmm::Pong)?,
            Some(ToVmm::Stop) => {
                reply(link, &FromVmm::Ok)?;
                return Ok(None);
            }
            Some(_) => reply(link, &not_booted())?,
            None => return Ok(None),
        }
    }
}

/// Serves requests for a booted VM until `stop` or until the link closes.
pub fn serve(link: &mut UnixStream, vm: &dyn RunningVm) -> io::Result<Ending> {
    // The last connection sent stays open here until the daemon's next
    // request, by which time it has received it: closing it while it is in
    // flight lost connections on macOS under load.
    let mut in_flight = None;
    loop {
        let request = read(link)?;
        drop(in_flight.take());
        match request {
            Some(ToVmm::AgentConnect) => match vm.agent_connection() {
                Ok(connection) => {
                    let frame = encode(&FromVmm::AgentConnection)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    host::send_with_fd(link, &frame, connection.as_fd())?;
                    in_flight = Some(connection);
                }
                Err(e) => reply(
                    link,
                    &FromVmm::Error(ErrorMessage::new(
                        ErrorCode::Io,
                        format!("cannot reach the guest agent: {e}"),
                    )),
                )?,
            },
            Some(ToVmm::Ping) => reply(link, &FromVmm::Pong)?,
            Some(ToVmm::Stop) => {
                reply(link, &FromVmm::Ok)?;
                return Ok(Ending::Stop);
            }
            Some(ToVmm::Boot { .. }) => reply(
                link,
                &FromVmm::Error(ErrorMessage::new(
                    ErrorCode::AlreadyExists,
                    "the VM has already booted",
                )),
            )?,
            Some(ToVmm::Hello(_)) => reply(
                link,
                &FromVmm::Error(ErrorMessage::new(
                    ErrorCode::BadRequest,
                    "hello was already done",
                )),
            )?,
            Some(ToVmm::Unknown) => unreachable!("decode refuses unknown types"),
            None => return Ok(Ending::Closed),
        }
    }
}

/// How long one attempt to greet the agent may take.
const AGENT_ATTEMPT: Duration = Duration::from_secs(2);
/// The pause between attempts while the guest boots.
const AGENT_RETRY: Duration = Duration::from_millis(20);

/// Waits until the guest agent answers `ping` and has set the clock, trying `connect` again
/// while the guest is still booting. Fails after `timeout`, or as soon as
/// `gone` says the VM stopped.
pub fn wait_for_agent(
    connect: &dyn Fn() -> io::Result<UnixStream>,
    gone: &dyn Fn() -> bool,
    timeout: Duration,
) -> Result<(), ErrorMessage> {
    let deadline = Instant::now() + timeout;
    loop {
        if gone() {
            return Err(ErrorMessage::new(
                ErrorCode::Internal,
                "the VM stopped while booting",
            ));
        }
        if let Ok(mut stream) = connect()
            && greet(&mut stream).is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ErrorMessage::new(
                ErrorCode::Internal,
                format!(
                    "the guest agent did not answer within {} seconds",
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(AGENT_RETRY);
    }
}

/// The handshake, then `ping` and the clock (`restored`) on a control
/// connection, as the protocol asks before `booted`.
fn greet(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(AGENT_ATTEMPT))?;
    let hello = Message::Hello(Hello::client(VERSIONS, software()));
    send_agent(stream, &hello)?;
    match read_agent(stream)? {
        Message::Hello(reply) if reply.agreed(VERSIONS).is_ok() => {}
        _ => return Err(io::ErrorKind::InvalidData.into()),
    }
    send_agent(stream, &Message::Control)?;
    send_agent(stream, &Message::Ping)?;
    if read_agent(stream)? != Message::Pong {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // The VM has no clock of its own, and TLS and package managers need
    // the right time.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let time_unix_ns = u64::try_from(now.as_nanos()).map_err(io::Error::other)?;
    send_agent(stream, &Message::Restored { time_unix_ns })?;
    match read_agent(stream)? {
        Message::Ok => Ok(()),
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
}

fn send_agent(stream: &mut UnixStream, message: &Message) -> io::Result<()> {
    write_frame(stream, message).map_err(|e| io::Error::other(e.to_string()))
}

fn read_agent(stream: &mut UnixStream) -> io::Result<Message> {
    match read_frame::<_, Message>(stream) {
        Ok(Some(message)) => Ok(message),
        Ok(None) => Err(io::ErrorKind::UnexpectedEof.into()),
        Err(e) => Err(io::Error::other(e.to_string())),
    }
}

#[cfg(test)]
mod tests;
