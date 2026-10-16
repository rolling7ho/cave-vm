use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_proto::agent::Message;
use cave_vm_proto::daemon::NetMode;
use cave_vm_proto::frame::decode;
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{ErrorCode, Hello, VERSIONS, read_frame, write_frame};

use super::*;

fn send(link: &mut UnixStream, message: &ToVmm) {
    write_frame(link, message).unwrap();
}

fn receive(link: &mut UnixStream) -> FromVmm {
    read_frame(link).unwrap().unwrap()
}

fn hello(link: &mut UnixStream) {
    send(link, &ToVmm::Hello(Hello::client(VERSIONS, "daemon test")));
    assert!(matches!(receive(link), FromVmm::Hello(_)));
}

fn config() -> VmmConfig {
    VmmConfig {
        kernel: "/k".into(),
        disk: "/d".into(),
        memory_bytes: 1 << 30,
        cpus: 2,
        net: NetMode::Nat,
        restore: None,
    }
}

#[test]
fn boot_comes_after_the_handshake_and_pings_are_answered() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let waiter = thread::spawn(move || wait_for_boot(&mut vmm).unwrap());
    hello(&mut daemon);
    send(&mut daemon, &ToVmm::Ping);
    assert!(matches!(receive(&mut daemon), FromVmm::Pong));
    send(&mut daemon, &ToVmm::AgentConnect);
    match receive(&mut daemon) {
        FromVmm::Error(e) => assert_eq!(e.code, ErrorCode::NotRunning),
        other => panic!("{other:?}"),
    }
    send(&mut daemon, &ToVmm::Boot { config: config() });
    assert_eq!(waiter.join().unwrap(), Some(config()));
}

#[test]
fn the_first_message_must_be_hello() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let waiter = thread::spawn(move || wait_for_boot(&mut vmm).unwrap());
    send(&mut daemon, &ToVmm::Ping);
    match receive(&mut daemon) {
        FromVmm::Error(e) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }
    assert_eq!(waiter.join().unwrap(), None);
}

#[test]
fn stop_or_a_closed_link_before_boot_means_no_boot() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let waiter = thread::spawn(move || wait_for_boot(&mut vmm).unwrap());
    hello(&mut daemon);
    send(&mut daemon, &ToVmm::Stop);
    assert!(matches!(receive(&mut daemon), FromVmm::Ok));
    assert_eq!(waiter.join().unwrap(), None);

    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let waiter = thread::spawn(move || wait_for_boot(&mut vmm).unwrap());
    hello(&mut daemon);
    drop(daemon);
    assert_eq!(waiter.join().unwrap(), None);
}

#[test]
fn an_unknown_message_gets_an_error_and_the_link_stays_up() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let waiter = thread::spawn(move || wait_for_boot(&mut vmm).unwrap());
    hello(&mut daemon);
    let body = br#"{"type":"teleport"}"#;
    daemon
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    daemon.write_all(body).unwrap();
    match receive(&mut daemon) {
        FromVmm::Error(e) => assert_eq!(e.code, ErrorCode::UnknownType),
        other => panic!("{other:?}"),
    }
    send(&mut daemon, &ToVmm::Ping);
    assert!(matches!(receive(&mut daemon), FromVmm::Pong));
    drop(daemon);
    assert_eq!(waiter.join().unwrap(), None);
}

/// A VM whose agent connections are socket pairs; the test keeps the far
/// ends to play the agent.
#[derive(Default)]
struct FakeVm {
    agents: Mutex<Vec<UnixStream>>,
    snapshots: Mutex<Vec<std::path::PathBuf>>,
    connects: crate::nat::Connects,
    fail: bool,
}

impl RunningVm for FakeVm {
    fn agent_connection(&self) -> io::Result<UnixStream> {
        if self.fail {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        let (ours, agent) = UnixStream::pair()?;
        self.agents.lock().unwrap().push(agent);
        Ok(ours)
    }

    fn snapshot(&self, dir: &std::path::Path) -> Result<(), ErrorMessage> {
        if self.fail {
            return Err(ErrorMessage::new(ErrorCode::Io, "disk full"));
        }
        self.snapshots.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }

    fn take_connects(&self) -> (Vec<AuditEntry>, u64) {
        self.connects.take()
    }
}

#[test]
fn connection_attempts_are_handed_over_once() {
    use crate::nat::{ConnectEvent, Protocol};
    use std::net::{Ipv4Addr, SocketAddrV4};

    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let vm = FakeVm::default();
    vm.connects.push(
        9,
        &ConnectEvent {
            protocol: Protocol::Tcp,
            dst: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 22),
            allowed: false,
            reason: Some("LAN"),
        },
    );
    thread::scope(|s| {
        let server = s.spawn(|| serve(&mut vmm, &vm).unwrap());
        send(&mut daemon, &ToVmm::TakeConnects);
        let FromVmm::Connects { entries, dropped } = receive(&mut daemon) else {
            panic!("no connects");
        };
        assert_eq!((entries.len(), dropped), (1, 0));
        assert_eq!(entries[0].time_unix_ms, 9);
        send(&mut daemon, &ToVmm::TakeConnects);
        assert_eq!(
            receive(&mut daemon),
            FromVmm::Connects {
                entries: Vec::new(),
                dropped: 0
            }
        );
        drop(daemon);
        assert_eq!(server.join().unwrap(), Ending::Closed);
    });
}

#[test]
fn an_agent_connection_arrives_as_a_descriptor() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let vm = FakeVm {
        fail: false,
        ..FakeVm::default()
    };
    thread::scope(|s| {
        let server = s.spawn(|| serve(&mut vmm, &vm).unwrap());
        send(&mut daemon, &ToVmm::AgentConnect);
        let mut buf = [0u8; 256];
        let (n, fd) = host::recv_with_fd(&daemon, &mut buf).unwrap();
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(n, 4 + len);
        assert!(matches!(
            decode::<FromVmm>(&buf[4..n]).unwrap(),
            FromVmm::AgentConnection
        ));
        let mut connection = UnixStream::from(fd.expect("a descriptor"));
        connection.write_all(b"ping").unwrap();
        let mut agent = vm.agents.lock().unwrap().pop().unwrap();
        let mut got = [0u8; 4];
        agent.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        send(&mut daemon, &ToVmm::Boot { config: config() });
        match receive(&mut daemon) {
            FromVmm::Error(e) => assert_eq!(e.code, ErrorCode::AlreadyExists),
            other => panic!("{other:?}"),
        }
        send(&mut daemon, &ToVmm::Stop);
        assert!(matches!(receive(&mut daemon), FromVmm::Ok));
        assert_eq!(server.join().unwrap(), Ending::Stop);
    });
}

#[test]
fn snapshots_are_saved_where_asked_and_failures_reported() {
    for fail in [false, true] {
        let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
        let vm = FakeVm {
            fail,
            ..FakeVm::default()
        };
        thread::scope(|s| {
            let server = s.spawn(|| serve(&mut vmm, &vm).unwrap());
            send(
                &mut daemon,
                &ToVmm::Snapshot {
                    dir: "/p/.cave/snapshots/s".into(),
                },
            );
            match receive(&mut daemon) {
                FromVmm::Ok => assert!(!fail),
                FromVmm::Error(e) => assert!(fail && e.message == "disk full", "{e:?}"),
                other => panic!("{other:?}"),
            }
            drop(daemon);
            assert_eq!(server.join().unwrap(), Ending::Closed);
        });
        let saved = vm.snapshots.lock().unwrap().clone();
        let expect: Vec<std::path::PathBuf> = if fail {
            vec![]
        } else {
            vec!["/p/.cave/snapshots/s".into()]
        };
        assert_eq!(saved, expect);
    }
}

#[test]
fn an_unreachable_agent_is_an_error_reply() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let vm = FakeVm {
        fail: true,
        ..FakeVm::default()
    };
    thread::scope(|s| {
        let server = s.spawn(|| serve(&mut vmm, &vm).unwrap());
        send(&mut daemon, &ToVmm::AgentConnect);
        match receive(&mut daemon) {
            FromVmm::Error(e) => assert_eq!(e.code, ErrorCode::Io),
            other => panic!("{other:?}"),
        }
        drop(daemon);
        assert_eq!(server.join().unwrap(), Ending::Closed);
    });
}

/// Plays an agent that answers the handshake, a control `ping` and the
/// clock.
fn agent() -> UnixStream {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    thread::spawn(move || {
        if let Ok(Some(Message::Hello(hello))) = read_frame::<_, Message>(&mut theirs) {
            let reply = hello.accept(VERSIONS, "agent test").unwrap();
            write_frame(&mut theirs, &Message::Hello(reply)).unwrap();
            let control = read_frame::<_, Message>(&mut theirs).unwrap();
            assert_eq!(control, Some(Message::Control));
            let ping = read_frame::<_, Message>(&mut theirs).unwrap();
            assert_eq!(ping, Some(Message::Ping));
            write_frame(&mut theirs, &Message::Pong).unwrap();
            match read_frame::<_, Message>(&mut theirs).unwrap() {
                Some(Message::Restored { time_unix_ns, .. }) => {
                    assert!(time_unix_ns > 1_700_000_000_000_000_000)
                }
                other => panic!("{other:?}"),
            }
            write_frame(&mut theirs, &Message::Ok).unwrap();
        }
    });
    ours
}

#[test]
fn waiting_for_the_agent_retries_until_it_answers() {
    let attempts = AtomicU32::new(0);
    let connect = || {
        if attempts.fetch_add(1, Ordering::SeqCst) < 3 {
            Err(io::ErrorKind::ConnectionRefused.into())
        } else {
            Ok(agent())
        }
    };
    wait_for_agent(&connect, &|| false, Duration::from_secs(5)).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
}

#[test]
fn a_connection_that_is_never_answered_is_tried_again_soon() {
    let attempts = AtomicU32::new(0);
    let silent = std::sync::Mutex::new(Vec::new());
    let connect = || {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            // As after a restore: the guest never sees this one.
            let (ours, theirs) = UnixStream::pair()?;
            silent.lock().unwrap().push(theirs);
            Ok(ours)
        } else {
            Ok(agent())
        }
    };
    let start = Instant::now();
    wait_for_agent(&connect, &|| false, Duration::from_secs(5)).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn waiting_for_the_agent_gives_up() {
    let start = Instant::now();
    let refuse = || Err(io::ErrorKind::ConnectionRefused.into());
    let e = wait_for_agent(&refuse, &|| false, Duration::from_millis(100)).unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal);
    assert!(start.elapsed() < Duration::from_secs(2));

    let e = wait_for_agent(&refuse, &|| true, Duration::from_secs(5)).unwrap_err();
    assert!(e.message.contains("stopped"), "{}", e.message);
}

/// Connections arrive open even when many are handed out at once. Closing
/// the VMM's end right after sending it lost connections on macOS under
/// load, so `serve` keeps it until the next request.
#[test]
fn many_agent_connections_arrive_open() {
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
                let vm = FakeVm {
                    fail: false,
                    ..FakeVm::default()
                };
                thread::scope(|s| {
                    s.spawn(|| serve(&mut vmm, &vm).unwrap());
                    for _ in 0..50 {
                        send(&mut daemon, &ToVmm::AgentConnect);
                        let mut buf = [0u8; 256];
                        let (_, fd) = host::recv_with_fd(&daemon, &mut buf).unwrap();
                        let mut connection = UnixStream::from(fd.expect("a descriptor"));
                        connection.write_all(b"x").unwrap();
                        let mut agent = vm.agents.lock().unwrap().pop().unwrap();
                        let mut got = [0u8; 1];
                        agent.read_exact(&mut got).unwrap();
                        agent.write_all(b"y").unwrap();
                        connection.read_exact(&mut got).unwrap();
                        assert_eq!(&got, b"y");
                    }
                    drop(daemon);
                });
            });
        }
    });
}

#[test]
fn snapshot_preparation_waits_for_the_agent() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let agent = thread::spawn(move || {
        let Some(Message::Hello(hello)) = read_frame::<_, Message>(&mut theirs).unwrap() else {
            panic!("no hello");
        };
        let reply = hello.accept(VERSIONS, "agent test").unwrap();
        write_frame(&mut theirs, &Message::Hello(reply)).unwrap();
        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(read_frame::<_, Message>(&mut theirs).unwrap().unwrap());
        }
        write_frame(&mut theirs, &Message::SnapshotReady).unwrap();
        seen
    });
    prepare_snapshot(ours).unwrap();
    assert_eq!(
        agent.join().unwrap(),
        [Message::Control, Message::SnapshotPrepare]
    );

    // An agent that answers something else is an error.
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    thread::spawn(move || {
        if let Ok(Some(Message::Hello(hello))) = read_frame::<_, Message>(&mut theirs) {
            let reply = hello.accept(VERSIONS, "agent test").unwrap();
            write_frame(&mut theirs, &Message::Hello(reply)).unwrap();
            let _ = read_frame::<_, Message>(&mut theirs);
            let _ = read_frame::<_, Message>(&mut theirs);
            write_frame(&mut theirs, &Message::Pong).unwrap();
        }
    });
    assert!(prepare_snapshot(ours).is_err());
}
