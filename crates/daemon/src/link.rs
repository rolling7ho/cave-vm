//! The daemon's end of a VMM control link (docs/protocol.md, "VMM control").

use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use cave_vm_proto::vmm::{FromVmm, ToVmm};
use cave_vm_proto::{ErrorMessage, FrameError, Hello, VERSIONS, read_frame, write_frame};

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

    /// Like [`request`](Self::request), with the usual timeout.
    pub fn call(&mut self, message: &ToVmm) -> Result<FromVmm, LinkError> {
        self.request(message, REPLY_TIMEOUT)
    }
}

pub fn daemon_software() -> String {
    format!("cave-daemon {}", env!("CARGO_PKG_VERSION"))
}
