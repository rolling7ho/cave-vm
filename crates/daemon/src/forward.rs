//! Port forwarding to the Mac (`-p 3000`): a listener on `localhost:<mac>`,
//! and for each accepted connection a new agent connection that asks the
//! agent for `port_forward` and then carries raw bytes both ways
//! (docs/protocol.md, "port_forward"). Only localhost listens.

use std::io;
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use cave_vm_proto::agent::Message;
use cave_vm_proto::daemon::PortMap;
use cave_vm_proto::{ErrorCode, ErrorMessage};

use crate::relay;

/// How often a listener checks whether it should stop.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Kept while a forwarded connection is open: what counts the VM as in
/// use.
pub type Held = Box<dyn Send>;

/// Opens a new agent connection to the VM.
pub type Connect = Arc<dyn Fn() -> Result<(UnixStream, Held), ErrorMessage> + Send + Sync>;

/// A listener on the Mac port, bound but not serving yet, so a port that
/// is in use is reported before the VM boots.
pub struct Bound {
    map: PortMap,
    listener: TcpListener,
}

/// A serving forward. Dropping it stops the listener; connections already
/// open end when either side closes them.
pub struct Forward {
    pub map: PortMap,
    stop: Arc<AtomicBool>,
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// The flag that asked for `map`, as the policy echoes it.
pub fn flag(map: PortMap) -> String {
    if map.mac == map.vm {
        format!("-p {}", map.mac)
    } else {
        format!("-p {}:{}", map.mac, map.vm)
    }
}

/// Binds `localhost:<mac>`.
pub fn bind(map: PortMap) -> Result<Bound, ErrorMessage> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, map.mac)).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse {
            ErrorMessage::new(
                ErrorCode::AlreadyExists,
                format!("port {} on this Mac is already in use", map.mac),
            )
            .with_fix(format!(
                "Pick another Mac port: cave run -p {}:{}",
                map.mac.saturating_add(1),
                map.vm
            ))
        } else {
            ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot listen on port {} on this Mac: {e}", map.mac),
            )
        }
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| ErrorMessage::new(ErrorCode::Io, format!("port {}: {e}", map.mac)))?;
    Ok(Bound { map, listener })
}

impl Bound {
    /// Accepts connections on its own thread until the [`Forward`] is
    /// dropped.
    pub fn serve(self, connect: Connect) -> Forward {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let (listener, port) = (self.listener, self.map.vm);
        thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        let connect = Arc::clone(&connect);
                        thread::spawn(move || {
                            if let Err(e) = carry(tcp, &connect, port) {
                                eprintln!("cave-daemon: port {port}: {}", e.message);
                            }
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        eprintln!("cave-daemon: port {port}: {e}");
                        thread::sleep(ACCEPT_POLL);
                    }
                }
            }
        });
        Forward {
            map: self.map,
            stop,
        }
    }
}

/// One forwarded connection. A port nothing listens on in the VM closes
/// the Mac side at once, as a refused connection would.
fn carry(tcp: TcpStream, connect: &Connect, port: u16) -> Result<(), ErrorMessage> {
    let io_error = |e: io::Error| ErrorMessage::new(ErrorCode::Io, e.to_string());
    tcp.set_nonblocking(false).map_err(io_error)?;
    let (mut agent, _held) = connect()?;
    relay::open(&mut agent, &Message::PortForward { port })?;
    match relay::read_agent(&mut agent)? {
        Message::PortForwardOk => {}
        Message::Error(e) => return Err(e),
        other => {
            return Err(ErrorMessage::new(
                ErrorCode::Io,
                format!("the guest agent answered port_forward with {other:?}"),
            ));
        }
    }
    let (mut tcp_in, mut agent_out) = (
        tcp.try_clone().map_err(io_error)?,
        agent.try_clone().map_err(io_error)?,
    );
    let up = thread::spawn(move || {
        let _ = io::copy(&mut tcp_in, &mut agent_out);
        let _ = agent_out.shutdown(Shutdown::Write);
    });
    let (mut agent_in, mut tcp_out) = (agent, tcp);
    let _ = io::copy(&mut agent_in, &mut tcp_out);
    let _ = tcp_out.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests;
