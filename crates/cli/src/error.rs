//! Errors in the CLI's shape: what went wrong, then how to fix it.

use std::fmt;
use std::io;

use cave_vm_proto::ErrorMessage;

use crate::style::Paint;

/// Exit code for cave's own failures (docs/cli.md, "Exit codes").
pub const EXIT_FAILED: u8 = 1;
/// Exit code for a usage error, as clap uses.
pub const EXIT_USAGE: u8 = 2;

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

    /// The daemon answered `what` with a reply this cave does not expect,
    /// most often because it is an older daemon kept running for VMs.
    pub fn unexpected(what: &str) -> CliError {
        CliError::new(format!("the daemon sent an unexpected reply to {what}")).fix(
            "The daemon may be from an older cave, kept running for your VMs. Run `cave stop` in each project with a running VM (`cave ls` lists them), then try again",
        )
    }

    pub fn fix(mut self, fix: impl Into<String>) -> CliError {
        self.fix = Some(fix.into());
        self
    }
}

impl CliError {
    /// The error as printed, with `error:` and `fix:` styled by `paint`.
    pub fn render(&self, paint: Paint) -> String {
        let mut out = format!("{} {}", paint.bad("error:"), self.message);
        if let Some(fix) = &self.fix {
            out.push_str(&format!("\n  {} {fix}", paint.fix("fix:")));
        }
        out
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(Paint::OFF))
    }
}

impl From<ErrorMessage> for CliError {
    /// A `conflict`'s files go under the message, one per line.
    fn from(e: ErrorMessage) -> CliError {
        let mut message = e.message;
        for file in e.files.unwrap_or_default() {
            message.push_str("\n  ");
            message.push_str(&file);
        }
        CliError {
            message,
            fix: e.fix,
            code: EXIT_FAILED,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> CliError {
        let error = CliError::new(e.to_string());
        match io_fix(e.kind()) {
            Some(fix) => error.fix(fix),
            None => error,
        }
    }
}

/// How to fix the common file errors, which say nothing of it themselves.
fn io_fix(kind: io::ErrorKind) -> Option<&'static str> {
    Some(match kind {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            "Check that you can write to that folder, or choose another one"
        }
        io::ErrorKind::StorageFull => "Free some disk space, for example with `cave gc`",
        io::ErrorKind::NotFound => "Check that the path exists and is spelled as you meant",
        io::ErrorKind::AlreadyExists => "Remove or rename what is there, or choose another name",
        _ => return None,
    })
}

pub type Result<T> = std::result::Result<T, CliError>;

#[cfg(test)]
mod tests {
    use super::*;
    use cave_vm_proto::ErrorCode;

    #[test]
    fn common_file_errors_say_how_to_fix_them() {
        let e = CliError::from(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(e.fix.unwrap().contains("write to that folder"));
        let e = CliError::from(io::Error::from(io::ErrorKind::StorageFull));
        assert!(e.fix.unwrap().contains("cave gc"));
        assert!(CliError::from(io::Error::other("odd")).fix.is_none());
    }

    #[test]
    fn unexpected_replies_say_how_to_fix_them() {
        let e = CliError::unexpected("list");
        assert_eq!(e.message, "the daemon sent an unexpected reply to list");
        assert!(e.fix.unwrap().contains("cave stop"));
    }

    #[test]
    fn a_conflicts_files_are_listed_between_the_message_and_the_fix() {
        let mut e = ErrorMessage::new(ErrorCode::Conflict, "2 files changed").with_fix("keep one");
        e.files = Some(vec!["src/app.ts".into(), "package.json".into()]);
        assert_eq!(
            CliError::from(e).to_string(),
            "error: 2 files changed\n  src/app.ts\n  package.json\n  fix: keep one"
        );
    }
}
