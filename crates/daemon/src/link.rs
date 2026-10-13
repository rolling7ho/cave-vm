//! The daemon's end of a VMM control link (docs/protocol.md, "VMM control").

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use cave_vm_proto::frame::decode;
use cave_vm_proto::vmm::{FromVmm, ToVmm};
use cave_vm_proto::{
    ErrorMessage, FrameError, Hello, MAX_FRAME, VERSIONS, read_frame, write_frame,
};
use cave_vm_vmm::host;

/// How long most requests may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a boot may take, until the guest agent answers.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a VMM did not answer as expected.
#[derive(Debug)]
pub enum LinkError {
    /// The link failed or closed, usually because the VMM exited.
    Io(io::Error),
    Frame(FrameError),
    /// The VMM answered with an `error`.
    Vmm(ErrorMessage),
    /// The VMM sent something else.
    Unexpected(FromVmm),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Io(e) => write!(f, "{e}"),
            LinkError::Frame(e) => write!(f, "{e}"),
            LinkError::Vmm(e) => write!(f, "{}", e.message),
            LinkError::Unexpected(m) => write!(f, "unexpected reply {m:?}"),
        }
    }
}

/// A connected, handshaken control link.
pub struct VmmLink {
    stream: UnixStream,
}

impl VmmLink {
    /// Does the handshake on a new link.
    pub fn handshake(stream: UnixStream) -> Result<VmmLink, LinkError> {
        let mut link = VmmLink { stream };
        let hello = ToVmm::Hello(Hello::client(VERSIONS, daemon_software()));
        match link.request(&hello, REPLY_TIMEOUT)? {
            FromVmm::Hello(reply) => {
                reply.agreed(VERSIONS).map_err(LinkError::Vmm)?;
                Ok(link)
            }
            other => Err(LinkError::Unexpected(other)),
        }
    }

    /// Sends `message` and reads the reply, waiting at most `timeout`. An
    /// `error` reply is returned as [`LinkError::Vmm`].
    pub fn request(&mut self, message: &ToVmm, timeout: Duration) -> Result<FromVmm, LinkError> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(LinkError::Io)?;
        write_frame(&mut self.stream, message).map_err(LinkError::Frame)?;
        match read_frame::<_, FromVmm>(&mut self.stream) {
            Ok(Some(FromVmm::Error(e))) => Err(LinkError::Vmm(e)),
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(LinkError::Io(io::ErrorKind::UnexpectedEof.into())),
            Err(FrameError::Io(e)) => Err(LinkError::Io(e)),
            Err(e) => Err(LinkError::Frame(e)),
        }
    }

    /// A new connection to the guest agent: the VMM answers
    /// `agent_connect` with the connection attached as a file descriptor
    /// (D26). The caller speaks the agent protocol on it from the handshake.
    pub fn agent_connect(&mut self) -> Result<UnixStream, LinkError> {
        self.stream
            .set_read_timeout(Some(REPLY_TIMEOUT))
            .map_err(LinkError::Io)?;
        write_frame(&mut self.stream, &ToVmm::AgentConnect).map_err(LinkError::Frame)?;
        // The descriptor comes with the first bytes of the reply.
        let mut head = [0u8; 4];
        let mut got = 0;
        let mut fd = None;
        while got < head.len() {
            let (n, received) =
                host::recv_with_fd(&self.stream, &mut head[got..]).map_err(LinkError::Io)?;
            if n == 0 {
                return Err(LinkError::Io(io::ErrorKind::UnexpectedEof.into()));
            }
            fd = fd.or(received);
            got += n;
        }
        let len = u32::from_be_bytes(head) as usize;
        if len > MAX_FRAME {
            return Err(LinkError::Frame(FrameError::TooLarge(len)));
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).map_err(LinkError::Io)?;
        match decode::<FromVmm>(&body).map_err(LinkError::Frame)? {
            FromVmm::AgentConnection => match fd {
                Some(fd) => Ok(UnixStream::from(fd)),
                None => Err(LinkError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "agent_connection came without a connection",
                ))),
            },
            FromVmm::Error(e) => Err(LinkError::Vmm(e)),
            other => Err(LinkError::Unexpected(other)),
        }
    }

    /// Like [`request`](Self::request), with the usual timeout.
    pub fn call(&mut self, message: &ToVmm) -> Result<FromVmm, LinkError> {
        self.request(message, REPLY_TIMEOUT)
    }
}

pub fn daemon_software() -> String {
    format!("cave-daemon {}", env!("CARGO_PKG_VERSION"))
}
