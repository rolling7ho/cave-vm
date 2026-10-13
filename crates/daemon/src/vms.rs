//! The VMs the daemon knows, and the VMM process behind each running one.
//!
//! A VM is stopped, starting, running or stopping. Starting launches a VMM
//! process, does the control handshake and boots; a watcher thread notices
//! when the process exits, whether asked to or not, so a crashed VMM leaves
//! the VM stopped and the next `start` launches a new one.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cave_vm_proto::daemon::{EntryKind, ListEntry, NetMode, Policy, PortMap, VmId, VmState};
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{ErrorCode, ErrorMessage};

use crate::home::Home;
use crate::launch::{Launched, Launcher};
use crate::link::{BOOT_TIMEOUT, LinkError, VmmLink};

/// The file holding a running VMM's process ID, for cleanup after a daemon
/// crash.
const PID_FILE: &str = "vmm.pid";

/// How long a VMM gets to exit after `stop` before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the watcher checks whether a VMM has exited.
const WATCH_INTERVAL: Duration = Duration::from_millis(20);

/// What a VM needs to boot, from its project: read from `cave.toml` and
/// `.cave/` by the real source, and made up by tests.
#[derive(Debug)]
pub struct Prepared {
    pub config: VmmConfig,
    pub policy: Policy,
}

/// Prepares VMs to boot.
pub trait VmSource: Send + Sync {
    fn prepare(
        &self,
        vm: &VmId,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Prepared, ErrorMessage>;
}

enum Slot {
    Stopped,
    /// Another request is starting or stopping it; wait for the change.
    Busy,
    Running(Running),
}

struct Running {
    link: VmmLink,
    child: Arc<Mutex<Child>>,
    policy: Policy,
    stop_requested: Arc<AtomicBool>,
}

struct Vm {
    id: VmId,
    dir: PathBuf,
    slot: Mutex<Slot>,
    changed: Condvar,
    created_unix: u64,
}

impl Vm {
    /// Locks the slot once no other request is starting or stopping it.
    fn idle_slot(&self) -> MutexGuard<'_, Slot> {
        let mut slot = self.slot.lock().expect("vm lock");
        while matches!(*slot, Slot::Busy) {
            slot = self.changed.wait(slot).expect("vm lock");
        }
        slot
    }

    fn set(&self, mut slot: MutexGuard<'_, Slot>, next: Slot) {
        *slot = next;
        self.changed.notify_all();
    }
}

/// Every VM the daemon knows.
pub struct Vms {
    home: Home,
    launcher: Arc<dyn Launcher>,
    source: Arc<dyn VmSource>,
    vms: Mutex<HashMap<PathBuf, Arc<Vm>>>,
}

fn error(code: ErrorCode, message: impl Into<String>) -> ErrorMessage {
    ErrorMessage::new(code, message)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A VM's display name: the fork's name, or the project folder's.
fn display_name(vm: &VmId) -> String {
    vm.fork.clone().unwrap_or_else(|| {
        Path::new(&vm.project)
            .file_name()
            .map_or_else(|| vm.project.clone(), |n| n.to_string_lossy().into_owned())
    })
}

impl Vms {
    pub fn new(home: Home, launcher: Arc<dyn Launcher>, source: Arc<dyn VmSource>) -> Vms {
        Vms {
            home,
            launcher,
            source,
            vms: Mutex::new(HashMap::new()),
        }
    }

    fn vm(&self, id: &VmId) -> Result<Arc<Vm>, ErrorMessage> {
        let dir = self.home.vm_dir(id).map_err(|e| {
            error(
                ErrorCode::Io,
                format!("cannot create the VM's state folder: {e}"),
            )
        })?;
        let mut vms = self.vms.lock().expect("vms lock");
        Ok(Arc::clone(vms.entry(dir.clone()).or_insert_with(|| {
            Arc::new(Vm {
                id: id.clone(),
                dir,
                slot: Mutex::new(Slot::Stopped),
                changed: Condvar::new(),
                created_unix: now_unix(),
            })
        })))
    }

    /// Starts a VM, or returns its policy if it is already running.
    pub fn start(
        &self,
        id: &VmId,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Policy, ErrorMessage> {
        let vm = self.vm(id)?;
        let slot = vm.idle_slot();
        if let Slot::Running(running) = &*slot {
            return Ok(running.policy.clone());
        }
        vm.set(slot, Slot::Busy);
        let result = self.launch(&vm, net, ports);
        let slot = vm.slot.lock().expect("vm lock");
        match result {
            Ok(running) => {
                let policy = running.policy.clone();
                vm.set(slot, Slot::Running(running));
                Ok(policy)
            }
            Err(e) => {
                vm.set(slot, Slot::Stopped);
                Err(e)
            }
        }
    }

    fn launch(
        &self,
        vm: &Arc<Vm>,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Running, ErrorMessage> {
        let prepared = self.source.prepare(&vm.id, net, ports)?;
        let Launched { child, control } =
            self.launcher
                .launch(&vm.dir, &prepared.config)
                .map_err(|e| {
                    error(ErrorCode::Internal, format!("cannot start the VMM: {e}"))
                        .with_fix("Run `cave doctor` to check the installation.")
                })?;
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let _ = fs::write(vm.dir.join(PID_FILE), pid.to_string());
        let booted = VmmLink::handshake(control).and_then(|mut link| {
            match link.request(
                &ToVmm::Boot {
                    config: prepared.config,
                },
                BOOT_TIMEOUT,
            )? {
                FromVmm::Booted => Ok(link),
                other => Err(LinkError::Unexpected(other)),
            }
        });
        let link = match booted {
            Ok(link) => link,
            Err(e) => {
                let _ = child.lock().expect("child lock").kill();
                let _ = child.lock().expect("child lock").wait();
                let _ = fs::remove_file(vm.dir.join(PID_FILE));
                let log = vm.dir.join(crate::launch::LOG_FILE);
                return Err(
                    error(ErrorCode::Internal, format!("the VM did not boot: {e}"))
                        .with_fix(format!("See {} for the VMM's output.", log.display())),
                );
            }
        };
        watch(
            Arc::clone(vm),
            Arc::clone(&child),
            Arc::clone(&stop_requested),
        );
        Ok(Running {
            link,
            child,
            policy: prepared.policy,
            stop_requested,
        })
    }

    /// A new connection to a running VM's guest agent.
    pub fn agent_connection(&self, id: &VmId) -> Result<UnixStream, ErrorMessage> {
        let vm = self.vm(id)?;
        let mut slot = vm.idle_slot();
        let Slot::Running(running) = &mut *slot else {
            return Err(error(
                ErrorCode::NotRunning,
                format!("{} is not running", display_name(id)),
            )
            .with_fix("start it with `cave run`"));
        };
        running.link.agent_connect().map_err(|e| match e {
            LinkError::Vmm(e) => e,
            e => error(
                ErrorCode::Internal,
                format!("cannot reach the VMM of {}: {e}", display_name(id)),
            ),
        })
    }

    /// Stops a VM. Stopping a stopped VM is not an error.
    pub fn stop(&self, id: &VmId) -> Result<(), ErrorMessage> {
        let vm = self.vm(id)?;
        let mut slot = vm.idle_slot();
        let Slot::Running(running) = std::mem::replace(&mut *slot, Slot::Busy) else {
            vm.set(slot, Slot::Stopped);
            return Ok(());
        };
        drop(slot);
        let Running {
            mut link,
            child,
            stop_requested,
            ..
        } = running;
        stop_requested.store(true, Ordering::Release);
        let _ = link.call(&ToVmm::Stop);
        drop(link);
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            let mut child = child.lock().expect("child lock");
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            drop(child);
            thread::sleep(WATCH_INTERVAL);
        }
        let _ = fs::remove_file(vm.dir.join(PID_FILE));
        let slot = vm.slot.lock().expect("vm lock");
        vm.set(slot, Slot::Stopped);
        Ok(())
    }

    /// The VMs of one project, or of every project the daemon has seen.
    pub fn list(&self, project: Option<&str>) -> Vec<ListEntry> {
        let vms: Vec<Arc<Vm>> = self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .cloned()
            .collect();
        let mut entries: Vec<ListEntry> = vms
            .iter()
            .filter(|vm| project.is_none_or(|p| vm.id.project == p))
            .map(|vm| {
                let state = match *vm.slot.lock().expect("vm lock") {
                    Slot::Running(_) => VmState::Running,
                    Slot::Stopped | Slot::Busy => VmState::Stopped,
                };
                ListEntry {
                    project: vm.id.project.clone(),
                    name: display_name(&vm.id),
                    kind: if vm.id.fork.is_some() {
                        EntryKind::Fork
                    } else {
                        EntryKind::Vm
                    },
                    state: Some(state),
                    auto: false,
                    size: 0,
                    created_unix: vm.created_unix,
                }
            })
            .collect();
        entries.sort_by(|a, b| (&a.project, &a.name).cmp(&(&b.project, &b.name)));
        entries
    }

    /// Stops every running VM, when the daemon exits.
    pub fn stop_all(&self) {
        let ids: Vec<VmId> = self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .map(|vm| vm.id.clone())
            .collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// Kills VMM processes an earlier daemon left behind and removes their
    /// PID files. Returns how many processes were killed.
    pub fn clean_up(&self) -> io::Result<usize> {
        let program = self.launcher.program_name();
        let mut killed = 0;
        for dir in self.home.vm_dirs()? {
            let pid_file = dir.join(PID_FILE);
            let Ok(pid) = fs::read_to_string(&pid_file) else {
                continue;
            };
            if let (Ok(pid), Some(program)) = (pid.trim().parse::<u32>(), &program)
                && process_name(pid).is_some_and(|name| name == *program)
                && kill(pid)
            {
                killed += 1;
            }
            fs::remove_file(&pid_file)?;
        }
        Ok(killed)
    }
}

/// Marks the VM stopped when its VMM exits on its own: a crash, or the
/// guest powering off.
fn watch(vm: Arc<Vm>, child: Arc<Mutex<Child>>, stop_requested: Arc<AtomicBool>) {
    thread::spawn(move || {
        loop {
            thread::sleep(WATCH_INTERVAL);
            if stop_requested.load(Ordering::Acquire) {
                return;
            }
            let exited = matches!(
                child.lock().expect("child lock").try_wait(),
                Ok(Some(_)) | Err(_)
            );
            if !exited {
                continue;
            }
            let mut slot = vm.slot.lock().expect("vm lock");
            while matches!(*slot, Slot::Busy) {
                slot = vm.changed.wait(slot).expect("vm lock");
            }
            if let Slot::Running(running) = &*slot
                && Arc::ptr_eq(&running.child, &child)
            {
                let _ = fs::remove_file(vm.dir.join(PID_FILE));
                vm.set(slot, Slot::Stopped);
            }
            return;
        }
    });
}

/// The executable file name of process `pid`, if it is running.
fn process_name(pid: u32) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || name.is_empty() {
        return None;
    }
    Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

fn kill(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}
