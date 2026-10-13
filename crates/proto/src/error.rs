//! The `error` message, which ends an operation.

use serde::{Deserialize, Serialize};

/// Stable error codes for programs. Version 1's set; a code from a newer
/// peer decodes as [`ErrorCode::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadFrame,
    FrameTooLarge,
    UnsupportedVersion,
    UnknownType,
    BadRequest,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotRunning,
    Busy,
    Io,
    Internal,
    /// `pull` found files changed on both sides; `files` lists them.
    Conflict,
    #[serde(other)]
    Unknown,
}

/// `{"type": "error", ...}`. `message` and `fix` are for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// With `unsupported_version`: the versions the sender speaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<Vec<u32>>,
    /// With `conflict`: the files changed on both sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

impl ErrorMessage {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorMessage {
        ErrorMessage {
            code,
            message: message.into(),
            fix: None,
            supported: None,
            files: None,
        }
    }

    /// Adds how to fix the problem.
    pub fn with_fix(mut self, fix: impl Into<String>) -> ErrorMessage {
        self.fix = Some(fix.into());
        self
    }
}

impl std::fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n{fix}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ErrorMessage {}
