//! Framing: a 4-byte big-endian length, then that many bytes of UTF-8 JSON
//! holding one message object.

use std::fmt;
use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::MAX_FRAME;
use crate::error::{ErrorCode, ErrorMessage};

/// A message enum for one link, decoded by [`decode`].
pub trait Message: Serialize + DeserializeOwned {
    /// Whether this is the catch-all for a `type` this build does not know.
    fn is_unknown(&self) -> bool;

    /// Checks rules the types alone do not express. The error describes
    /// the problem for the peer.
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Why a frame could not be read, written or decoded.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// A length of zero.
    Empty,
    /// A length over [`MAX_FRAME`]. The body is left unread.
    TooLarge(usize),
    /// The connection ended inside a frame.
    Truncated,
    /// The body is not a JSON object.
    NotJson(String),
    /// The object has no `type` string.
    NoType,
    /// A `type` this build does not know.
    UnknownType(String),
    /// A known `type` with missing or malformed fields.
    BadMessage {
        kind: String,
        reason: String,
    },
}

impl FrameError {
    /// The code to send the peer in an `error` message.
    pub fn code(&self) -> ErrorCode {
        match self {
            FrameError::Io(_) => ErrorCode::Io,
            FrameError::Empty | FrameError::Truncated | FrameError::NotJson(_) => {
                ErrorCode::BadFrame
            }
            FrameError::TooLarge(_) => ErrorCode::FrameTooLarge,
            FrameError::NoType | FrameError::BadMessage { .. } => ErrorCode::BadRequest,
            FrameError::UnknownType(_) => ErrorCode::UnknownType,
        }
    }

    /// The `error` message to send the peer before closing.
    pub fn to_error_message(&self) -> ErrorMessage {
        ErrorMessage::new(self.code(), self.to_string())
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "connection error: {e}"),
            FrameError::Empty => f.write_str("frame of length 0"),
            FrameError::TooLarge(len) => {
                write!(f, "frame of {len} bytes is over the {MAX_FRAME}-byte limit")
            }
            FrameError::Truncated => f.write_str("connection ended inside a frame"),
            FrameError::NotJson(e) => write!(f, "frame is not a JSON object: {e}"),
            FrameError::NoType => f.write_str("message has no \"type\""),
            FrameError::UnknownType(kind) => write!(f, "unknown message type {kind:?}"),
            FrameError::BadMessage { kind, reason } => {
                write!(f, "malformed {kind:?} message: {reason}")
            }
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> FrameError {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    }
}

/// Encodes `message` as a whole frame, length included.
pub fn encode<M: Serialize>(message: &M) -> Result<Vec<u8>, FrameError> {
    let mut out = vec![0; 4];
    serde_json::to_writer(&mut out, message).map_err(|e| FrameError::NotJson(e.to_string()))?;
    let len = out.len() - 4;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    out[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(out)
}

/// Decodes a frame body (without the length).
pub fn decode<M: Message>(body: &[u8]) -> Result<M, FrameError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| FrameError::NotJson(e.to_string()))?;
    if !value.is_object() {
        return Err(FrameError::NotJson("not an object".into()));
    }
    let kind = match value.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        _ => return Err(FrameError::NoType),
    };
    let message: M = serde_json::from_value(value).map_err(|e| FrameError::BadMessage {
        kind: kind.clone(),
        reason: e.to_string(),
    })?;
    if message.is_unknown() {
        return Err(FrameError::UnknownType(kind));
    }
    message
        .validate()
        .map_err(|reason| FrameError::BadMessage { kind, reason })?;
    Ok(message)
}

/// Writes one frame.
pub fn write_frame<W: Write, M: Serialize>(w: &mut W, message: &M) -> Result<(), FrameError> {
    w.write_all(&encode(message)?)?;
    Ok(())
}

/// Reads one frame. `Ok(None)` means the peer closed the connection
/// between frames.
pub fn read_frame<R: Read, M: Message>(r: &mut R) -> Result<Option<M>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    decode(&body).map(Some)
}
