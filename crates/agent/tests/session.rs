//! The agent's operations over a Unix socket pair, as the host would drive
//! them over vsock. Runs on the host: exec, files and forwarding are plain
//! std code.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use cave_vm_agent::{Config, serve};
use cave_vm_proto::agent::{Exec, Message, Signal, Stream};
use cave_vm_proto::{Data, ErrorCode, Hello, VERSIONS, read_frame, write_frame};
use sha2::{Digest, Sha256};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("cave-agent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Starts an agent session and does the handshake.
fn connect(dir: &Dir) -> UnixStream {
    let (client, agent) = UnixStream::pair().unwrap();
    let config = Config {
        default_cwd: dir.0.clone(),
        software: "cave-agent test".into(),
    };
    thread::spawn(move || serve(Box::new(agent), &config));
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut client = client;
    send(
        &mut client,
        Message::Hello(Hello::client(VERSIONS, "cave test")),
    );
    match recv(&mut client) {
        Message::Hello(h) => assert_eq!(h.version, Some(1)),
        other => panic!("{other:?}"),
    }
    client
}

fn send(stream: &mut UnixStream, message: Message) {
    write_frame(stream, &message).unwrap();
}

fn recv(stream: &mut UnixStream) -> Message {
    read_frame(stream).unwrap().expect("a message")
}

/// Collected output of an exec.
#[derive(Default, Debug)]
struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    signal: Option<String>,
}

fn collect(stream: &mut UnixStream) -> Run {
    let mut run = Run::default();
    loop {
        match recv(stream) {
            Message::Output {
                stream: Stream::Stdout,
                data,
            } => run.stdout.extend(data.0),
            Message::Output {
                stream: Stream::Stderr,
                data,
            } => run.stderr.extend(data.0),
            Message::Exit { code, signal } => {
                run.code = code;
                run.signal = signal;
                return run;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn exec(argv: &[&str]) -> Exec {
    Exec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        ..Exec::default()
    }
}

fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn exec_streams_output_and_the_exit_code() {
    let dir = Dir::new("exec");
    let mut c = connect(&dir);
    let mut req = exec(&[
        "/bin/sh",
        "-c",
        "echo out; echo err >&2; echo $GREETING; pwd; exit 3",
    ]);
    req.env.insert("GREETING".into(), "hi".into());
    send(&mut c, Message::Exec(req));
    let run = collect(&mut c);
    let cwd = fs::canonicalize(&dir.0).unwrap();
    let stdout = String::from_utf8(run.stdout).unwrap();
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("out"));
    assert_eq!(lines.next(), Some("hi"));
    assert_eq!(
        fs::canonicalize(lines.next().unwrap()).unwrap(),
        cwd,
        "default cwd"
    );
    assert_eq!(run.stderr, b"err\n");
    assert_eq!((run.code, run.signal), (Some(3), None));
}

#[test]
fn exec_without_stdin_reads_dev_null() {
    let dir = Dir::new("nostdin");
    let mut c = connect(&dir);
    send(&mut c, Message::Exec(exec(&["/bin/cat"])));
    let run = collect(&mut c);
    assert_eq!((run.stdout.len(), run.code), (0, Some(0)));
}

#[test]
fn exec_with_stdin_gets_input_until_input_end() {
    let dir = Dir::new("stdin");
    let mut c = connect(&dir);
    let mut req = exec(&["/bin/cat"]);
    req.stdin = true;
    send(&mut c, Message::Exec(req));
    send(
        &mut c,
        Message::Input {
            data: Data(b"hello ".to_vec()),
        },
    );
    send(
        &mut c,
        Message::Input {
            data: Data(b"agent".to_vec()),
        },
    );
    send(&mut c, Message::InputEnd);
    let run = collect(&mut c);
    assert_eq!(
        (run.stdout.as_slice(), run.code),
        (&b"hello agent"[..], Some(0))
    );
}

#[test]
fn signal_ends_the_program_and_is_reported() {
    let dir = Dir::new("signal");
    let mut c = connect(&dir);
    send(&mut c, Message::Exec(exec(&["/bin/sleep", "30"])));
    thread::sleep(Duration::from_millis(100));
    send(
        &mut c,
        Message::Signal {
            signal: Signal::Kill,
        },
    );
    let run = collect(&mut c);
    assert_eq!((run.code, run.signal.as_deref()), (None, Some("KILL")));
}

#[test]
fn missing_and_unrunnable_programs_exit_like_a_shell() {
    let dir = Dir::new("missing");
    let mut c = connect(&dir);
    send(&mut c, Message::Exec(exec(&["no-such-program-cave"])));
    let run = collect(&mut c);
    assert_eq!(run.code, Some(127));
    assert!(
        String::from_utf8(run.stderr)
            .unwrap()
            .contains("command not found")
    );

    let script = dir.0.join("not-executable.sh");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    let mut c = connect(&dir);
    send(&mut c, Message::Exec(exec(&[script.to_str().unwrap()])));
    assert_eq!(collect(&mut c).code, Some(126));
}

#[test]
fn missing_working_directory_is_an_error() {
    let dir = Dir::new("nocwd");
    let mut c = connect(&dir);
    let mut req = exec(&["/bin/true"]);
    req.cwd = Some("/no/such/dir".into());
    send(&mut c, Message::Exec(req));
    match recv(&mut c) {
        Message::Error(e) => assert_eq!(e.code, ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
}

#[test]
fn closing_the_connection_kills_the_program() {
    let dir = Dir::new("hangup");
    let marker = dir.0.join("survived");
    let mut c = connect(&dir);
    let script = format!("sleep 1; touch {}", marker.display());
    send(&mut c, Message::Exec(exec(&["/bin/sh", "-c", &script])));
    thread::sleep(Duration::from_millis(100));
    drop(c);
    thread::sleep(Duration::from_millis(1500));
    assert!(!marker.exists(), "the program was killed");
}

#[test]
fn file_write_then_read_round_trips_with_mode() {
    let dir = Dir::new("files");
    let data: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();

    let mut c = connect(&dir);
    send(
        &mut c,
        Message::FileWrite {
            path: "src/deep/app.bin".into(),
            mode: Some(0o640),
            create_dirs: true,
        },
    );
    for chunk in data.chunks(256 * 1024) {
        send(
            &mut c,
            Message::FileData {
                data: Data(chunk.to_vec()),
            },
        );
    }
    send(
        &mut c,
        Message::FileEnd {
            sha256: sha256(&data),
        },
    );
    match recv(&mut c) {
        Message::FileWritten { size } => assert_eq!(size, data.len() as u64),
        other => panic!("{other:?}"),
    }
    let path = dir.0.join("src/deep/app.bin");
    assert_eq!(fs::read(&path).unwrap(), data);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );

    let mut c = connect(&dir);
    send(
        &mut c,
        Message::FileRead {
            path: "src/deep/app.bin".into(),
        },
    );
    let mut got = Vec::new();
    match recv(&mut c) {
        Message::FileInfo { size, mode } => {
            assert_eq!((size, mode), (data.len() as u64, 0o640));
        }
        other => panic!("{other:?}"),
    }
    loop {
        match recv(&mut c) {
            Message::FileData { data } => got.extend(data.0),
            Message::FileEnd { sha256: hash } => {
                assert_eq!(hash, sha256(&data));
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(got, data);
}

#[test]
fn damaged_write_leaves_the_original_untouched() {
    let dir = Dir::new("damaged");
    let path = dir.0.join("keep.txt");
    fs::write(&path, "original").unwrap();
    let mut c = connect(&dir);
    send(
        &mut c,
        Message::FileWrite {
            path: "keep.txt".into(),
            mode: None,
            create_dirs: false,
        },
    );
    send(
        &mut c,
        Message::FileData {
            data: Data(b"replacement".to_vec()),
        },
    );
    send(
        &mut c,
        Message::FileEnd {
            sha256: sha256(b"something else"),
        },
    );
    match recv(&mut c) {
        Message::Error(e) => {
            assert_eq!(e.code, ErrorCode::Io);
            assert!(e.message.contains("untouched"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    let leftovers = fs::read_dir(&dir.0).unwrap().count();
    assert_eq!(leftovers, 1, "no temporary file left");
}

#[test]
fn reading_a_missing_file_says_so() {
    let dir = Dir::new("readmissing");
    let mut c = connect(&dir);
    send(
        &mut c,
        Message::FileRead {
            path: "src/app.ts".into(),
        },
    );
    match recv(&mut c) {
        Message::Error(e) => {
            assert_eq!(e.code, ErrorCode::NotFound);
            assert!(e.message.contains("no such file in the VM"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn port_forward_carries_raw_bytes_both_ways() {
    let dir = Dir::new("forward");
    // A server inside "the guest", on loopback only.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        conn.write_all(b"pong").unwrap();
    });
    let mut c = connect(&dir);
    send(&mut c, Message::PortForward { port });
    assert_eq!(recv(&mut c), Message::PortForwardOk);
    c.write_all(b"ping").unwrap();
    let mut reply = Vec::new();
    c.read_to_end(&mut reply).unwrap();
    assert_eq!(reply, b"pong");
    server.join().unwrap();
    let _ = c.shutdown(Shutdown::Both);
}

#[test]
fn port_forward_to_a_closed_port_is_not_found() {
    let dir = Dir::new("closedport");
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut c = connect(&dir);
    send(&mut c, Message::PortForward { port });
    match recv(&mut c) {
        Message::Error(e) => assert_eq!(e.code, ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
}

#[test]
fn control_answers_ping_and_snapshot_prepare() {
    let dir = Dir::new("control");
    let mut c = connect(&dir);
    send(&mut c, Message::Control);
    send(&mut c, Message::Ping);
    assert_eq!(recv(&mut c), Message::Pong);
    send(&mut c, Message::SnapshotPrepare);
    assert_eq!(recv(&mut c), Message::SnapshotReady);
    send(&mut c, Message::Exec(exec(&["/bin/true"])));
    match recv(&mut c) {
        Message::Error(e) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }
}

#[test]
fn protocol_errors_are_reported() {
    let dir = Dir::new("proto");
    // Something other than hello first.
    let (mut client, agent) = UnixStream::pair().unwrap();
    let config = Config {
        default_cwd: dir.0.clone(),
        software: "t".into(),
    };
    thread::spawn(move || serve(Box::new(agent), &config));
    send(&mut client, Message::Ping);
    match recv(&mut client) {
        Message::Error(e) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }

    // No common version.
    let (mut client, agent) = UnixStream::pair().unwrap();
    let config = Config {
        default_cwd: dir.0.clone(),
        software: "t".into(),
    };
    thread::spawn(move || serve(Box::new(agent), &config));
    send(&mut client, Message::Hello(Hello::client(&[99], "cave 9")));
    match recv(&mut client) {
        Message::Error(e) => {
            assert_eq!(e.code, ErrorCode::UnsupportedVersion);
            assert_eq!(e.supported, Some(VERSIONS.to_vec()));
        }
        other => panic!("{other:?}"),
    }

    // A reply where a request belongs.
    let mut c = connect(&dir);
    send(&mut c, Message::Pong);
    match recv(&mut c) {
        Message::Error(e) => {
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert!(e.message.contains("Pong"), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
}
