//! Messages on the guest agent link (vsock port [`AGENT_PORT`](crate::AGENT_PORT)).
//!
//! One operation per connection: after the handshake the client sends one
//! request (`exec`, `pty_open`, `file_read`, `file_write`, `port_forward` or
//! `control`), and the connection carries that operation until it ends.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::Data;
use crate::error::ErrorMessage;
use crate::frame;
use crate::handshake::Hello;

/// Working directory when a request leaves `cwd` out.
pub const DEFAULT_CWD: &str = "/workspace";

/// Exit code when the program is not found, as in a shell.
pub const EXIT_NOT_FOUND: i32 = 127;

/// Exit code when the program cannot be run, as in a shell.
pub const EXIT_CANNOT_RUN: i32 = 126;

/// A timezone for the guest: its name, such as `Asia/Manila`, and the
/// Mac's compiled zone file for it, so the guest needs no zone database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timezone {
    pub name: String,
    pub tzif: Data,
}

/// Every message on the agent link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Error(ErrorMessage),

    /// Runs `argv` directly, without a shell.
    Exec(Exec),
    Output {
        stream: Stream,
        data: Data,
    },
    Input {
        data: Data,
    },
    InputEnd,
    Signal {
        signal: Signal,
    },
    /// The last message of `exec` and `pty_open`. Has exactly one of `code`
    /// and `signal`.
    Exit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        /// The name of the signal that ended the program, such as `KILL`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },

    PtyOpen(PtyOpen),
    PtyData {
        data: Data,
    },
    PtyResize {
        cols: u16,
        rows: u16,
    },

    FileRead {
        path: String,
    },
    FileInfo {
        size: u64,
        /// Unix permission bits.
        mode: u32,
    },
    FileData {
        data: Data,
    },
    FileEnd {
        /// Hex SHA-256 of all the `file_data` bytes.
        sha256: String,
    },
    FileWrite {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        create_dirs: bool,
    },
    FileWritten {
        size: u64,
    },

    /// Connects to `127.0.0.1:<port>` in the guest. After `port_forward_ok`
    /// the connection carries raw TCP bytes, without framing.
    PortForward {
        port: u16,
    },
    PortForwardOk,

    /// Opens the control connection.
    Control,
    Ping,
    Pong,
    SnapshotPrepare,
    SnapshotReady,
    /// Sent after a boot and a restore, and again whenever the Mac's clock
    /// or timezone moves, so the agent sets the guest's to match.
    Restored {
        time_unix_ns: u64,
        /// The Mac's timezone; left out when it cannot be read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<Timezone>,
    },
    Ok,

    /// A `type` this build does not know. [`frame::decode`] reports it as
    /// [`FrameError::UnknownType`](crate::FrameError::UnknownType) and never
    /// returns it.
    #[serde(other, skip_serializing)]
    Unknown,
}

/// The `exec` request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Exec {
    /// Program and arguments. At least one element.
    pub argv: Vec<String>,
    /// Working directory; [`DEFAULT_CWD`] when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Whether the client will send `input`. If not, stdin is `/dev/null`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stdin: bool,
}

/// The `pty_open` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyOpen {
    /// The guest user's login shell when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Signals a client may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Signal {
    Int,
    Term,
    Kill,
    Hup,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl frame::Message for Message {
    fn is_unknown(&self) -> bool {
        matches!(self, Message::Unknown)
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Message::Exec(exec) if exec.argv.is_empty() => Err("\"argv\" is empty".into()),
            Message::PtyOpen(PtyOpen {
                argv: Some(argv), ..
            }) if argv.is_empty() => Err("\"argv\" is empty".into()),
            Message::Exit { code, signal } if code.is_some() == signal.is_some() => {
                Err("needs exactly one of \"code\" and \"signal\"".into())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::frame::{FrameError, decode, encode, read_frame, write_frame};
    use serde_json::{Value, json};

    fn parse(value: Value) -> Result<Message, FrameError> {
        decode(&serde_json::to_vec(&value).unwrap())
    }

    /// Each example from `docs/protocol.md` decodes to `message` and
    /// encodes back to the same JSON.
    fn check(value: Value, message: Message) {
        assert_eq!(parse(value.clone()).unwrap(), message, "{value}");
        assert_eq!(serde_json::to_value(&message).unwrap(), value);
    }

    #[test]
    fn handshake_and_error_examples() {
        check(
            json!({"type": "hello", "versions": [1], "software": "cave 0.1.0"}),
            Message::Hello(Hello::client(&[1], "cave 0.1.0")),
        );
        check(
            json!({"type": "hello", "version": 1, "software": "cave-agent 0.1.0"}),
            Message::Hello(Hello {
                versions: None,
                version: Some(1),
                software: "cave-agent 0.1.0".into(),
            }),
        );
        check(
            json!({"type": "error", "code": "not_found", "message": "no such file in the VM: src/app.ts", "fix": "Check the path with cave exec -- ls src"}),
            Message::Error(
                ErrorMessage::new(ErrorCode::NotFound, "no such file in the VM: src/app.ts")
                    .with_fix("Check the path with cave exec -- ls src"),
            ),
        );
        check(
            json!({"type": "error", "code": "unsupported_version", "message": "...", "supported": [2, 3]}),
            Message::Error(ErrorMessage {
                supported: Some(vec![2, 3]),
                ..ErrorMessage::new(ErrorCode::UnsupportedVersion, "...")
            }),
        );
    }

    #[test]
    fn exec_examples() {
        check(
            json!({"type": "exec", "argv": ["npm", "test"], "cwd": "/workspace", "env": {"CI": "1"}, "stdin": true}),
            Message::Exec(Exec {
                argv: vec!["npm".into(), "test".into()],
                cwd: Some("/workspace".into()),
                env: [("CI".to_string(), "1".to_string())].into(),
                stdin: true,
            }),
        );
        check(
            json!({"type": "output", "stream": "stdout", "data": "aGVsbG8K"}),
            Message::Output {
                stream: Stream::Stdout,
                data: Data(b"hello\n".to_vec()),
            },
        );
        check(
            json!({"type": "input", "data": "aGk="}),
            Message::Input {
                data: Data(b"hi".to_vec()),
            },
        );
        check(json!({"type": "input_end"}), Message::InputEnd);
        check(
            json!({"type": "signal", "signal": "INT"}),
            Message::Signal {
                signal: Signal::Int,
            },
        );
        check(
            json!({"type": "exit", "code": 0}),
            Message::Exit {
                code: Some(0),
                signal: None,
            },
        );
        check(
            json!({"type": "exit", "signal": "KILL"}),
            Message::Exit {
                code: None,
                signal: Some("KILL".into()),
            },
        );
    }

    #[test]
    fn pty_file_port_and_control_examples() {
        check(
            json!({"type": "pty_open", "argv": ["/bin/bash", "-l"], "cwd": "/workspace", "term": "xterm-256color", "cols": 120, "rows": 40}),
            Message::PtyOpen(PtyOpen {
                argv: Some(vec!["/bin/bash".into(), "-l".into()]),
                cwd: Some("/workspace".into()),
                env: BTreeMap::new(),
                term: Some("xterm-256color".into()),
                cols: 120,
                rows: 40,
            }),
        );
        check(
            json!({"type": "pty_resize", "cols": 100, "rows": 30}),
            Message::PtyResize {
                cols: 100,
                rows: 30,
            },
        );
        check(
            json!({"type": "file_read", "path": "/workspace/src/app.ts"}),
            Message::FileRead {
                path: "/workspace/src/app.ts".into(),
            },
        );
        check(
            json!({"type": "file_info", "size": 1234, "mode": 420}),
            Message::FileInfo {
                size: 1234,
                mode: 420,
            },
        );
        check(
            json!({"type": "file_end", "sha256": "9f86d081"}),
            Message::FileEnd {
                sha256: "9f86d081".into(),
            },
        );
        check(
            json!({"type": "file_write", "path": "/workspace/src/app.ts", "mode": 420, "create_dirs": true}),
            Message::FileWrite {
                path: "/workspace/src/app.ts".into(),
                mode: Some(420),
                create_dirs: true,
            },
        );
        check(
            json!({"type": "file_written", "size": 1234}),
            Message::FileWritten { size: 1234 },
        );
        check(
            json!({"type": "port_forward", "port": 3000}),
            Message::PortForward { port: 3000 },
        );
        check(json!({"type": "port_forward_ok"}), Message::PortForwardOk);
        for (kind, message) in [
            ("control", Message::Control),
            ("ping", Message::Ping),
            ("pong", Message::Pong),
            ("snapshot_prepare", Message::SnapshotPrepare),
            ("snapshot_ready", Message::SnapshotReady),
            ("ok", Message::Ok),
        ] {
            check(json!({ "type": kind }), message);
        }
        check(
            json!({"type": "restored", "time_unix_ns": 1791800569000000000u64}),
            Message::Restored {
                time_unix_ns: 1_791_800_569_000_000_000,
                timezone: None,
            },
        );
        check(
            json!({"type": "restored", "time_unix_ns": 1, "timezone": {"name": "Asia/Manila", "tzif": "VFppZg=="}}),
            Message::Restored {
                time_unix_ns: 1,
                timezone: Some(Timezone {
                    name: "Asia/Manila".into(),
                    tzif: Data(b"TZif".to_vec()),
                }),
            },
        );
    }

    #[test]
    fn optional_fields_are_left_out_and_unknown_ones_ignored() {
        let exec = Message::Exec(Exec {
            argv: vec!["ls".into()],
            ..Exec::default()
        });
        assert_eq!(
            serde_json::to_value(&exec).unwrap(),
            json!({"type": "exec", "argv": ["ls"]})
        );
        assert_eq!(
            parse(json!({"type": "exec", "argv": ["ls"], "added_in_1_1": {"x": 1}})).unwrap(),
            exec
        );
    }

    #[test]
    fn errors_map_to_the_right_codes() {
        let code = |value: Value| parse(value).unwrap_err().code();
        assert_eq!(code(json!({"type": "teleport"})), ErrorCode::UnknownType);
        assert_eq!(code(json!({"argv": ["ls"]})), ErrorCode::BadRequest);
        assert_eq!(code(json!({"type": 7})), ErrorCode::BadRequest);
        assert_eq!(code(json!({"type": "exec"})), ErrorCode::BadRequest);
        assert_eq!(
            code(json!({"type": "exec", "argv": []})),
            ErrorCode::BadRequest
        );
        assert_eq!(
            code(json!({"type": "pty_open", "argv": [], "cols": 1, "rows": 1})),
            ErrorCode::BadRequest
        );
        assert_eq!(code(json!({"type": "exit"})), ErrorCode::BadRequest);
        assert_eq!(
            code(json!({"type": "exit", "code": 1, "signal": "KILL"})),
            ErrorCode::BadRequest
        );
        assert_eq!(
            code(json!({"type": "signal", "signal": "STOP"})),
            ErrorCode::BadRequest
        );
        assert_eq!(
            code(json!({"type": "input", "data": "not base64!"})),
            ErrorCode::BadRequest
        );
        assert_eq!(code(json!([1, 2])), ErrorCode::BadFrame);
        assert!(matches!(
            decode::<Message>(b"{\"type\": "),
            Err(FrameError::NotJson(_))
        ));
        let unknown = parse(json!({"type": "teleport"})).unwrap_err();
        assert_eq!(unknown.to_error_message().code, ErrorCode::UnknownType);
        assert!(unknown.to_string().contains("teleport"));
    }

    #[test]
    fn unknown_error_codes_from_newer_peers_still_decode() {
        let message = parse(json!({"type": "error", "code": "quota_exceeded", "message": "m"}));
        assert!(matches!(
            message,
            Ok(Message::Error(ErrorMessage {
                code: ErrorCode::Unknown,
                ..
            }))
        ));
    }

    #[test]
    fn frames_round_trip_over_a_stream() {
        let messages = [
            Message::Hello(Hello::client(crate::VERSIONS, "cave test")),
            Message::Output {
                stream: Stream::Stderr,
                data: Data(vec![0, 159, 255]),
            },
            Message::Exit {
                code: Some(1),
                signal: None,
            },
        ];
        let mut wire = Vec::new();
        for m in &messages {
            write_frame(&mut wire, m).unwrap();
        }
        let mut reader = wire.as_slice();
        for m in &messages {
            assert_eq!(
                read_frame::<_, Message>(&mut reader).unwrap().as_ref(),
                Some(m)
            );
        }
        assert!(
            read_frame::<_, Message>(&mut reader).unwrap().is_none(),
            "clean end"
        );
    }

    #[test]
    fn length_is_big_endian_and_counts_only_the_json() {
        let frame = encode(&Message::Ping).unwrap();
        let json = br#"{"type":"ping"}"#;
        assert_eq!(&frame[..4], &(json.len() as u32).to_be_bytes());
        assert_eq!(&frame[4..], json);
    }

    #[test]
    fn truncated_frames_are_errors() {
        let frame = encode(&Message::Ping).unwrap();
        for cut in [2, 4, frame.len() - 1] {
            let mut reader = &frame[..cut];
            assert!(
                matches!(
                    read_frame::<_, Message>(&mut reader),
                    Err(FrameError::Truncated)
                ),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn empty_and_oversized_frames_are_refused_without_reading_the_body() {
        let mut reader: &[u8] = &[0, 0, 0, 0];
        let err = read_frame::<_, Message>(&mut reader).unwrap_err();
        assert!(matches!(err, FrameError::Empty));
        assert_eq!(err.code(), ErrorCode::BadFrame);

        let over = (crate::MAX_FRAME as u32 + 1).to_be_bytes();
        let mut wire = over.to_vec();
        wire.extend_from_slice(b"body that must stay unread");
        let mut reader = wire.as_slice();
        let err = read_frame::<_, Message>(&mut reader).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(n) if n == crate::MAX_FRAME + 1));
        assert_eq!(err.code(), ErrorCode::FrameTooLarge);
        assert_eq!(reader, b"body that must stay unread");
    }

    #[test]
    fn a_frame_at_the_limit_is_read_and_one_over_is_not_written() {
        // The largest chunk a sender may put in one message fits.
        let chunk = Message::FileData {
            data: Data(vec![0xab; crate::MAX_CHUNK]),
        };
        let frame = encode(&chunk).unwrap();
        assert!(frame.len() - 4 < crate::MAX_FRAME);
        let mut reader = frame.as_slice();
        assert_eq!(read_frame::<_, Message>(&mut reader).unwrap(), Some(chunk));

        let huge = Message::FileData {
            data: Data(vec![0; crate::MAX_FRAME]),
        };
        assert!(matches!(encode(&huge), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn unknown_is_never_written() {
        assert!(encode(&Message::Unknown).is_err());
    }
}
