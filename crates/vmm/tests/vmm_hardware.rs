//! `cave-vmm` snapshots and restores a real VM over its control link: a
//! process started before the snapshot is still running after a restore in
//! a new VMM, and the guest clock is set again. `#[ignore]`d, since it needs
//! the hypervisor entitlement and a built image:
//!
//! ```sh
//! image/debian/build.sh && image/manifest.sh
//! cargo test -p cave-vm-vmm --test vmm_hardware -- --ignored
//! ```
//!
//! `CAVE_E2E_IMAGE_DIR` points at another image folder; the default is
//! `target/image`. The first test's VM has no network. The second needs
//! the internet and runs only when `CAVE_E2E_TCP_TARGET` names a server
//! that keeps an idle TCP connection open for a minute, such as
//! `1.1.1.1:80`.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::fs;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cave_vm_proto::agent::{Exec, Message};
use cave_vm_proto::daemon::NetMode;
use cave_vm_proto::frame::decode;
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{Hello, VERSIONS, read_frame, write_frame};
use cave_vm_vmm::host;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn image_dir() -> PathBuf {
    let dir = std::env::var_os("CAVE_E2E_IMAGE_DIR")
        .map_or_else(|| repo().join("target/image"), PathBuf::from);
    assert!(
        dir.join("debian.img").is_file(),
        "no image in {}; build one with image/debian/build.sh",
        dir.display()
    );
    dir
}

/// One test at a time: each start signs the same `cave-vmm` binary again,
/// and a VMM started while another signing rewrites it fails to launch.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct Vmm {
    link: UnixStream,
    child: Child,
}

impl Vmm {
    /// Starts `cave-vmm`, signed for the hypervisor, and boots `config`.
    fn boot(config: VmmConfig, log: &Path) -> Vmm {
        let (mut link, theirs) = UnixStream::pair().unwrap();
        let log = fs::File::create(log).unwrap();
        let child = Command::new(repo().join("scripts/hypervisor-runner.sh"))
            .arg(env!("CARGO_BIN_EXE_cave-vmm"))
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        link.set_read_timeout(Some(Duration::from_secs(90)))
            .unwrap();
        write_frame(&mut link, &ToVmm::Hello(Hello::client(VERSIONS, "test"))).unwrap();
        assert!(matches!(read(&mut link), FromVmm::Hello(_)));
        write_frame(&mut link, &ToVmm::Boot { config }).unwrap();
        let mut vmm = Vmm { link, child };
        match read(&mut vmm.link) {
            FromVmm::Booted => vmm,
            other => panic!("boot: {other:?}"),
        }
    }

    fn call(&mut self, message: &ToVmm) -> FromVmm {
        write_frame(&mut self.link, message).unwrap();
        read(&mut self.link)
    }

    fn agent(&mut self) -> UnixStream {
        write_frame(&mut self.link, &ToVmm::AgentConnect).unwrap();
        let mut head = [0u8; 4];
        let mut got = 0;
        let mut fd = None;
        while got < 4 {
            let (n, received) = host::recv_with_fd(&self.link, &mut head[got..]).unwrap();
            assert!(n > 0, "the link closed");
            fd = fd.or(received);
            got += n;
        }
        let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
        self.link.read_exact(&mut body).unwrap();
        assert_eq!(decode::<FromVmm>(&body).unwrap(), FromVmm::AgentConnection);
        UnixStream::from(fd.expect("a connection"))
    }

    /// Runs `script` with `sh -c` in the guest and returns its output.
    fn sh(&mut self, script: &str) -> String {
        let mut agent = self.agent();
        agent
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write_frame(&mut agent, &Message::Hello(Hello::client(VERSIONS, "test"))).unwrap();
        assert!(matches!(
            read_frame::<_, Message>(&mut agent).unwrap(),
            Some(Message::Hello(_))
        ));
        let exec = Exec {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: None,
            env: Default::default(),
            stdin: false,
        };
        write_frame(&mut agent, &Message::Exec(exec)).unwrap();
        let mut out = Vec::new();
        loop {
            match read_frame::<_, Message>(&mut agent).unwrap() {
                Some(Message::Output { data, .. }) => out.extend(data.0),
                Some(Message::Exit { code: Some(0), .. }) => break,
                other => panic!(
                    "{script}: {other:?} after {:?}",
                    String::from_utf8_lossy(&out)
                ),
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn stop(mut self) {
        assert_eq!(self.call(&ToVmm::Stop), FromVmm::Ok);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "cave-vmm exited with {status}");
    }
}

fn read(link: &mut UnixStream) -> FromVmm {
    read_frame::<_, FromVmm>(link)
        .unwrap()
        .expect("the VMM closed the link")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement and a built image"]
fn a_process_survives_snapshot_and_restore() {
    let _one = one_at_a_time();
    let images = image_dir();
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("cave-vmm-hw-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let disk = root.join("disk.img");
    host::clonefile(&images.join("debian.img"), &disk).unwrap();
    let snap = root.join("snap");
    let config = VmmConfig {
        kernel: images.join("Image").to_string_lossy().into_owned(),
        disk: disk.to_string_lossy().into_owned(),
        memory_bytes: 1 << 30,
        cpus: 2,
        net: NetMode::Off,
        restore: None,
    };

    let mut vmm = Vmm::boot(config.clone(), &root.join("boot.log"));
    let pid = vmm.sh("sleep 100000 >/dev/null 2>&1 & echo $!");
    let pid = pid.trim();
    vmm.sh("echo before > /tmp/marker && echo written > /var/tmp/on-disk && sync");
    let reply = vmm.call(&ToVmm::Snapshot {
        dir: snap.to_string_lossy().into_owned(),
    });
    assert_eq!(reply, FromVmm::Ok);
    // The VM runs on after the snapshot.
    assert_eq!(vmm.sh("cat /tmp/marker"), "before\n");
    vmm.sh("echo after > /tmp/marker");
    vmm.stop();

    // A new VMM restores it, with the disk from the snapshot.
    let restored_disk = root.join("restored.img");
    host::clonefile(&snap.join("disk.img"), &restored_disk).unwrap();
    let mut vmm = Vmm::boot(
        VmmConfig {
            disk: restored_disk.to_string_lossy().into_owned(),
            restore: Some(snap.to_string_lossy().into_owned()),
            ..config
        },
        &root.join("restore.log"),
    );
    assert_eq!(vmm.sh(&format!("kill -0 {pid} && echo alive")), "alive\n");
    assert_eq!(vmm.sh("cat /tmp/marker"), "before\n");
    assert_eq!(vmm.sh("cat /var/tmp/on-disk"), "written\n");
    let guest_now: u64 = vmm.sh("date +%s").trim().parse().unwrap();
    assert!(
        guest_now.abs_diff(unix_now()) <= 2,
        "guest clock {guest_now}"
    );
    // And it can be snapshotted again, over the snapshot it runs from.
    let reply = vmm.call(&ToVmm::Snapshot {
        dir: snap.to_string_lossy().into_owned(),
    });
    assert_eq!(reply, FromVmm::Ok);
    assert_eq!(vmm.sh(&format!("kill -0 {pid} && echo alive")), "alive\n");
    vmm.stop();
    // Timings, for `--nocapture`.
    for log in ["boot.log", "restore.log"] {
        let text = fs::read_to_string(root.join(log)).unwrap();
        for line in text.lines().filter(|l| l.starts_with("cave-vmm:")) {
            eprintln!("{log}: {line}");
        }
    }
    fs::remove_dir_all(&root).unwrap();
}

/// A fresh, empty folder for a test's files.
fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("cave-vmm-hw-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement, a built image and the internet"]
fn a_connection_from_before_a_snapshot_is_reset_after_a_restore() {
    let Ok(target) = std::env::var("CAVE_E2E_TCP_TARGET") else {
        eprintln!("skipped: set CAVE_E2E_TCP_TARGET to run it");
        return;
    };
    let _one = one_at_a_time();
    let (host, port) = target
        .rsplit_once(':')
        .expect("CAVE_E2E_TCP_TARGET is host:port");
    let images = image_dir();
    let root = scratch("net");
    let disk = root.join("disk.img");
    host::clonefile(&images.join("debian.img"), &disk).unwrap();
    let snap = root.join("snap");
    let config = VmmConfig {
        kernel: images.join("Image").to_string_lossy().into_owned(),
        disk: disk.to_string_lossy().into_owned(),
        memory_bytes: 1 << 30,
        cpus: 1,
        net: NetMode::Nat,
        restore: None,
    };

    let mut vmm = Vmm::boot(config.clone(), &root.join("boot.log"));
    // A reader that blocks on an idle connection and notes when it ends.
    let reader = format!(
        "exec 3<>/dev/tcp/{host}/{port} && echo open > /tmp/conn && \
         cat <&3 >/dev/null 2>/tmp/conn.err; echo closed >> /tmp/conn"
    );
    vmm.sh(&format!(
        "setsid bash -c '{reader}' </dev/null >/dev/null 2>&1 & \
         for i in $(seq 50); do [ -s /tmp/conn ] && break; sleep 0.1; done; cat /tmp/conn"
    ))
    .contains("open")
    .then_some(())
    .expect("the guest could not connect");
    let reply = vmm.call(&ToVmm::Snapshot {
        dir: snap.to_string_lossy().into_owned(),
    });
    assert_eq!(reply, FromVmm::Ok);
    assert_eq!(
        vmm.sh("cat /tmp/conn"),
        "open\n",
        "the connection ended early"
    );
    vmm.stop();

    let restored_disk = root.join("restored.img");
    host::clonefile(&snap.join("disk.img"), &restored_disk).unwrap();
    let mut vmm = Vmm::boot(
        VmmConfig {
            disk: restored_disk.to_string_lossy().into_owned(),
            restore: Some(snap.to_string_lossy().into_owned()),
            ..config
        },
        &root.join("restore.log"),
    );
    // The reader sees the reset within seconds, not a hang.
    let out = vmm.sh(
        "for i in $(seq 100); do grep -q closed /tmp/conn && break; sleep 0.1; done; \
         cat /tmp/conn /tmp/conn.err",
    );
    assert!(
        out.contains("closed"),
        "still open after a restore: {out:?}"
    );
    assert!(out.contains("reset"), "not ended by a reset: {out:?}");
    // And the guest can connect again.
    let again = vmm.sh(&format!(
        "bash -c 'exec 3<>/dev/tcp/{host}/{port}' && echo reconnected"
    ));
    assert_eq!(again, "reconnected\n");
    vmm.stop();
    fs::remove_dir_all(&root).unwrap();
}
