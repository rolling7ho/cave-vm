//! Credentials (design, "Credentials"; P5.3): a call names a credential,
//! the daemon resolves its value from the project's `.env`, and the value
//! goes into the command's environment on the agent link only. It never
//! goes back to the client: output that prints it has the exact value
//! replaced with the name in brackets, `[API_KEY]` [D9]. Redaction is best
//! effort, since a command can still leak a value through files or the
//! network.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use cave_vm_proto::{ErrorCode, ErrorMessage};

/// The file in the project folder that credentials are read from.
pub const ENV_FILE: &str = ".env";

/// Whether `name` can be a credential's name: a shell variable name.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The variables in a `.env` file. Lines are `NAME=value`, optionally
/// after `export `. Blank lines and lines starting with `#` are skipped. A
/// value in single quotes is taken as written; one in double quotes has
/// `\n`, `\"` and `\\` unescaped. An unquoted value ends at ` #`, and is
/// trimmed. A later line for the same name wins.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !valid_name(name) {
            continue;
        }
        vars.insert(name.to_string(), unquote(value.trim()));
    }
    vars
}

fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'') {
        return inner.split('\'').next().unwrap_or_default().to_string();
    }
    if let Some(inner) = value.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        return out;
    }
    let end = value.find(" #").unwrap_or(value.len());
    value[..end].trim_end().to_string()
}

/// Looks up each of `names` in `project`'s `.env`, in order.
pub fn resolve(project: &Path, names: &[String]) -> Result<Vec<(String, String)>, ErrorMessage> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let path = project.join(ENV_FILE);
    if let Some(name) = names.iter().find(|n| !valid_name(n)) {
        return Err(ErrorMessage::new(
            ErrorCode::BadRequest,
            format!("{name:?} is not a credential name"),
        )
        .with_fix("Name a variable from the project's .env, such as API_KEY"));
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let vars = parse_env(&text);
    names
        .iter()
        .map(|name| match vars.get(name) {
            Some(value) => Ok((name.clone(), value.clone())),
            None => Err(ErrorMessage::new(
                ErrorCode::NotFound,
                format!("no credential named {name} in {}", path.display()),
            )
            .with_fix(format!("Add a line `{name}=<value>` to {}", path.display()))),
        })
        .collect()
}

/// Replaces credential values in a stream of output chunks with their
/// names in brackets, including a value split across chunks: the end of a
/// chunk that could be the start of a value is held back until the next
/// chunk shows whether it is.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Values and their replacements, longest value first so that a value
    /// containing another is replaced whole.
    secrets: Vec<(Vec<u8>, Vec<u8>)>,
    held: Vec<u8>,
}

impl Redactor {
    /// A redactor for `credentials`, as names and values. Empty values
    /// cannot be redacted and are left out.
    pub fn new(credentials: &[(String, String)]) -> Redactor {
        let mut secrets: Vec<(Vec<u8>, Vec<u8>)> = credentials
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (value.as_bytes().to_vec(), format!("[{name}]").into_bytes()))
            .collect();
        secrets.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        secrets.dedup_by(|a, b| a.0 == b.0);
        Redactor {
            secrets,
            held: Vec::new(),
        }
    }

    /// The redacted output for `chunk`, which may hold back its end.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return chunk.to_vec();
        }
        self.held.extend_from_slice(chunk);
        self.scan(false)
    }

    /// Whatever is held back, redacted, once the output has ended.
    pub fn finish(&mut self) -> Vec<u8> {
        self.scan(true)
    }

    fn scan(&mut self, last: bool) -> Vec<u8> {
        let input = std::mem::take(&mut self.held);
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        'scan: while i < input.len() {
            let rest = &input[i..];
            for (value, replacement) in &self.secrets {
                if rest.starts_with(value) {
                    out.extend_from_slice(replacement);
                    i += value.len();
                    continue 'scan;
                }
            }
            if !last
                && self
                    .secrets
                    .iter()
                    .any(|(value, _)| value.len() > rest.len() && value.starts_with(rest))
            {
                self.held = rest.to_vec();
                break;
            }
            out.push(input[i]);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests;
