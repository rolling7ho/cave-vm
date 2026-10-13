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
struct FakeVm {
    agents: Mutex<Vec<UnixStream>>,
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
}

#[test]
fn an_agent_connection_arrives_as_a_descriptor() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let vm = FakeVm {
        agents: Mutex::new(Vec::new()),
        fail: false,
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
fn an_unreachable_agent_is_an_error_reply() {
    let (mut daemon, mut vmm) = UnixStream::pair().unwrap();
    let vm = FakeVm {
        agents: Mutex::new(Vec::new()),
        fail: true,
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

/// Plays an agent that answers the handshake and a control `ping`.
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
fn waiting_for_the_agent_gives_up() {
    let start = Instant::now();
    let refuse = || Err(io::ErrorKind::ConnectionRefused.into());
    let e = wait_for_agent(&refuse, &|| false, Duration::from_millis(100)).unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal);
    assert!(start.elapsed() < Duration::from_secs(2));

    let e = wait_for_agent(&refuse, &|| true, Duration::from_secs(5)).unwrap_err();
    assert!(e.message.contains("stopped"), "{}", e.message);
}
