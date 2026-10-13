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
/// Boots only if it can use its disk's folder and the kernel but cannot
/// read `CAVE_FAKE_SECRET` or write next to it.
const SANDBOX_CHECK: &str = "sandbox-check";

/// Not a test: the fake VMM's entry point when this binary is started as
/// one. As a test it returns at once.
#[test]
fn fake_vmm() {
    let Ok(mode) = std::env::var("CAVE_FAKE_VMM") else {
        return;
    };
    let stdin = io::stdin().as_fd().try_clone_to_owned().unwrap();
    let mut link = UnixStream::from(stdin);
    // The last connection sent stays open here until the next request, as
    // in cave-vmm: closing it while in flight can lose it on macOS.
    let mut in_flight = None;
    loop {
        let message = match read_frame::<_, ToVmm>(&mut link) {
            Ok(Some(message)) => message,
            _ if mode == IGNORE_HANGUP => loop {
                thread::sleep(Duration::from_secs(60));
            },
            _ => std::process::exit(0),
        };
        drop(in_flight.take());
        let reply = match message {
            ToVmm::Hello(hello) => FromVmm::Hello(hello.accept(VERSIONS, "fake-vmm").unwrap()),
            ToVmm::Boot { .. } if mode == FAIL_BOOT => FromVmm::Error(ErrorMessage::new(
                ErrorCode::Internal,
                "the kernel did not start",
            )),
            ToVmm::Boot { config } if mode == SANDBOX_CHECK => sandbox_check(&config),
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
                in_flight = Some(ours);
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

/// What the sandboxed fake VMM can and cannot reach.
fn sandbox_check(config: &VmmConfig) -> FromVmm {
    let disk = std::path::Path::new(&config.disk);
    let secret = PathBuf::from(std::env::var_os("CAVE_FAKE_SECRET").unwrap());
    // What went wrong when each check fails.
    let checks = [
        ("could not write its disk", fs::write(disk, b"disk").is_ok()),
        ("could not read its disk", fs::read(disk).is_ok()),
        (
            "could not read the kernel",
            fs::read(&config.kernel).is_ok(),
        ),
        ("read a file outside", fs::read(&secret).is_err()),
        (
            "wrote a file outside",
            fs::write(secret.with_extension("new"), b"x").is_err(),
        ),
        (
            "listed a folder outside",
            fs::read_dir(secret.parent().unwrap()).is_err(),
        ),
    ];
    match checks.iter().find(|(_, ok)| !ok) {
        None => FromVmm::Booted,
        Some((what, _)) => FromVmm::Error(ErrorMessage::new(
            ErrorCode::Internal,
            format!("the sandboxed VMM {what}"),
        )),
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

fn config(vm: &VmId) -> VmmConfig {
    VmmConfig {
        kernel: "/fake/Image".into(),
        disk: format!("{}/.cave/disk.img", vm.project),
        memory_bytes: 2 << 30,
        cpus: 2,
        net: NetMode::Nat,
    }
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
            config: VmmConfig { net, ..config(vm) },
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
    let mut launched = launcher.launch(&dir, &config(&vm())).unwrap();
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
    match r.request(Request::Doctor) {
        Reply::Error(e) => {
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert!(e.message.contains("doctor"), "{}", e.message);
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

/// The VMM sandbox (P3.13): a VMM started under the profile reaches its
/// project's `.cave/`, the image cache and the kernel, and nothing else.
#[test]
fn the_sandboxed_vmm_reaches_only_its_project_state_and_the_image_cache() {
    let home = home("sandbox");
    let root = home.root().canonicalize().unwrap();
    let state = root.join("project/.cave");
    let images = root.join("images");
    let outside = root.join("someone");
    for dir in [&state, &images, &outside] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(images.join("Image"), b"kernel").unwrap();
    let secret = outside.join("secret.txt");
    fs::write(&secret, b"do not read").unwrap();

    let mut launcher = fake_launcher(SANDBOX_CHECK);
    launcher
        .envs
        .push(("CAVE_FAKE_SECRET".into(), secret.clone().into()));
    launcher.sandbox = Some(images.clone());
    let config = VmmConfig {
        kernel: images.join("Image").to_string_lossy().into_owned(),
        disk: state.join("disk.img").to_string_lossy().into_owned(),
        ..config(&vm())
    };
    let dir = home.vm_dir(&vm()).unwrap();
    let mut launched = launcher.launch(&dir, &config).unwrap();
    let mut link = launched.control;
    write_frame(&mut link, &ToVmm::Hello(Hello::client(VERSIONS, "test"))).unwrap();
    assert!(matches!(
        read_frame(&mut link).unwrap(),
        Some(FromVmm::Hello(_))
    ));
    write_frame(&mut link, &ToVmm::Boot { config }).unwrap();
    let reply: Option<FromVmm> = read_frame(&mut link).unwrap();
    assert_eq!(reply, Some(FromVmm::Booted));
    assert!(!secret.with_extension("new").exists());
    write_frame(&mut link, &ToVmm::Stop).unwrap();
    let _ = launched.child.wait();
    let _ = fs::remove_dir_all(home.root());
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

#[test]
fn ports_listen_on_localhost_while_the_vm_runs() {
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    let free = || {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let r = rig("ports", NORMAL);
    let start = |ports: Vec<PortMap>| {
        r.request(Request::Start {
            vm: vm(),
            net: None,
            ports,
        })
    };

    // A Mac port in use is refused before the VM boots.
    let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let busy = taken.local_addr().unwrap().port();
    match start(vec![PortMap { mac: busy, vm: 80 }]) {
        Reply::Error(e) => assert_eq!(e.code, ErrorCode::AlreadyExists),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.source.prepared.load(Ordering::SeqCst), 0);

    let (a, b) = (free(), free());
    assert!(matches!(
        start(vec![PortMap { mac: a, vm: 80 }]),
        Reply::Policy(_)
    ));
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, a)).is_ok());

    // `-p` on a running VM adds a forward without a restart.
    match start(vec![PortMap { mac: b, vm: 81 }]) {
        Reply::Policy(policy) => {
            assert_eq!(policy.ports.len(), 2, "{policy:?}");
            assert!(policy.widened.contains(&format!("-p {b}:81")));
        }
        other => panic!("{other:?}"),
    }
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, b)).is_ok());
    assert_eq!(r.source.prepared.load(Ordering::SeqCst), 1);
    match start(vec![PortMap { mac: b, vm: 82 }]) {
        Reply::Error(e) => assert!(e.message.contains("already forwards"), "{e:?}"),
        other => panic!("{other:?}"),
    }

    // Stopping the VM closes the Mac ports.
    assert_eq!(r.request(Request::Stop { vm: vm() }), Reply::Ok);
    thread::sleep(Duration::from_millis(300));
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, a)).is_err());
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, b)).is_err());
}

#[test]
fn remove_stops_the_vm_and_deletes_only_its_state() {
    let r = rig("rm", NORMAL);
    let project = r.home.root().join("proj");
    fs::create_dir_all(project.join(".cave")).unwrap();
    fs::write(project.join("cave.toml"), "").unwrap();
    fs::write(project.join(".cave/disk.img"), "disk").unwrap();
    let id = VmId {
        project: project.to_string_lossy().into_owned(),
        fork: None,
    };
    let started = r.request(Request::Start {
        vm: id.clone(),
        net: None,
        ports: Vec::new(),
    });
    assert!(matches!(started, Reply::Policy(_)), "{started:?}");
    // The daemon remembers the project in its own folder, not in `.cave/`.
    assert_eq!(r.home.projects().unwrap(), std::slice::from_ref(&project));

    let remove = |name: Option<&str>| {
        r.request(Request::Remove {
            vm: id.clone(),
            name: name.map(String::from),
        })
    };
    match remove(Some("nope")) {
        Reply::Error(e) => assert_eq!(e.code, ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
    assert_eq!(remove(None), Reply::Ok);
    assert!(!project.join(".cave").exists());
    assert!(project.join("cave.toml").exists());
    assert!(r.home.projects().unwrap().is_empty());
    assert!(
        matches!(r.request(Request::List { project: None }), Reply::List { entries } if entries.is_empty())
    );
    // Again, with nothing left, is fine.
    assert_eq!(remove(None), Reply::Ok);

    // A `.cave` that is a link is removed without following it.
    let elsewhere = r.home.root().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep"), "keep").unwrap();
    std::os::unix::fs::symlink(&elsewhere, project.join(".cave")).unwrap();
    assert_eq!(remove(None), Reply::Ok);
    assert!(fs::symlink_metadata(project.join(".cave")).is_err());
    assert!(elsewhere.join("keep").exists());
}
