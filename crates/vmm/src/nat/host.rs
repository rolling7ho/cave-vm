//! The host side of forwarded connections: a trait over host sockets, so
//! tests use in-process socket pairs instead of the network.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, SocketAddrV4, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// A connected host socket.
pub trait HostStream: Read + Write + Send {
    fn try_clone_stream(&self) -> io::Result<Box<dyn HostStream>>;
    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()>;
}

impl HostStream for TcpStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn HostStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

impl HostStream for UnixStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn HostStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

/// Opens host connections for the guest. Called on a thread of its own, so
/// it may block.
pub trait Connector: Send + Sync {
    fn connect(&self, addr: SocketAddrV4) -> io::Result<Box<dyn HostStream>>;
}

/// Real TCP connections.
#[derive(Debug, Clone)]
pub struct TcpConnector {
    pub timeout: Duration,
}

impl Default for TcpConnector {
    fn default() -> TcpConnector {
        // Roughly what Linux gives up after with its default SYN retries.
        TcpConnector {
            timeout: Duration::from_secs(60),
        }
    }
}

impl Connector for TcpConnector {
    fn connect(&self, addr: SocketAddrV4) -> io::Result<Box<dyn HostStream>> {
        let stream = TcpStream::connect_timeout(&SocketAddr::V4(addr), self.timeout)?;
        stream.set_nodelay(true)?;
        Ok(Box::new(stream))
    }
}
