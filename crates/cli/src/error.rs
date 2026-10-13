//! Errors in the CLI's shape: what went wrong, then how to fix it.

use std::fmt;
use std::io;

use cave_vm_proto::ErrorMessage;

/// Exit code for cave's own failures (docs/cli.md, "Exit codes").
pub const EXIT_FAILED: u8 = 1;

#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub fix: Option<String>,
    pub code: u8,
}

impl CliError {
    pub fn new(message: impl Into<String>) -> CliError {
        CliError {
            message: message.into(),
            fix: None,
            code: EXIT_FAILED,
        }
    }

    pub fn fix(mut self, fix: impl Into<String>) -> CliError {
        self.fix = Some(fix.into());
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error: {}", self.message)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n  fix: {fix}")?;
        }
        Ok(())
    }
}

impl From<ErrorMessage> for CliError {
    fn from(e: ErrorMessage) -> CliError {
        CliError {
            message: e.message,
            fix: e.fix,
            code: EXIT_FAILED,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> CliError {
        CliError::new(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CliError>;
