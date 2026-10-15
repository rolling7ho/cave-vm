//! Colour, bold and underline for the CLI's output, on a terminal only.
//! `NO_COLOR` (any value) or `TERM=dumb` turns them off, and so does
//! output to a file or a pipe (docs/cli.md, "Output").

use std::io::IsTerminal;

/// Whether to style text for one stream. Functions that build output
/// take one, so tests can render it plain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint(bool);

impl Paint {
    /// No styling.
    pub const OFF: Paint = Paint(false);

    /// For text printed to standard output.
    pub fn stdout() -> Paint {
        Paint(wanted(std::io::stdout().is_terminal()))
    }

    /// For text printed to standard error.
    pub fn stderr() -> Paint {
        Paint(wanted(std::io::stderr().is_terminal()))
    }

    fn wrap(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }

    /// Bold and underlined, for headings.
    pub fn heading(self, text: &str) -> String {
        self.wrap("1;4", text)
    }

    /// Bold red, for errors and what was refused.
    pub fn bad(self, text: &str) -> String {
        self.wrap("1;31", text)
    }

    /// Green, for what went well.
    pub fn good(self, text: &str) -> String {
        self.wrap("32", text)
    }

    /// Yellow, for what needs a look.
    pub fn notice(self, text: &str) -> String {
        self.wrap("33", text)
    }

    /// `text` with a leading `warning:` in bold yellow.
    pub fn warning(self, text: &str) -> String {
        match text.strip_prefix("warning:") {
            Some(rest) => format!("{}{rest}", self.wrap("1;33", "warning:")),
            None => text.to_string(),
        }
    }

    /// Bold green, for how to fix a problem.
    pub fn fix(self, text: &str) -> String {
        self.wrap("1;32", text)
    }
}

fn wanted(terminal: bool) -> bool {
    terminal
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_leaves_text_alone() {
        assert_eq!(Paint::OFF.bad("error:"), "error:");
        assert_eq!(Paint(true).bad("error:"), "\x1b[1;31merror:\x1b[0m");
        assert_eq!(Paint(true).heading("NAME"), "\x1b[1;4mNAME\x1b[0m");
        assert_eq!(
            Paint(true).warning("warning: x"),
            "\x1b[1;33mwarning:\x1b[0m x"
        );
        assert_eq!(Paint(true).warning("x"), "x");
    }
}
