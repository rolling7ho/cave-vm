//! VMM supervision against a fake VMM process: this test binary, started
//! again with `CAVE_FAKE_VMM` set, speaks the VMM control protocol on its
//! standard input.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_daemon::home::Home;
use cave_vm_daemon::launch::{Launcher, ProcessLauncher};
use cave_vm_daemon::server;
use cave_vm_daemon::vms::{Prepared, VmSource, Vms};
use cave_vm_proto::daemon::{
    EntryKind, NetMode, Policy, PortMap, Reply, Request, Storage, VmId, VmState,
};
use cave_vm_proto::frame::encode;
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{ErrorCode, ErrorMessage, Hello, VERSIONS, read_frame, write_frame};
use cave_vm_vmm::host;

/// The fake VMM's behaviour, from `CAVE_FAKE_VMM`.
const NORMAL: &str = "normal";
const FAIL_BOOT: &str = "fail-boot";
const CRASH_AFTER_BOOT: &str = "crash-after-boot";
const IGNORE_HANGUP: &str = "ignore-hangup";

/// Not a test: the fake VMM's entry point when this binary is started as
/// one. As a test it returns at once.
#[test]
fn fake_vmm() {
    let Ok(mode) = std::env::var("CAVE_FAKE_VMM") else {
        return;
    };
    let stdin = io::stdin().as_fd().try_clone_to_owned().unwrap();
    let mut link = UnixStream::from(stdin);
    loop {
        let message = match read_frame::<_, ToVmm>(&mut link) {
            Ok(Some(message)) => message,
            _ if mode == IGNORE_HANGUP => loop {
                thread::sleep(Duration::from_secs(60));
            },
            _ => std::process::exit(0),
        };
        let reply = match message {
            ToVmm::Hello(hello) => FromVmm::Hello(hello.accept(VERSIONS, "fake-vmm").unwrap()),
            ToVmm::Boot { .. } if mode == FAIL_BOOT => FromVmm::Error(ErrorMessage::new(
                ErrorCode::Internal,
                "the kernel did not start",
            )),
            ToVmm::Boot { .. } => FromVmm::Booted,
            ToVmm::Ping => FromVmm::Pong,
            ToVmm::AgentConnect => {
                // The far end echoes, standing in for the guest agent.
                let (ours, mut agent) = UnixStream::pair().unwrap();
                thread::spawn(move || {
                    let mut copy = agent.try_clone().unwrap();
                    let _ = io::copy(&mut agent, &mut copy);
                });
                let frame = encode(&FromVmm::AgentConnection).unwrap();
                host::send_with_fd(&link, &frame, ours.as_fd()).unwrap();
                continue;
            }
            ToVmm::Stop => {
                write_frame(&mut link, &FromVmm::Ok).unwrap();
                std::process::exit(0);
            }
            _ => FromVmm::Error(ErrorMessage::new(ErrorCode::BadRequest, "not in the fake")),
        };
        write_frame(&mut link, &reply).unwrap();
        if matches!(reply, FromVmm::Booted) && mode == CRASH_AFTER_BOOT {
            thread::sleep(Duration::from_millis(100));
            std::process::exit(3);
        }
    }
}

fn fake_launcher(mode: &str) -> ProcessLauncher {
    let mut launcher = ProcessLauncher::new(std::env::current_exe().unwrap());
    launcher.args = ["fake_vmm", "--exact", "--nocapture", "--test-threads=1"]
        .map(Into::into)
        .to_vec();
    launcher.envs = vec![("CAVE_FAKE_VMM".into(), mode.into())];
    launcher
}

/// Counts preparations and makes up a config and policy.
#[derive(Default)]
struct Source {
    prepared: AtomicUsize,
}

impl VmSource for Source {
    fn prepare(
        &self,
        vm: &VmId,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Prepared, ErrorMessage> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        let net = net.unwrap_or(NetMode::Nat);
        Ok(Prepared {
            config: VmmConfig {
                kernel: "/fake/Image".into(),
                disk: format!("{}/.cave/disk.img", vm.project),
                memory_bytes: 2 << 30,
                cpus: 2,
                net,
            },
            policy: Policy {
                name: "app".into(),
                image: "base".into(),
                storage: Storage::Sealed,
                net,
                ports: ports.to_vec(),
                cpus: 2,
                memory_bytes: 2 << 30,
                ttl_seconds: None,
                widened: Vec::new(),
            },
        })
    }
}

struct Rig {
    home: Home,
    vms: Arc<Vms>,
    source: Arc<Source>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.vms.stop_all();
        let _ = fs::remove_dir_all(self.home.root());
    }
}

fn home(name: &str) -> Home {
    // Short, since the daemon socket's path must fit in 104 bytes.
    let root = PathBuf::from("/tmp").join(format!("cvd-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    Home::at(root).unwrap()
}

fn rig(name: &str, mode: &str) -> Rig {
    let home = home(name);
    let source = Arc::new(Source::default());
    let vms = Arc::new(Vms::new(
        home.clone(),
        Arc::new(fake_launcher(mode)),
        source.clone(),
    ));
    let listener = server::bind(&home).unwrap();
    let serving = Arc::clone(&vms);
    thread::spawn(move || server::serve(&listener, &serving));
    Rig { home, vms, source }
}

fn vm() -> VmId {
    VmId {
        project: "/Users/someone/projects/app".into(),
        fork: None,
    }
}

impl Rig {
    fn request(&self, request: Request) -> Reply {
        let mut c = UnixStream::connect(self.home.socket()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write_frame(&mut c, &Request::Hello(Hello::client(VERSIONS, "test"))).unwrap();
        assert!(matches!(
            read_frame::<_, Reply>(&mut c).unwrap(),
            Some(Reply::Hello(_))
        ));
        write_frame(&mut c, &request).unwrap();
        read_frame(&mut c).unwrap().expect("a reply")
    }

    fn start(&self) -> Reply {
        self.request(Request::Start {
            vm: vm(),
            net: None,
            ports: Vec::new(),
        })
    }

    fn state(&self) -> Option<VmState> {
        match self.request(Request::List { project: None }) {
            Reply::List { entries } => entries.first().and_then(|e| e.state),
            other => panic!("{other:?}"),
        }
    }

    fn pid_file(&self) -> PathBuf {
        self.home.vm_dir(&vm()).unwrap().join("vmm.pid")
    }

    fn wait_for_state(&self, want: VmState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.state() != Some(want) {
            assert!(Instant::now() < deadline, "never reached {want:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn the_socket_and_data_directory_are_user_only() {
    let r = rig("perm", NORMAL);
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(r.home.root()), 0o700);
    assert_eq!(mode(&r.home.socket()), 0o600);
    assert_eq!(mode(&r.home.vm_dir(&vm()).unwrap()), 0o700);
}

#[test]
fn a_second_daemon_on_the_same_data_directory_is_refused() {
    let r = rig("twice", NORMAL);
    let err = server::bind(&r.home).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    assert!(err.to_string().contains("already running"));
}

#[test]
fn start_list_and_stop() {
    let r = rig("life", NORMAL);
    assert!(
        matches!(r.request(Request::List { project: None }), Reply::List { entries } if entries.is_empty())
    );
    match r.start() {
        Reply::Policy(policy) => assert_eq!(policy.net, NetMode::Nat),
        other => panic!("{other:?}"),
    }
    match r.request(Request::List {
        project: Some(vm().project),
    }) {
        Reply::List { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "app");
            assert_eq!(entries[0].kind, EntryKind::Vm);
            assert_eq!(entries[0].state, Some(VmState::Running));
        }
        other => panic!("{other:?}"),
    }
    let pid: u32 = fs::read_to_string(r.pid_file()).unwrap().parse().unwrap();
    assert!(pid > 0);

    // Starting a running VM returns its policy without a second VMM.
    assert!(matches!(r.start(), Reply::Policy(_)));
    assert_eq!(r.source.prepared.load(Ordering::SeqCst), 1);

    assert_eq!(r.request(Request::Stop { vm: vm() }), Reply::Ok);
    assert_eq!(r.state(), Some(VmState::Stopped));
    assert!(!r.pid_file().exists());
    // Stopping again is fine.
    assert_eq!(r.request(Request::Stop { vm: vm() }), Reply::Ok);
}

#[test]
fn a_failed_boot_is_reported_and_leaves_the_vm_stopped() {
    let r = rig("fail", FAIL_BOOT);
    match r.start() {
        Reply::Error(e) => {
            assert!(
                e.message.contains("the kernel did not start"),
                "{}",
                e.message
            );
            assert!(e.fix.unwrap().contains("vmm.log"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.state(), Some(VmState::Stopped));
    assert!(!r.pid_file().exists());
}

#[test]
fn a_crashed_vmm_leaves_the_vm_stopped_and_start_restarts_it() {
    let r = rig("crash", CRASH_AFTER_BOOT);
    assert!(matches!(r.start(), Reply::Policy(_)));
    r.wait_for_state(VmState::Stopped);
    assert!(!r.pid_file().exists());
    assert!(matches!(r.start(), Reply::Policy(_)));
    assert_eq!(r.source.prepared.load(Ordering::SeqCst), 2);
}

#[test]
fn startup_cleanup_kills_a_vmm_left_by_an_earlier_daemon() {
    let home = home("cleanup");
    let launcher = fake_launcher(IGNORE_HANGUP);
    let dir = home.vm_dir(&vm()).unwrap();
    // An earlier daemon started this VMM and then died: its control link is
    // gone, but this VMM ignores that.
    let mut launched = launcher.launch(&dir).unwrap();
    fs::write(dir.join("vmm.pid"), launched.child.id().to_string()).unwrap();
    drop(launched.control);
    thread::sleep(Duration::from_millis(200));
    assert!(
        launched.child.try_wait().unwrap().is_none(),
        "still running"
    );

    let vms = Vms::new(
        home.clone(),
        Arc::new(launcher),
        Arc::new(Source::default()),
    );
    assert_eq!(vms.clean_up().unwrap(), 1);
    assert!(!dir.join("vmm.pid").exists());
    let deadline = Instant::now() + Duration::from_secs(5);
    while launched.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the old VMM is still running");
        thread::sleep(Duration::from_millis(20));
    }

    // A PID file for a process that is gone, or someone else's, is just
    // removed.
    fs::write(dir.join("vmm.pid"), "1").unwrap();
    assert_eq!(vms.clean_up().unwrap(), 0);
    assert!(!dir.join("vmm.pid").exists());
    let _ = fs::remove_dir_all(home.root());
}

#[test]
fn unsupported_and_malformed_requests_are_errors() {
    let r = rig("unsup", NORMAL);
    match r.request(Request::Images) {
        Reply::Error(e) => {
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert!(e.message.contains("images"), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
    match r.request(Request::Stop {
        vm: VmId {
            project: "relative/path".into(),
            fork: None,
        },
    }) {
        Reply::Error(e) => assert!(e.message.contains("absolute"), "{}", e.message),
        other => panic!("{other:?}"),
    }
}

#[test]
fn agent_connections_come_from_the_running_vmm() {
    let r = rig("agent", NORMAL);
    let err = r.vms.agent_connection(&vm()).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotRunning);
    assert!(err.fix.unwrap().contains("cave run"));

    assert!(matches!(r.start(), Reply::Policy(_)));
    for _ in 0..2 {
        let mut agent = r.vms.agent_connection(&vm()).unwrap();
        agent.write_all(b"hello").unwrap();
        let mut back = [0u8; 5];
        agent.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello");
    }
    // The link still answers after handing out connections.
    assert_eq!(r.state(), Some(VmState::Running));
}
