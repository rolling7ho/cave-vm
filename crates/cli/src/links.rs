//! Sign-in links from an agent in the VM, opened on the Mac (D56, decision
//! 0027).
//!
//! `cave agent` installs [`HELPER`] in the VM and starts the agent with
//! `BROWSER` pointing at it. The helper writes the link to its terminal
//! inside a private escape sequence; [`Relay`] takes the sequence out of
//! the output `cave` relays, and [`open`] opens the link with `open` if
//! [`allowed`] says so.

use std::process::{Command, Stdio};

/// Where the helper goes in the VM, for `BROWSER`.
pub const HELPER_PATH: &str = "$HOME/.local/bin/cave-open";

/// Shell commands that install the helper in the VM, replacing an older
/// copy.
pub const INSTALL_HELPER: &str = "mkdir -p \"$HOME/.local/bin\" && \
cat > \"$HOME/.local/bin/cave-open\" <<'CAVE'
#!/bin/sh
# Installed by cave agent: asks cave on the Mac to open a sign-in link (D56).
[ -w /dev/tty ] || exit 1
printf '\\033]7733;cave-open;%s\\007' \"$1\" > /dev/tty
CAVE
chmod +x \"$HOME/.local/bin/cave-open\"";

/// The start of the helper's escape sequence; [`END`] ends it.
const MARKER: &[u8] = b"\x1b]7733;cave-open;";

/// BEL, which ends the sequence.
const END: u8 = 0x07;

/// The longest link kept; a longer sequence is passed through as text.
const MAX_LINK: usize = 8 << 10;

/// The hosts whose links are opened, and their subdomains.
const HOSTS: [&str; 3] = ["claude.ai", "claude.com", "anthropic.com"];

/// Whether `link` may be opened on the Mac: `https://`, no spaces or
/// control characters, no user name or port, on one of [`HOSTS`].
pub fn allowed(link: &str) -> bool {
    let Some(rest) = link.strip_prefix("https://") else {
        return false;
    };
    if link.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if host.is_empty() || host.contains(['@', ':']) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Opens `link` in the Mac's default browser. Returns whether it was
/// allowed and handed to `open`.
pub fn open(link: &str) -> bool {
    if !allowed(link) {
        return false;
    }
    Command::new("/usr/bin/open")
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Takes the helper's sequences out of a terminal's output, which may
/// split them across chunks.
#[derive(Default)]
pub struct Relay {
    /// Bytes that may be the start of a sequence, held for the next chunk.
    held: Vec<u8>,
}

impl Relay {
    /// Returns the output to show, and the links found in it.
    pub fn feed(&mut self, data: &[u8]) -> (Vec<u8>, Vec<String>) {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(data);
        let mut out = Vec::with_capacity(buf.len());
        let mut links = Vec::new();
        let mut at = 0;
        while at < buf.len() {
            let Some(start) = find(&buf[at..], MARKER).map(|i| at + i) else {
                // Hold a tail that could still become the marker.
                let tail = (1..MARKER.len())
                    .rev()
                    .find(|&n| buf.len() - at >= n && buf.ends_with(&MARKER[..n]))
                    .unwrap_or(0);
                out.extend_from_slice(&buf[at..buf.len() - tail]);
                self.held = buf[buf.len() - tail..].to_vec();
                return (out, links);
            };
            out.extend_from_slice(&buf[at..start]);
            let body = start + MARKER.len();
            match buf[body..].iter().position(|&b| b == END) {
                Some(len) if len <= MAX_LINK => {
                    links.push(String::from_utf8_lossy(&buf[body..body + len]).into_owned());
                    at = body + len + 1;
                }
                None if buf.len() - body <= MAX_LINK => {
                    self.held = buf[start..].to_vec();
                    return (out, links);
                }
                // Too long to be a link: shown as it is.
                _ => {
                    out.push(buf[start]);
                    at = start + 1;
                }
            }
        }
        (out, links)
    }

    /// Whatever is still held, for when the output ends.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

/// Where `needle` first appears in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(link: &str) -> Vec<u8> {
        [MARKER, link.as_bytes(), &[END]].concat()
    }

    #[test]
    fn only_sign_in_hosts_over_https_are_allowed() {
        assert!(allowed(
            "https://claude.com/cai/oauth/authorize?code=true&state=x"
        ));
        assert!(allowed("https://platform.claude.com/oauth/code/callback"));
        assert!(allowed("https://claude.ai/login"));
        assert!(allowed("https://console.anthropic.com"));
        assert!(!allowed("http://claude.com/login"));
        assert!(!allowed("https://claude.com.example.org/login"));
        assert!(!allowed("https://evilclaude.com/login"));
        assert!(!allowed("https://claude.com@example.org/"));
        assert!(!allowed("https://claude.com:8443/"));
        assert!(!allowed("https://example.org/?next=https://claude.com"));
        assert!(!allowed("https://claude.com/a b"));
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("https://"));
    }

    #[test]
    fn the_sequence_is_taken_out_of_the_output() {
        let mut relay = Relay::default();
        let data = [
            b"before ".as_slice(),
            &seq("https://claude.com/x"),
            b" after",
        ]
        .concat();
        assert_eq!(
            relay.feed(&data),
            (
                b"before  after".to_vec(),
                vec!["https://claude.com/x".to_string()]
            )
        );
        assert!(relay.finish().is_empty());
    }

    #[test]
    fn a_sequence_split_across_chunks_is_put_back_together() {
        let data = [b"a".as_slice(), &seq("https://claude.ai/y"), b"b"].concat();
        for split in 0..=data.len() {
            let mut relay = Relay::default();
            let (mut out, mut links) = relay.feed(&data[..split]);
            let (more, found) = relay.feed(&data[split..]);
            out.extend(more);
            out.extend(relay.finish());
            links.extend(found);
            assert_eq!(out, b"ab", "split at {split}");
            assert_eq!(links, ["https://claude.ai/y"], "split at {split}");
        }
    }

    #[test]
    fn other_escape_sequences_go_through() {
        let mut relay = Relay::default();
        let (out, links) = relay.feed(b"\x1b[2J\x1b]0;title\x07text\x1b");
        assert_eq!(out, b"\x1b[2J\x1b]0;title\x07text");
        assert!(links.is_empty());
        assert_eq!(relay.feed(b"[0m").0, b"\x1b[0m");
    }

    #[test]
    fn an_unended_sequence_that_is_too_long_is_shown() {
        let mut relay = Relay::default();
        let mut data = MARKER.to_vec();
        data.extend(std::iter::repeat_n(b'x', MAX_LINK + 1));
        let (out, links) = relay.feed(&data);
        assert!(links.is_empty());
        let mut all = out;
        all.extend(relay.finish());
        assert_eq!(all, data);
    }
}
