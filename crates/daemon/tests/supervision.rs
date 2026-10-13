//! VMM supervision against a fake VMM process: this test binary, started
//! again with `CAVE_FAKE_VMM` set, speaks the VMM control protocol on its
//! standard input.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cave_vm_daemon::home::Home;
use cave_vm_daemon::launch::{Launcher, ProcessLauncher};
use cave_vm_daemon::server;
use cave_vm_daemon::vms::{Clock, Prepared, VmSource, Vms};
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
    let mut booted: Option<VmmConfig> = None;
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
            ToVmm::Boot { config } => {
                let reply = fake_restore(&config);
                booted = Some(config);
                reply
            }
            ToVmm::Snapshot { dir } => {
                let config = booted.as_ref().expect("booted");
                let dir = PathBuf::from(dir);
                fs::write(dir.join("state"), "state").unwrap();
                fs::write(dir.join("memory"), "memory").unwrap();
                let disk = fs::read(&config.disk).unwrap_or_default();
                fs::write(dir.join("disk.img"), disk).unwrap();
                FromVmm::Ok
            }
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

/// A boot that restores checks the disk is the snapshot's and refuses a
/// memory file reading `damaged`. What it restored from goes into
/// `restored-from` next to the disk; a normal boot removes that.
fn fake_restore(config: &VmmConfig) -> FromVmm {
    let disk = std::path::Path::new(&config.disk);
    let marker = disk.with_file_name("restored-from");
    let Some(dir) = &config.restore else {
        let _ = fs::remove_file(&marker);
        return FromVmm::Booted;
    };
    let dir = std::path::Path::new(dir);
    if fs::read(dir.join("memory")).unwrap() == b"damaged" {
        return FromVmm::Error(ErrorMessage::new(
            ErrorCode::BadRequest,
            "cannot restore: the memory file is damaged",
        ));
    }
    if fs::read(disk).unwrap() != fs::read(dir.join("disk.img")).unwrap() {
        return FromVmm::Error(ErrorMessage::new(
            ErrorCode::BadRequest,
            "the disk is not the snapshot's",
        ));
    }
    fs::write(&marker, format!("{} {}", dir.display(), config.cpus)).unwrap();
    FromVmm::Booted
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
        disk: match &vm.fork {
            None => format!("{}/.cave/disk.img", vm.project),
            Some(fork) => format!("{}/.cave/forks/{fork}/disk.img", vm.project),
        },
        memory_bytes: 2 << 30,
        cpus: 2,
        net: NetMode::Nat,
        restore: None,
    }
}

/// Counts preparations and makes up a config and policy.
#[derive(Default)]
struct Source {
    prepared: AtomicUsize,
    idle_suspend: Mutex<Option<Duration>>,
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
            idle_suspend: *self.idle_suspend.lock().unwrap(),
        })
    }
}

/// A clock that moves only when a test moves it.
#[derive(Default)]
struct FakeClock(AtomicU64);

impl FakeClock {
    fn advance(&self, by: Duration) {
        self.0
            .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

struct Rig {
    home: Home,
    vms: Arc<Vms>,
    source: Arc<Source>,
    clock: Arc<FakeClock>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.vms.stop_all();
        let _ = fs::remove_dir_all(self.home.root());
        if let Some(dir) = PathBuf::from(vm().project).parent() {
            let _ = fs::remove_dir_all(dir);
        }
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
    let clock = Arc::new(FakeClock::default());
    let vms = Arc::new(
        Vms::new(home.clone(), Arc::new(fake_launcher(mode)), source.clone())
            .with_clock(clock.clone()),
    );
    let listener = server::bind(&home).unwrap();
    let serving = Arc::clone(&vms);
    thread::spawn(move || server::serve(&listener, &serving));
    Rig {
        home,
        vms,
        source,
        clock,
    }
}

/// The VM of this test's own project folder, named after the test's
/// thread so tests running at once do not share one.
fn vm() -> VmId {
    let thread = thread::current();
    let test = thread.name().unwrap_or("main").replace("::", "-");
    let project = std::env::temp_dir()
        .join(format!("cvd-proj-{}-{test}", std::process::id()))
        .join("app");
    VmId {
        project: project.to_string_lossy().into_owned(),
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

    // `stop` suspends it (D20), and the next start resumes it.
    assert_eq!(r.request(Request::Stop { vm: vm() }), Reply::Ok);
    assert_eq!(r.state(), Some(VmState::Suspended));
    assert!(!r.pid_file().exists());
    // Stopping again is fine.
    assert_eq!(r.request(Request::Stop { vm: vm() }), Reply::Ok);
    assert_eq!(r.state(), Some(VmState::Suspended));
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
    let Err(err) = r.vms.agent_connection(&vm()) else {
        panic!("a connection to a stopped VM")
    };
    assert_eq!(err.code, ErrorCode::NotRunning);
    assert!(err.fix.unwrap().contains("cave run"));

    assert!(matches!(r.start(), Reply::Policy(_)));
    for _ in 0..2 {
        let (mut agent, _in_use) = r.vms.agent_connection(&vm()).unwrap();
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

/// A project folder with a disk, for snapshots.
fn snapshot_project(r: &Rig) -> (VmId, PathBuf) {
    let project = r.home.root().join("projects/app");
    fs::create_dir_all(project.join(".cave")).unwrap();
    fs::write(project.join(".cave/disk.img"), "disk v1").unwrap();
    let id = VmId {
        project: project.to_string_lossy().into_owned(),
        fork: None,
    };
    (id, project)
}

impl Rig {
    fn snapshot(&self, id: &VmId, name: Option<&str>) -> Reply {
        self.request(Request::Snapshot {
            vm: id.clone(),
            name: name.map(String::from),
        })
    }

    fn restore(&self, id: &VmId, name: Option<&str>) -> Reply {
        self.request(Request::Restore {
            vm: id.clone(),
            name: name.map(String::from),
        })
    }

    fn snapshots(&self, id: &VmId) -> Vec<(String, bool)> {
        match self.request(Request::List {
            project: Some(id.project.clone()),
        }) {
            Reply::List { entries } => entries
                .into_iter()
                .filter(|e| e.kind == EntryKind::Snapshot)
                .map(|e| (e.name, e.auto))
                .collect(),
            other => panic!("{other:?}"),
        }
    }
}

fn manifest(project: &std::path::Path, name: &str) -> serde_json::Value {
    let text = fs::read_to_string(
        project
            .join(".cave/snapshots")
            .join(name)
            .join("snapshot.json"),
    );
    serde_json::from_str(&text.unwrap()).unwrap()
}

fn error_of(reply: Reply) -> ErrorMessage {
    match reply {
        Reply::Error(e) => e,
        other => panic!("{other:?}"),
    }
}

#[test]
fn snapshots_of_a_running_and_a_stopped_vm() {
    let r = rig("snap", NORMAL);
    let (id, project) = snapshot_project(&r);
    // Nothing booted yet, but the disk is there: a disk-only snapshot.
    let Reply::Snapshot { name, size } = r.snapshot(&id, None) else {
        panic!("no snapshot")
    };
    assert!(
        name.starts_with("app-") && name.ends_with("-debian"),
        "{name}"
    );
    assert!(size > 0);
    assert_eq!(manifest(&project, &name)["has_memory"], false);
    assert_eq!(
        fs::read(project.join(".cave/snapshots").join(&name).join("disk.img")).unwrap(),
        b"disk v1"
    );

    // The default name gets a counter; a running VM's memory is saved.
    assert!(matches!(
        r.request(Request::Start {
            vm: id.clone(),
            net: None,
            ports: Vec::new()
        }),
        Reply::Policy(_)
    ));
    let Reply::Snapshot { name: second, .. } = r.snapshot(&id, None) else {
        panic!("no snapshot")
    };
    assert_eq!(second, format!("{name}-2"));
    let m = manifest(&project, &second);
    assert_eq!(m["has_memory"], true);
    assert_eq!(m["auto"], false);
    assert_eq!(m["vm"]["cpus"], 2);
    assert_eq!(m["image"], "base");
    let dir = project.join(".cave/snapshots").join(&second);
    assert!(dir.join("memory").is_file() && dir.join("state").is_file());

    assert!(matches!(
        r.snapshot(&id, Some("before")),
        Reply::Snapshot { .. }
    ));
    let e = error_of(r.snapshot(&id, Some("before")));
    assert_eq!(e.code, ErrorCode::AlreadyExists);
    assert!(e.fix.unwrap().contains("cave rm before"));
    let e = error_of(r.snapshot(&id, Some("no spaces")));
    assert_eq!(e.code, ErrorCode::BadRequest);
    assert_eq!(
        r.snapshots(&id),
        [
            (name.clone(), false),
            (second, false),
            ("before".to_string(), false)
        ]
    );
    // It runs on, and `cave rm <snapshot>` removes just the snapshot.
    assert_eq!(r.state(), Some(VmState::Running));
    assert_eq!(
        r.request(Request::Remove {
            vm: id.clone(),
            name: Some("before".into())
        }),
        Reply::Ok
    );
    assert!(!project.join(".cave/snapshots/before").exists());
    assert!(project.join(".cave/disk.img").exists());
}

#[test]
fn a_restore_brings_back_the_disk_and_memory_and_can_be_undone() {
    let r = rig("restore", NORMAL);
    let (id, project) = snapshot_project(&r);
    let e = error_of(r.restore(&id, None));
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.message.contains("no snapshots yet"), "{}", e.message);

    assert!(matches!(
        r.request(Request::Start {
            vm: id.clone(),
            net: None,
            ports: Vec::new()
        }),
        Reply::Policy(_)
    ));
    assert!(matches!(
        r.snapshot(&id, Some("before")),
        Reply::Snapshot { .. }
    ));
    fs::write(project.join(".cave/disk.img"), "disk v2").unwrap();

    let e = error_of(r.restore(&id, Some("nope")));
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.fix.unwrap().contains("cave ls"));

    // With no name, the newest snapshot taken by name.
    let Reply::Restored {
        name,
        memory_restored,
        reason,
        saved,
    } = r.restore(&id, None)
    else {
        panic!("not restored")
    };
    assert_eq!(name, "before");
    assert!(memory_restored, "{reason:?}");
    assert_eq!(
        fs::read(project.join(".cave/disk.img")).unwrap(),
        b"disk v1"
    );
    let marker = fs::read_to_string(project.join(".cave/restored-from")).unwrap();
    assert!(marker.ends_with("/snapshots/before 2"), "{marker}");
    assert_eq!(r.state(), Some(VmState::Running));

    // The state before the restore was saved, and restoring it undoes it.
    let saved = saved.expect("an automatic snapshot");
    assert!(r.snapshots(&id).contains(&(saved.clone(), true)));
    assert!(matches!(
        r.restore(&id, Some(&saved)),
        Reply::Restored {
            memory_restored: true,
            ..
        }
    ));
    assert_eq!(
        fs::read(project.join(".cave/disk.img")).unwrap(),
        b"disk v2"
    );
    // Automatic snapshots are passed over for the default.
    assert!(matches!(r.restore(&id, None), Reply::Restored { name, .. } if name == "before"));
}

#[test]
fn a_restore_falls_back_to_the_disk_when_the_memory_cannot_be_loaded() {
    let r = rig("fallback", NORMAL);
    let (id, project) = snapshot_project(&r);
    // Taken while stopped: there is no memory to load.
    assert!(matches!(
        r.snapshot(&id, Some("cold")),
        Reply::Snapshot { .. }
    ));
    match r.restore(&id, Some("cold")) {
        Reply::Restored {
            memory_restored: false,
            reason: Some(reason),
            ..
        } => assert!(reason.contains("without memory state"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!project.join(".cave/restored-from").exists());
    assert_eq!(r.state(), Some(VmState::Running));

    // A damaged memory file: the VMM refuses it, and the VM boots from the
    // snapshot's disk instead.
    assert!(matches!(
        r.snapshot(&id, Some("warm")),
        Reply::Snapshot { .. }
    ));
    let warm = project.join(".cave/snapshots/warm");
    fs::write(warm.join("memory"), "damaged").unwrap();
    fs::write(project.join(".cave/disk.img"), "disk v2").unwrap();
    match r.restore(&id, Some("warm")) {
        Reply::Restored {
            memory_restored: false,
            reason: Some(reason),
            ..
        } => assert!(reason.contains("memory file is damaged"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!project.join(".cave/restored-from").exists());
    assert_eq!(
        fs::read(project.join(".cave/disk.img")).unwrap(),
        b"disk v1"
    );
    assert_eq!(r.state(), Some(VmState::Running));
}

#[test]
fn stop_suspends_and_the_next_start_resumes() {
    let r = rig("suspend", NORMAL);
    let (id, project) = snapshot_project(&r);
    let start = |net| {
        r.request(Request::Start {
            vm: id.clone(),
            net,
            ports: Vec::new(),
        })
    };
    let suspended = project.join(".cave/suspended");
    let marker = project.join(".cave/restored-from");
    assert!(matches!(start(None), Reply::Policy(_)));
    assert_eq!(r.request(Request::Stop { vm: id.clone() }), Reply::Ok);
    assert!(suspended.join("memory").exists());
    let listed = match r.request(Request::List {
        project: Some(id.project.clone()),
    }) {
        Reply::List { entries } => entries,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].state, Some(VmState::Suspended));

    // A snapshot of the suspended VM keeps its memory.
    assert!(matches!(
        r.snapshot(&id, Some("asleep")),
        Reply::Snapshot { .. }
    ));
    assert_eq!(manifest(&project, "asleep")["has_memory"], true);

    // Starting resumes it and removes the suspension.
    assert!(matches!(start(None), Reply::Policy(_)));
    let restored = fs::read_to_string(&marker).unwrap();
    assert!(restored.ends_with("/suspended 2"), "{restored}");
    assert!(!suspended.exists());
    assert_eq!(r.state(), Some(VmState::Running));

    // A start that turns the network off cannot resume, so it boots.
    assert_eq!(r.request(Request::Stop { vm: id.clone() }), Reply::Ok);
    assert!(matches!(start(Some(NetMode::Off)), Reply::Policy(p) if p.net == NetMode::Off));
    assert!(!marker.exists());
    assert!(!suspended.exists());

    // Restoring a snapshot drops a suspension.
    assert_eq!(r.request(Request::Stop { vm: id.clone() }), Reply::Ok);
    assert!(suspended.exists());
    assert!(matches!(
        r.restore(&id, Some("asleep")),
        Reply::Restored { .. }
    ));
    assert!(!suspended.exists());
}

#[test]
fn an_unused_vm_is_suspended_after_its_idle_time() {
    let r = rig("idle", NORMAL);
    let (id, project) = snapshot_project(&r);
    let minutes = |n: u64| Duration::from_secs(n * 60);
    let start = || {
        r.request(Request::Start {
            vm: id.clone(),
            net: None,
            ports: Vec::new(),
        })
    };
    *r.source.idle_suspend.lock().unwrap() = Some(minutes(30));
    assert!(matches!(start(), Reply::Policy(_)));
    r.clock.advance(minutes(29));
    assert!(r.vms.suspend_idle().is_empty());

    // An open agent connection keeps it in use, however long.
    let connection = r.vms.agent_connection(&id).unwrap();
    r.clock.advance(minutes(120));
    assert!(r.vms.suspend_idle().is_empty());
    // The idle time counts from when it closed.
    drop(connection);
    r.clock.advance(minutes(29));
    assert!(r.vms.suspend_idle().is_empty());
    r.clock.advance(minutes(1));
    assert_eq!(r.vms.suspend_idle(), std::slice::from_ref(&id));
    assert!(project.join(".cave/suspended/memory").exists());
    assert_eq!(r.state(), Some(VmState::Suspended));

    // `cave run` on a running VM counts as use.
    assert!(matches!(start(), Reply::Policy(_)));
    assert!(project.join(".cave/restored-from").exists());
    r.clock.advance(minutes(20));
    assert!(matches!(start(), Reply::Policy(_)));
    r.clock.advance(minutes(20));
    assert!(r.vms.suspend_idle().is_empty());
    r.clock.advance(minutes(10));
    assert_eq!(r.vms.suspend_idle(), std::slice::from_ref(&id));

    // `idle_suspend = "off"`: never.
    *r.source.idle_suspend.lock().unwrap() = None;
    assert!(matches!(start(), Reply::Policy(_)));
    r.clock.advance(minutes(24 * 60));
    assert!(r.vms.suspend_idle().is_empty());
}

#[test]
fn every_running_vm_is_suspended_before_the_mac_sleeps() {
    let r = rig("sleep", NORMAL);
    let (id, project) = snapshot_project(&r);
    assert!(matches!(
        r.request(Request::Start {
            vm: id.clone(),
            net: None,
            ports: Vec::new()
        }),
        Reply::Policy(_)
    ));
    assert!(matches!(r.start(), Reply::Policy(_)));
    let mut suspended = r.vms.suspend_all();
    suspended.sort_by(|a, b| a.project.cmp(&b.project));
    let mut want = vec![id, vm()];
    want.sort_by(|a, b| a.project.cmp(&b.project));
    assert_eq!(suspended, want);
    assert!(project.join(".cave/suspended/memory").exists());
    assert!(
        PathBuf::from(vm().project)
            .join(".cave/suspended/memory")
            .exists()
    );
    // Nothing is left running, so a second sleep has nothing to do.
    assert!(r.vms.suspend_all().is_empty());
}

impl Rig {
    fn fork(&self, id: &VmId, name: Option<&str>, from: Option<&str>) -> Reply {
        self.request(Request::Fork {
            vm: id.clone(),
            name: name.map(String::from),
            from: from.map(String::from),
        })
    }

    fn forks(&self, id: &VmId) -> Vec<(String, Option<VmState>)> {
        match self.request(Request::List {
            project: Some(id.project.clone()),
        }) {
            Reply::List { entries } => entries
                .into_iter()
                .filter(|e| e.kind == EntryKind::Fork)
                .map(|e| (e.name, e.state))
                .collect(),
            other => panic!("{other:?}"),
        }
    }
}

fn fork_id(id: &VmId, name: &str) -> VmId {
    VmId {
        fork: Some(name.into()),
        ..id.clone()
    }
}

#[test]
fn a_fork_of_the_running_vm_starts_from_its_memory() {
    let r = rig("fork", NORMAL);
    let (id, project) = snapshot_project(&r);
    assert!(matches!(
        r.request(Request::Start {
            vm: id.clone(),
            net: None,
            ports: Vec::new()
        }),
        Reply::Policy(_)
    ));
    let Reply::Forked { name, from, reason } = r.fork(&id, None, None) else {
        panic!("not forked")
    };
    assert_eq!(
        (name.as_str(), from.as_str(), reason),
        ("fork-1", "app", None)
    );
    let dir = project.join(".cave/forks/fork-1");
    let marker = fs::read_to_string(dir.join("restored-from")).unwrap();
    assert!(marker.ends_with("/forks/fork-1 2"), "{marker}");
    // Booted: the memory is not needed again, since the disk moves on.
    assert!(!dir.join("memory").exists() && !dir.join("state").exists());
    assert_eq!(manifest_in(&dir)["has_memory"], false);
    assert_eq!(
        r.forks(&id),
        [("fork-1".to_string(), Some(VmState::Running))]
    );

    // Parent and fork have their own disks.
    fs::write(project.join(".cave/disk.img"), "disk v2").unwrap();
    assert_eq!(fs::read(dir.join("disk.img")).unwrap(), b"disk v1");

    // The next fork gets the next name; a name in use is refused.
    assert!(matches!(r.fork(&id, None, None), Reply::Forked { name, .. } if name == "fork-2"));
    assert!(matches!(
        r.snapshot(&id, Some("before")),
        Reply::Snapshot { .. }
    ));
    for taken in ["before", "fork-1"] {
        assert_eq!(
            error_of(r.fork(&id, Some(taken), None)).code,
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            error_of(r.snapshot(&id, Some(taken))).code,
            ErrorCode::AlreadyExists
        );
    }

    // A stopped fork is listed as suspended, and resumes on its next start.
    assert_eq!(
        r.request(Request::Stop {
            vm: fork_id(&id, "fork-1")
        }),
        Reply::Ok
    );
    assert!(
        r.forks(&id)
            .contains(&("fork-1".to_string(), Some(VmState::Suspended)))
    );
    assert!(matches!(
        r.request(Request::Start {
            vm: fork_id(&id, "fork-1"),
            net: None,
            ports: Vec::new()
        }),
        Reply::Policy(p) if p.name == "app"
    ));
    assert!(dir.join("restored-from").exists());
    assert!(!dir.join("suspended").exists());

    // `cave rm fork-1` stops and removes just the fork.
    assert_eq!(
        r.request(Request::Remove {
            vm: id.clone(),
            name: Some("fork-1".into())
        }),
        Reply::Ok
    );
    assert!(!dir.exists());
    assert_eq!(
        r.forks(&id),
        [("fork-2".to_string(), Some(VmState::Running))]
    );
    // Removing the project's VM stops its forks too.
    assert_eq!(
        r.request(Request::Remove {
            vm: id.clone(),
            name: None
        }),
        Reply::Ok
    );
    assert!(!project.join(".cave").exists());
    assert!(r.forks(&id).is_empty());
}

#[test]
fn a_fork_from_a_snapshot() {
    let r = rig("forkfrom", NORMAL);
    let (id, project) = snapshot_project(&r);
    let e = error_of(r.fork(&id, Some("try"), Some("nope")));
    assert_eq!(e.code, ErrorCode::NotFound);

    // A snapshot without memory: the fork boots from its disk.
    assert!(matches!(
        r.snapshot(&id, Some("cold")),
        Reply::Snapshot { .. }
    ));
    match r.fork(&id, Some("try"), Some("cold")) {
        Reply::Forked {
            name,
            from,
            reason: Some(reason),
        } => {
            assert_eq!((name.as_str(), from.as_str()), ("try", "cold"));
            assert!(reason.contains("without memory state"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let dir = project.join(".cave/forks/try");
    assert_eq!(fs::read(dir.join("disk.img")).unwrap(), b"disk v1");
    assert_eq!(manifest_in(&dir)["name"], "try");
    // The snapshot is untouched, and the project's VM was not started.
    assert!(project.join(".cave/snapshots/cold/disk.img").is_file());
    assert_eq!(r.state(), Some(VmState::Stopped));
    assert_eq!(r.forks(&id), [("try".to_string(), Some(VmState::Running))]);
}

fn manifest_in(dir: &std::path::Path) -> serde_json::Value {
    let text = fs::read_to_string(dir.join("snapshot.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}
