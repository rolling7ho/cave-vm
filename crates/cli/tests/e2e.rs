//! Gate 3 end to end, and snapshots: the real `cave`, `cave-daemon` and
//! `cave-vmm` boot a real VM from a locally built image. `#[ignore]`d, since it needs an
//! Apple Silicon Mac with the hypervisor entitlement and a built image:
//!
//! ```sh
//! image/debian/build.sh && image/manifest.sh
//! cargo build --workspace
//! cargo test -p cave-vm-cli --test e2e -- --ignored
//! ```
//!
//! `CAVE_E2E_IMAGE_DIR` points at another image folder; the default is
//! `target/image`. `CAVE_E2E_KEEP` keeps the test's folder, with the VMM
//! logs. Nothing here reaches the network: the image comes from
//! that folder, and the only connections the guest makes are the denied
//! ones, which the VMM refuses without opening a host socket. Outbound
//! HTTPS, `apt` and Docker need the network and are checked by hand.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Rig {
    root: PathBuf,
    bin: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let built = Path::new(env!("CARGO_BIN_EXE_cave"))
            .parent()
            .unwrap()
            .to_path_buf();
        // Short, so the daemon socket path fits in sun_path.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("cve{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bin = root.join("bin");
        for dir in ["bin", "home/proj", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for name in ["cave", "cave-daemon", "cave-vmm"] {
            let from = built.join(name);
            assert!(
                from.is_file(),
                "{} is missing; run `cargo build --workspace` first",
                from.display()
            );
            fs::copy(&from, bin.join(name)).unwrap();
        }
        // Local ad hoc signing, as scripts/hypervisor-runner.sh does for
        // test binaries; not release signing.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let signed = Command::new("/usr/bin/codesign")
            .args(["--sign", "-", "--force", "--entitlements"])
            .arg(repo.join("scripts/hypervisor.entitlements"))
            .arg(bin.join("cave-vmm"))
            .status()
            .unwrap();
        assert!(signed.success(), "cannot sign cave-vmm");
        Rig { root, bin }
    }

    fn project(&self) -> PathBuf {
        self.root.join("home/proj")
    }

    fn image_dir() -> PathBuf {
        let dir = std::env::var_os("CAVE_E2E_IMAGE_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/image"),
            PathBuf::from,
        );
        assert!(
            dir.join("images.json").is_file(),
            "no image in {}; build one with image/debian/build.sh && image/manifest.sh",
            dir.display()
        );
        dir
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.bin.join("cave"));
        command
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.root.join("home"))
            .env("CAVE_HOME", self.root.join("data"))
            .env("CAVE_IMAGE_DIR", Rig::image_dir())
            .stdin(Stdio::null());
        command
    }

    fn cave(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs `cave exec -- argv` and returns its stdout, asserting `code`.
    fn exec(&self, argv: &[&str], code: i32) -> String {
        self.exec_in(None, argv, code)
    }

    /// [`Rig::exec`] in a fork, or in the project's VM.
    fn exec_in(&self, fork: Option<&str>, argv: &[&str], code: i32) -> String {
        let mut args = vec!["exec"];
        if let Some(fork) = fork {
            args.extend(["--fork", fork]);
        }
        args.push("--");
        args.extend_from_slice(argv);
        let out = self.cave(&args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn vmm_pid(&self) -> Option<u32> {
        let vms = self.root.join("data/vms");
        fs::read_dir(vms)
            .ok()?
            .flatten()
            .find_map(|e| fs::read_to_string(e.path().join("vmm.pid")).ok())
            .and_then(|pid| pid.trim().parse().ok())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.cave(&["stop"]);
        let _ = Command::new("/usr/bin/pkill")
            .arg("-f")
            .arg(self.bin.join("cave-daemon"))
            .status();
        // `CAVE_E2E_KEEP` keeps the folder, with each VMM's log, to look at.
        if std::env::var_os("CAVE_E2E_KEEP").is_some() {
            eprintln!("kept {}", self.root.display());
        } else {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The Mac's own address on its default route, if it has one. Connecting
/// a UDP socket sends nothing.
fn mac_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// `GET path` on localhost:port, or `None` until something answers.
fn http_get(port: u16, path: &str) -> Option<String> {
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).ok()?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(tcp, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut text = String::new();
    tcp.read_to_string(&mut text).ok()?;
    (!text.is_empty()).then_some(text)
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn gate_3() {
    let rig = Rig::new();

    // cave init && cave run && cave exec -- uname -a
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    let run = rig.cave(&["run"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // The policy goes to stderr, keeping stdout for the command's output.
    let policy = String::from_utf8_lossy(&run.stderr);
    assert!(policy.contains("started in"), "{policy}");
    let toml = fs::read_to_string(rig.project().join("cave.toml")).unwrap();
    assert!(toml.contains("@sha256:"), "the image is pinned: {toml}");
    assert!(rig.exec(&["uname", "-a"], 0).contains("aarch64"));
    assert_eq!(rig.exec(&["whoami"], 0).trim(), "cave");
    assert_eq!(rig.exec(&["pwd"], 0).trim(), "/workspace");
    rig.exec(&["false"], 1);
    rig.exec(&["no-such-command"], 127);

    // Every denied range, and the Mac itself, is refused at once in nat
    // mode: curl exits 7 (could not connect), not 28 (timed out).
    let mut denied = vec![
        "10.0.0.1".to_string(),
        "172.16.0.1".into(),
        "192.168.1.1".into(),
        "169.254.169.254".into(),
        "100.64.0.1".into(),
    ];
    denied.extend(mac_address().map(|ip| ip.to_string()));
    for ip in &denied {
        let url = format!("http://{ip}/");
        rig.exec(&["curl", "-s", "-m", "5", "-o", "/dev/null", &url], 7);
    }

    // cave shell, through a pseudo-terminal from script(1).
    let mut script = Command::new("/usr/bin/script");
    script
        .args(["-q", "/dev/null"])
        .arg(rig.bin.join("cave"))
        .arg("shell")
        .current_dir(rig.project())
        .env("HOME", rig.root.join("home"))
        .env("CAVE_HOME", rig.root.join("data"))
        .env("CAVE_IMAGE_DIR", Rig::image_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = script.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo SHELL-$((40+2)) $(whoami)\nexit 3\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SHELL-42 cave"), "{text}");

    // Port forwarding: localhost on the Mac reaches the VM's port.
    let port = free_port();
    let forward = format!("{port}:8000");
    let run = rig.cave(&["run", "-p", &forward]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(String::from_utf8_lossy(&run.stderr).contains(&format!("-p {forward}")));
    let mut server = rig
        .command(&[
            "exec",
            "--",
            "python3",
            "-m",
            "http.server",
            "8000",
            "--bind",
            "127.0.0.1",
            "--directory",
            "/etc",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let page = loop {
        if let Some(page) = http_get(port, "/hostname").filter(|p| p.contains("200 OK")) {
            break page;
        }
        assert!(
            Instant::now() < deadline,
            "the forwarded port never answered"
        );
        thread::sleep(Duration::from_millis(200));
    };
    assert!(page.trim_end().ends_with("cave"), "{page}");
    kill(&mut server);

    // cave cp, both ways.
    let file = rig.project().join("data.txt");
    fs::write(&file, "round trip\n").unwrap();
    assert!(
        rig.cave(&["cp", "data.txt", ":copied.txt"])
            .status
            .success()
    );
    assert_eq!(rig.exec(&["cat", "copied.txt"], 0), "round trip\n");
    let back = rig.root.join("back.txt");
    let out = rig.cave(&["cp", ":copied.txt", back.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&back).unwrap(), "round trip\n");

    // A killed VMM is noticed, and the next command starts a new one.
    let pid = rig.vmm_pid().expect("a running VMM");
    assert!(
        Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).contains("running") {
        assert!(Instant::now() < deadline, "the crash was never noticed");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(rig.exec(&["uname", "-r"], 0).starts_with("6."));
    assert_ne!(rig.vmm_pid(), Some(pid));

    // cave rm keeps cave.toml.
    assert!(rig.cave(&["rm", "--yes"]).status.success());
    assert!(!rig.project().join(".cave").exists());
    assert!(rig.project().join("cave.toml").exists());
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn snapshot_and_restore() {
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    let pid = rig.exec(
        &["sh", "-c", "setsid sleep 100000 >/dev/null 2>&1 & echo $!"],
        0,
    );
    let pid = pid.trim().to_string();
    rig.exec(&["sh", "-c", "echo before > marker && sync"], 0);

    let out = rig.cave(&["snapshot", "before"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("Saved snapshot before ("), "{stdout}");
    // A second one gets the default name.
    let out = rig.cave(&["snapshot"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Saved snapshot proj-20"), "{stdout}");
    assert!(stdout.contains("-debian ("), "{stdout}");
    let ls = String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).into_owned();
    assert!(ls.contains("before") && ls.contains("snapshot"), "{ls}");

    // Changes after the snapshot are rolled back, and the process from
    // before it is running again.
    rig.exec(
        &["sh", "-c", &format!("echo after > marker && kill {pid}")],
        0,
    );
    let out = rig.cave(&["restore", "before"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");
    assert!(stdout.starts_with("Restored before ("), "{stdout}");
    assert_eq!(rig.exec(&["cat", "marker"], 0), "before\n");
    rig.exec(&["kill", "-0", &pid], 0);
    let date = rig.exec(&["date", "+%s"], 0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        date.trim().parse::<u64>().unwrap().abs_diff(now) <= 2,
        "guest clock {date}"
    );

    // The restore saved the state before it, so it can be undone.
    let saved = stdout
        .split("saved as ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .expect("the automatic snapshot's name")
        .to_string();
    assert!(rig.cave(&["restore", &saved]).status.success());
    assert_eq!(rig.exec(&["cat", "marker"], 0), "after\n");

    // A damaged memory file: the restore boots from the snapshot's disk
    // and says so.
    let memory = rig.project().join(".cave/snapshots/before/memory");
    fs::OpenOptions::new()
        .write(true)
        .open(&memory)
        .unwrap()
        .set_len(4096)
        .unwrap();
    let out = rig.cave(&["restore", "before"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: could not load the memory state of before"),
        "{stderr}"
    );
    assert!(stderr.contains("booted normally"), "{stderr}");
    assert_eq!(rig.exec(&["cat", "marker"], 0), "before\n");
    // A fresh boot can give another process the same PID, so check that
    // it is not the sleep.
    rig.exec(
        &[
            "sh",
            "-c",
            &format!("! grep -qs 100000 /proc/{pid}/cmdline"),
        ],
        0,
    );

    // cave rm removes one snapshot.
    assert!(rig.cave(&["rm", "--yes", "before"]).status.success());
    assert!(!rig.project().join(".cave/snapshots/before").exists());

    // cave gc counts the image once and the project's own blocks, and
    // offers nothing that is in use.
    let out = rig.cave(&["gc", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let usage: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    eprintln!("cave gc: {usage}");
    let image = &usage["images"][0];
    assert_eq!(image["in_use"], true, "{usage}");
    let project = &usage["projects"][0];
    let disk = project["disk"].as_u64().unwrap();
    let snapshots = project["snapshots"].as_u64().unwrap();
    // The VM's own writes, far less than the 20 GB disk or the image.
    assert!(
        disk > 0 && disk < image["size"].as_u64().unwrap(),
        "{usage}"
    );
    assert!(snapshots > 0, "{usage}");
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn export_copies_the_project_out_of_the_vm_and_a_snapshot() {
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    rig.exec(
        &[
            "sh",
            "-c",
            "mkdir -p src .git && echo one > src/app.txt && echo ref > .git/HEAD \
             && printf '\\000\\377' > data.bin && sync",
        ],
        0,
    );
    let out = rig.cave(&["snapshot", "first"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    rig.exec(&["sh", "-c", "echo two > src/app.txt"], 0);

    // The running VM, into the default folder.
    let out = rig.cave(&["export"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("Exported 3 files ("), "{stdout}");
    let dir = rig
        .project()
        .join(stdout.trim().rsplit(" to ").next().unwrap());
    assert!(
        dir.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("proj-export-20")
    );
    assert_eq!(
        fs::read_to_string(dir.join("src/app.txt")).unwrap(),
        "two\n"
    );
    assert_eq!(fs::read_to_string(dir.join(".git/HEAD")).unwrap(), "ref\n");
    assert_eq!(fs::read(dir.join("data.bin")).unwrap(), [0, 255]);
    // It does not write over a folder with files in it.
    let out = rig.cave(&["export"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));

    // A snapshot, through a fork that is gone afterwards.
    let out = rig.cave(&["export", "first", "-o", "old"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let old = rig.project().join("old");
    assert_eq!(
        fs::read_to_string(old.join("src/app.txt")).unwrap(),
        "one\n"
    );
    let ls = String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).into_owned();
    assert!(!ls.contains("export-"), "{ls}");
    assert_eq!(rig.exec(&["cat", "src/app.txt"], 0), "two\n");
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn stop_suspends_and_the_next_command_resumes() {
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    let pid = rig.exec(
        &["sh", "-c", "setsid sleep 100000 >/dev/null 2>&1 & echo $!"],
        0,
    );
    let pid = pid.trim().to_string();

    let started = Instant::now();
    let out = rig.cave(&["stop"]);
    let suspended = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("Suspended proj ("), "{stdout}");
    let ls = String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).into_owned();
    assert!(
        ls.lines()
            .any(|l| l.starts_with("proj") && l.contains("suspended")),
        "{ls}"
    );

    // The next command resumes it with the process from before.
    let started = Instant::now();
    rig.exec(&["kill", "-0", &pid], 0);
    let resumed = started.elapsed();
    assert!(!rig.project().join(".cave/suspended").exists());

    // Turning the network off cannot resume, so it boots from the disk.
    assert!(rig.cave(&["stop"]).status.success());
    let out = rig.cave(&["run", "--net", "off"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A fresh boot can give another process the same PID, so check that
    // it is not the sleep.
    rig.exec(
        &[
            "sh",
            "-c",
            &format!("! grep -qs 100000 /proc/{pid}/cmdline"),
        ],
        0,
    );

    // An unused VM suspends itself after `idle_suspend`.
    let toml = rig.project().join("cave.toml");
    let text = fs::read_to_string(&toml).unwrap();
    fs::write(
        &toml,
        text.replacen("[vm]\n", "[vm]\nidle_suspend = \"1s\"\n", 1),
    )
    .unwrap();
    assert!(rig.cave(&["stop"]).status.success());
    assert!(rig.cave(&["run", "--net", "off"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ls = String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).into_owned();
        if ls
            .lines()
            .any(|l| l.starts_with("proj") && l.contains("suspended"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "not suspended: {ls}");
        thread::sleep(Duration::from_millis(500));
    }

    eprintln!("cave stop: {suspended:?}; resumed and ran a command: {resumed:?}");
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn forks_run_on_their_own() {
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    let pid = rig.exec(
        &["sh", "-c", "setsid sleep 100000 >/dev/null 2>&1 & echo $!"],
        0,
    );
    let pid = pid.trim().to_string();
    rig.exec(&["sh", "-c", "echo parent > marker && sync"], 0);
    let out = rig.cave(&["snapshot", "base"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // From a snapshot: timed until the fork has run a command, the gate 4
    // measure (docs/decisions/0005-spike-report.md).
    let started = Instant::now();
    let out = rig.cave(&["fork", "try", "--from", "base"]);
    let forked = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with("Forked try from base (")
            && stdout.contains("Use it with: cave shell --fork try"),
        "{stdout}"
    );
    assert_eq!(rig.exec_in(Some("try"), &["cat", "marker"], 0), "parent\n");
    let first_command = started.elapsed();

    // From the current state, with the default name.
    let started = Instant::now();
    let out = rig.cave(&["fork"]);
    let fresh = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("Forked fork-1 from proj ("), "{stdout}");

    // All three run on their own, each with the process from before.
    for (vm, text) in [(None, "p2"), (Some("try"), "t2"), (Some("fork-1"), "f2")] {
        rig.exec_in(vm, &["sh", "-c", &format!("echo {text} > marker")], 0);
    }
    for (vm, text) in [(None, "p2"), (Some("try"), "t2"), (Some("fork-1"), "f2")] {
        assert_eq!(rig.exec_in(vm, &["cat", "marker"], 0), format!("{text}\n"));
        rig.exec_in(vm, &["kill", "-0", &pid], 0);
    }
    rig.exec_in(Some("try"), &["kill", &pid], 0);
    rig.exec(&["kill", "-0", &pid], 0);

    let ls = String::from_utf8_lossy(&rig.cave(&["ls"]).stdout).into_owned();
    for name in ["try", "fork-1"] {
        assert!(
            ls.lines()
                .any(|l| l.starts_with(name) && l.contains("fork") && l.contains("running")),
            "{ls}"
        );
    }
    assert!(rig.cave(&["stop", "--fork", "fork-1"]).status.success());
    assert!(rig.cave(&["rm", "--yes", "try"]).status.success());
    assert!(!rig.project().join(".cave/forks/try").exists());
    assert_eq!(rig.exec(&["cat", "marker"], 0), "p2\n");

    eprintln!(
        "cave fork --from: {forked:?}; until the fork ran a command: {first_command:?}; \
         cave fork of the running VM: {fresh:?}"
    );
    // The VMMs' own timings, for `--nocapture`.
    for dir in fs::read_dir(rig.root.join("data/vms")).unwrap().flatten() {
        let log = fs::read_to_string(dir.path().join("vmm.log")).unwrap_or_default();
        for line in log.lines().filter(|l| l.starts_with("cave-vmm:")) {
            eprintln!("{line}");
        }
    }
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn credentials_reach_the_command_but_not_the_agent() {
    const VALUE: &str = "snednao_#m34nr";
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    fs::write(rig.project().join(".env"), format!("API_KEY={VALUE}\n")).unwrap();
    rig.exec(&["true"], 0);

    // Through the MCP server, as an agent would. The value is printed
    // whole, then split across two writes.
    let script = "test \"${#API_KEY}\" = 14 && echo \"API_KEY: $API_KEY\" && \
                  printf %s \"${API_KEY:0:5}\" && sleep 0.3 && printf '%s\\n' \"${API_KEY:5}\"";
    let calls = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "1"}}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "exec", "arguments": {"command": script, "credentials": ["API_KEY"]}}}),
    ];
    let mut input = String::new();
    for call in &calls {
        input.push_str(&format!("{call}\n"));
    }
    let mut mcp = rig
        .command(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    mcp.stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = mcp.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains(VALUE), "{stdout}");
    assert!(
        stdout.contains("exit code 0") && stdout.contains("API_KEY: [API_KEY]\\n[API_KEY]\\n"),
        "{stdout}"
    );

    // The audit log and a snapshot name it, but never hold the value.
    let log = fs::read_to_string(rig.project().join(".cave/audit.log")).unwrap();
    assert!(log.contains(r#""credentials":["API_KEY"]"#), "{log}");
    assert!(!log.contains(VALUE));
    assert!(rig.cave(&["snapshot", "after"]).status.success());
    let manifest =
        fs::read_to_string(rig.project().join(".cave/snapshots/after/snapshot.json")).unwrap();
    assert!(!manifest.contains(VALUE));
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(manifest["credentials"], serde_json::json!(["API_KEY"]));

    // Forking it warns.
    let out = rig.cave(&["fork", "f", "--from", "after"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: snapshot after was taken after API_KEY was injected")
            && stderr.contains("the fork's memory may contain it"),
        "{stderr}"
    );
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn agent_on_the_mac_reaches_the_vm_only_through_mcp() {
    // A real Claude Code run uses the signed-in account, so it runs only
    // when asked for.
    if std::env::var_os("CAVE_E2E_CLAUDE").is_none() {
        eprintln!("skipped: set CAVE_E2E_CLAUDE=1 to run Claude Code");
        return;
    }
    let rig = Rig::new();
    assert!(rig.cave(&["init", "--sealed"]).status.success());
    rig.exec(&["sh", "-c", "echo from-the-vm > /workspace/marker"], 0);
    let path = format!(
        "{}:{}",
        rig.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = rig
        .command(&[
            "agent",
            "claude",
            "--",
            "-p",
            "Read the file marker with the cave read_file tool, then run `uname -s` with the cave exec tool. \
             Reply with exactly two lines: the file's contents, then the command's output.",
            "--allowedTools",
            "mcp__cave__read_file,mcp__cave__exec",
        ])
        .env("PATH", path)
        .env("HOME", std::env::var("HOME").unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("from-the-vm") && stdout.contains("Linux"),
        "{stdout}"
    );
    assert!(
        stderr.contains("Undo everything the agent did with: cave restore proj-"),
        "{stderr}"
    );
    // The audit log shows what it did.
    let log = fs::read_to_string(rig.project().join(".cave/audit.log")).unwrap();
    assert!(log.contains("uname -s"), "{log}");
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn editors_reach_sshd_in_the_vm_through_the_proxy() {
    let rig = Rig::new();
    let out = rig.cave(&["init", "--sealed"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("connect VS Code Remote-SSH or Zed to proj.cave"),
        "{stdout}"
    );
    rig.exec(&["sh", "-c", "echo edited-in-vm > /workspace/marker"], 0);
    // ssh reads the config of the real user's home, not $HOME, so the
    // rig's config is passed by hand, as an editor would find it.
    let ssh = |args: &[&str]| {
        Command::new("/usr/bin/ssh")
            .arg("-F")
            .arg(rig.root.join("home/.ssh/config"))
            .args(["-o", "BatchMode=yes", "proj.cave"])
            .args(args)
            .env("HOME", rig.root.join("home"))
            .env("CAVE_HOME", rig.root.join("data"))
            .env("CAVE_IMAGE_DIR", Rig::image_dir())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = ssh(&["cat", "/workspace/marker", "&&", "whoami"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "edited-in-vm\ncave\n");
    // The key is added once.
    assert!(ssh(&["true"]).status.success());
    assert_eq!(
        rig.exec(&["sh", "-c", "wc -l < ~/.ssh/authorized_keys"], 0)
            .trim(),
        "1"
    );
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn synced_mode_mirrors_the_vm_and_pulls_mac_edits() {
    let rig = Rig::new();
    let mac = rig.project();
    assert!(rig.cave(&["init", "--synced"]).status.success());
    fs::create_dir_all(mac.join("src")).unwrap();
    fs::write(mac.join("src/app.ts"), "one\n").unwrap();
    fs::write(mac.join("both.txt"), "base\n").unwrap();
    rig.exec(&["true"], 0);
    let sync = |rig: &Rig| {
        let out = rig.cave(&["sync"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // Whatever the VM lacks comes in with a pull, and then both sides
    // agree.
    let out = rig.cave(&["pull"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(rig.exec(&["cat", "src/app.ts"], 0), "one\n");
    assert_eq!(sync(&rig), "The Mac mirror is up to date\n");

    // VM edits reach the Mac; the VM's .env and node_modules do not.
    rig.exec(
        &[
            "sh",
            "-c",
            "echo two > src/app.ts && echo new > new.txt && echo K=v > .env && \
             mkdir -p node_modules/x && echo y > node_modules/x/i.js",
        ],
        0,
    );
    let out = sync(&rig);
    assert!(
        out.contains("Wrote 2 files to your Mac: new.txt, src/app.ts"),
        "{out}"
    );
    assert_eq!(fs::read_to_string(mac.join("src/app.ts")).unwrap(), "two\n");
    assert!(!mac.join(".env").exists() && !mac.join("node_modules").exists());

    // A Mac edit is never written over by a sync, and a pull brings it in
    // after a snapshot.
    fs::write(mac.join("src/app.ts"), "mac\n").unwrap();
    assert!(
        sync(&rig).contains("1 file you changed on your Mac waits for `cave pull`: src/app.ts")
    );
    let out = rig.cave(&["pull"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("Saved snapshot ") && stdout.contains(" before pulling"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Applied 1 file from your Mac: src/app.ts"),
        "{stdout}"
    );
    assert_eq!(rig.exec(&["cat", "src/app.ts"], 0), "mac\n");

    // A file changed on both sides stops the pull and is listed.
    fs::write(mac.join("both.txt"), "mac\n").unwrap();
    rig.exec(&["sh", "-c", "echo vm > both.txt"], 0);
    let out = rig.cave(&["pull"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("1 file changed both on your Mac and in the VM since the last sync, so nothing was pulled\n  both.txt\n  fix: "),
        "{stderr}"
    );
    assert_eq!(rig.exec(&["cat", "both.txt"], 0), "vm\n");

    // A snapshot syncs too, once the client has its answer.
    rig.exec(&["sh", "-c", "echo snap > snap.txt"], 0);
    assert!(rig.cave(&["snapshot", "s1"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !mac.join("snap.txt").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(fs::read_to_string(mac.join("snap.txt")).unwrap(), "snap\n");
}
