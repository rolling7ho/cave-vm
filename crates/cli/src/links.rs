//! Sign-in links from an agent in the VM, opened on the Mac (D56, decision
//! 0027).
//!
//! `cave agent` installs [`HELPER`] in the VM and starts the agent with
//! `BROWSER` pointing at it. The helper writes the link to its terminal
//! inside a private escape sequence; [`Relay`] takes the sequence out of
//! the output `cave` relays, and [`open`] opens the link with `open` if
//! [`allowed`] says so.
//!
//! A sign-in link whose `redirect_uri` is `http://localhost:<port>/...`
//! sends the browser back to a server the agent runs in the VM, which the
//! Mac cannot reach. For those, [`relay_callback`] listens on that port on
//! the Mac's loopback until the browser arrives, and passes its request on
//! to the same port inside the VM.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_proto::daemon::VmId;

use crate::copy::exec_in_vm;

/// How long the Mac waits for the browser to come back after sign-in.
const CALLBACK_WAIT: Duration = Duration::from_secs(600);

/// The most of the browser's request read before giving up on it.
const MAX_REQUEST: usize = 16 << 10;

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

/// Opens `link` in the Mac's default browser, after starting
/// [`relay_callback`] if it returns to `localhost`. Returns whether it was
/// allowed and handed to `open`.
pub fn open(link: &str, vm: &VmId) -> bool {
    if !allowed(link) {
        return false;
    }
    if let Some((port, path)) = local_callback(link) {
        relay_callback(vm.clone(), port, path);
    }
    Command::new("/usr/bin/open")
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// The port and path of `link`'s `redirect_uri` when it is
/// `http://localhost:<port>/<path>`.
pub fn local_callback(link: &str) -> Option<(u16, String)> {
    let query = link.split_once('?')?.1;
    let query = query.split('#').next()?;
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("redirect_uri="))?;
    let uri = percent_decode(value)?;
    let rest = uri.strip_prefix("http://localhost:")?;
    let split = rest.find('/').unwrap_or(rest.len());
    let port = rest[..split].parse().ok().filter(|&p: &u16| p >= 1024)?;
    let path = if split == rest.len() {
        "/"
    } else {
        &rest[split..]
    };
    Some((port, path.to_string()))
}

/// `%XX` escapes decoded, and `+` as a space.
fn percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
            }
            b'+' => out.push(b' '),
            _ => out.push(b),
        }
    }
    String::from_utf8(out).ok()
}

/// Listens on `port` on the Mac's loopback, IPv4 and IPv6, in the
/// background. The first request for `path` is passed on to the same port
/// inside `vm`, with `curl` in the VM, and the browser is sent where the
/// agent's server sends it. Gives up after [`CALLBACK_WAIT`], or at once
/// if the port is taken on the Mac.
fn relay_callback(vm: VmId, port: u16, path: String) {
    let listeners: Vec<TcpListener> = [
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)),
        TcpListener::bind((Ipv6Addr::LOCALHOST, port)),
    ]
    .into_iter()
    .flatten()
    .filter(|l| l.set_nonblocking(true).is_ok())
    .collect();
    if listeners.is_empty() {
        return;
    }
    thread::spawn(move || {
        let deadline = Instant::now() + CALLBACK_WAIT;
        while Instant::now() < deadline {
            for listener in &listeners {
                if let Ok((stream, _)) = listener.accept()
                    && answer(stream, &path, |target| in_vm(&vm, port, target))
                {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Requests `target` from `port` inside `vm` with `curl`. Returns the
/// status code and where it redirects, if it ran.
fn in_vm(vm: &VmId, port: u16, target: &str) -> Option<(String, String)> {
    let url = format!("http://localhost:{port}{target}");
    let mut out = Vec::new();
    let argv = [
        "curl",
        "-s",
        "--max-time",
        "30",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code} %{redirect_url}",
        &url,
    ];
    match exec_in_vm(vm, &argv, None, &mut out) {
        Ok((0, _)) => {
            let out = String::from_utf8_lossy(&out).into_owned();
            let (code, location) = out.split_once(' ').unwrap_or((&out, ""));
            Some((code.to_string(), location.trim().to_string()))
        }
        _ => None,
    }
}

/// Answers one browser request, passing the callback to `forward`.
/// Returns whether it was the callback.
fn answer(
    mut stream: TcpStream,
    path: &str,
    forward: impl FnOnce(&str) -> Option<(String, String)>,
) -> bool {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(target) = request_target(&mut stream) else {
        return false;
    };
    if target.split('?').next() != Some(path) {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return false;
    }
    let response = match forward(&target) {
        Some((code, location)) if code.starts_with('3') && allowed(&location) => format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
        Some((code, _)) if code.starts_with('2') || code.starts_with('3') => {
            page("Signed in. The agent in your cave VM has it; you can close this tab.")
        }
        _ => page(
            "The agent in your cave VM did not take the sign-in. Go back to Terminal and try again.",
        ),
    };
    let _ = stream.write_all(response.as_bytes());
    true
}

/// The request target of an HTTP `GET`, such as `/callback?code=...`.
fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).ok().filter(|&n| n > 0)?;
        head.extend_from_slice(&buf[..n]);
        if head.len() > MAX_REQUEST {
            return None;
        }
    }
    let line = head.split(|&b| b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    let target = match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => target,
        _ => return None,
    };
    let safe = target.starts_with('/')
        && target
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\'));
    safe.then(|| target.to_string())
}

/// A small HTML page with `text`.
fn page(text: &str) -> String {
    let body = format!("<!doctype html><meta charset=utf-8><title>cave</title><p>{text}</p>\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
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

    /// Sends `request` to [`answer`] and returns its response.
    fn ask(request: &str, forward: Option<(String, String)>) -> (bool, String) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut asked = None;
        let was = answer(server, "/callback", |target| {
            asked = Some(target.to_string());
            forward
        });
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        if was {
            assert_eq!(asked.as_deref(), Some("/callback?code=c1&state=s1"));
        }
        (was, response)
    }

    #[test]
    fn the_callback_is_passed_on_and_the_browser_redirected() {
        let get = "GET /callback?code=c1&state=s1 HTTP/1.1\r\nHost: localhost:42221\r\n\r\n";
        let success = "https://platform.claude.com/oauth/code/success?app=claude-code";
        let (was, response) = ask(get, Some(("302".into(), success.into())));
        assert!(was);
        assert!(response.starts_with("HTTP/1.1 302 Found\r\n"), "{response}");
        assert!(
            response.contains(&format!("Location: {success}\r\n")),
            "{response}"
        );

        // A redirect anywhere else is not followed.
        let (_, response) = ask(get, Some(("302".into(), "https://example.org/".into())));
        assert!(response.starts_with("HTTP/1.1 200 OK") && response.contains("Signed in"));
        let (_, response) = ask(get, None);
        assert!(response.contains("did not take the sign-in"), "{response}");

        // Other paths are not passed on, and the relay keeps waiting.
        let (was, response) = ask("GET /other HTTP/1.1\r\n\r\n", None);
        assert!(!was);
        assert!(response.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn a_localhost_redirect_is_found_in_the_link() {
        let link = "https://claude.com/cai/oauth/authorize?code=true&client_id=x\
                    &redirect_uri=http%3A%2F%2Flocalhost%3A42221%2Fcallback&scope=a+b&state=s";
        assert_eq!(local_callback(link), Some((42221, "/callback".into())));
        let manual = "https://claude.com/cai/oauth/authorize?code=true\
                      &redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback";
        assert_eq!(local_callback(manual), None);
        assert_eq!(
            local_callback("https://claude.com/?redirect_uri=http%3A%2F%2Flocalhost%3A80%2F"),
            None
        );
        assert_eq!(
            local_callback("https://claude.com/?redirect_uri=http%3A%2F%2Flocalhost%3A5000"),
            Some((5000, "/".into()))
        );
        assert_eq!(local_callback("https://claude.com/login"), None);
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
