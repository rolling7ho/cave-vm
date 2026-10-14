//! Errors in cave's shape: what went wrong, then how to fix it.

use std::fmt;
use std::io;

use cave_vm_proto::{ErrorCode, ErrorMessage, FrameError};

/// An operation that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The daemon's error code, when the daemon reported the error.
    pub code: Option<ErrorCode>,
    pub message: String,
    /// How to fix the problem, when there is a known way.
    pub fix: Option<String>,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Error {
        Error {
            code: None,
            message: message.into(),
            fix: None,
        }
    }

    pub(crate) fn fix(mut self, fix: impl Into<String>) -> Error {
        self.fix = Some(fix.into());
        self
    }

    /// The error for a reply that does not answer the request.
    pub(crate) fn unexpected(request: &str, reply: impl fmt::Debug) -> Error {
        Error::new(format!(
            "the cave daemon sent an unexpected reply to {request}: {reply:?}"
        ))
        .fix("Update cave so the SDK and the daemon match")
    }

    /// The error for a connection that broke.
    pub(crate) fn daemon_gone(e: impl fmt::Display) -> Error {
        Error::new(format!("lost the connection to the cave daemon: {e}"))
            .fix("Try again; if it keeps happening, run `cave doctor`")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n  fix: {fix}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<ErrorMessage> for Error {
    fn from(e: ErrorMessage) -> Error {
        Error {
            code: Some(e.code),
            message: e.message,
            fix: e.fix,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::new(e.to_string())
    }
}

impl From<FrameError> for Error {
    fn from(e: FrameError) -> Error {
        Error::daemon_gone(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
