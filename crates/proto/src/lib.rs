//! Message types shared by the daemon, the guest agent and the CLI.
//!
//! The wire format is specified in `docs/protocol.md`: frames of a 4-byte
//! big-endian length and a JSON object ([`frame`]), a `hello` handshake that
//! picks the protocol version ([`handshake`]), and the messages themselves.
//! [`agent`] has the guest agent link's messages, and [`daemon`] the daemon
//! API's requests and replies.

#![deny(unsafe_code)]

pub mod agent;
pub mod daemon;
mod data;
pub mod error;
pub mod frame;
pub mod handshake;

pub use data::Data;
pub use error::{ErrorCode, ErrorMessage};
pub use frame::{FrameError, read_frame, write_frame};
pub use handshake::Hello;

/// The protocol versions this build speaks, oldest first.
pub const VERSIONS: &[u32] = &[1];

/// The guest agent's vsock port.
pub const AGENT_PORT: u32 = 1024;

/// Largest frame body, in bytes.
pub const MAX_FRAME: usize = 1 << 20;

/// Largest chunk of binary data a sender puts in one message, before base64.
pub const MAX_CHUNK: usize = 256 << 10;
