//! The VMs the daemon knows, and the VMM process behind each running one.
//!
//! A VM is stopped, starting, running or stopping. Starting launches a VMM
//! process, does the control handshake and boots; a watcher thread notices
//! when the process exits, whether asked to or not, so a crashed VMM leaves
//! the VM stopped and the next `start` launches a new one.
//!
//! A running VM is in use while it has an agent connection open (a `cave`
//! command, an SDK or MCP call, or a forwarded port's connection). One
//! left unused for its `idle_suspend` time is suspended
//! ([`Vms::suspend_idle`]).
//!
//! Snapshots and restores of a project's VM are in [`snapshots`].

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

use cave_vm_proto::daemon::{
    EntryKind, ImageInfo, ListEntry, NetMode, Policy, PortMap, VmId, VmState,
};
use cave_vm_proto::vmm::{FromVmm, ToVmm, VmmConfig};
use cave_vm_proto::{ErrorCode, ErrorMessage};

use crate::forward::{self, Bound, Forward};
use crate::home::{Home, PROJECT_FILE};
use crate::launch::{Launched, Launcher};
use crate::link::{BOOT_TIMEOUT, LinkError, VmmLink};
use crate::project::STATE_DIR;

/// The file holding a running VMM's process ID, for cleanup after a daemon
/// crash.
const PID_FILE: &str = "vmm.pid";

/// How long a VMM gets to exit after `stop` before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the watcher checks whether a VMM has exited.
const WATCH_INTERVAL: Duration = Duration::from_millis(20);

mod snapshots;

/// What a VM needs to boot, from its project: read from `cave.toml` and
/// `.cave/` by the real source, and made up by tests.
#[derive(Debug)]
pub struct Prepared {
    pub config: VmmConfig,
    pub policy: Policy,
    /// Suspend the VM after this long unused; `None` never does.
    pub idle_suspend: Option<Duration>,
}

/// A monotonic clock, faked in tests.
pub trait Clock: Send + Sync {
    /// The time since some fixed point.
    fn now(&self) -> Duration;
}

/// The real clock.
pub struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// How much a running VM is used.
struct Activity {
    clock: Arc<dyn Clock>,
    usage: Mutex<Usage>,
}

struct Usage {
    open: usize,
    /// When it was last used.
    last: Duration,
}

impl Activity {
    fn new(clock: Arc<dyn Clock>) -> Activity {
        let last = clock.now();
        Activity {
            clock,
            usage: Mutex::new(Usage { open: 0, last }),
        }
    }

    fn touch(&self) {
        self.usage.lock().expect("usage lock").last = self.clock.now();
    }

    fn idle_for(&self) -> Option<Duration> {
        let usage = self.usage.lock().expect("usage lock");
        (usage.open == 0).then(|| self.clock.now().saturating_sub(usage.last))
    }
}

/// Keeps a VM counted as in use until it is dropped.
pub struct InUse(Arc<Activity>);

impl InUse {
    fn new(activity: &Arc<Activity>) -> InUse {
        let mut usage = activity.usage.lock().expect("usage lock");
        usage.open += 1;
        usage.last = activity.clock.now();
        drop(usage);
        InUse(Arc::clone(activity))
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut usage = self.0.usage.lock().expect("usage lock");
        usage.open -= 1;
        usage.last = self.0.clock.now();
    }
}

/// Prepares VMs to boot.
pub trait VmSource: Send + Sync {
    fn prepare(
        &self,
        vm: &VmId,
        net: Option<NetMode>,
        ports: &[PortMap],
    ) -> Result<Prepared, ErrorMessage>;

    /// The base images for `cave images`, with how many of `projects` use
    /// each.
    fn images(&self, projects: &[PathBuf]) -> Result<Vec<ImageInfo>, ErrorMessage> {
        let _ = projects;
        Err(error(ErrorCode::Internal, "this daemon cannot list images"))
    }
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
    /// Stop listening when the VM stops.
    forwards: Vec<Forward>,
    idle_suspend: Option<Duration>,
    activity: Arc<Activity>,
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
    clock: Arc<dyn Clock>,
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
            clock: Arc::new(SystemClock::default()),
        }
    }

    /// Uses `clock` for idle times, in tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Vms {
        self.clock = clock;
        self
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
            // Best effort: only `cave images` counts use from it.
            let _ = fs::write(dir.join(PROJECT_FILE), &id.project);
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
        let mut slot = vm.idle_slot();
        if let Slot::Running(running) = &mut *slot {
            running.activity.touch();
            // Forwarding goes through the agent, so `-p` on a running VM
            // adds forwards without a restart.
            add_forwards(&vm, running, ports)?;
            return Ok(running.policy.clone());
        }
        // Bound first, so a port in use is reported before the VM boots.
        let bound = bind_all(ports)?;
        vm.set(slot, Slot::Busy);
        let result = self.boot(&vm, net, ports).map(|mut running| {
            running.forwards = serve_all(&vm, bound);
            running
        });
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

    fn launch(&self, vm: &Arc<Vm>, prepared: Prepared) -> Result<Running, ErrorMessage> {
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
            forwards: Vec::new(),
            idle_suspend: prepared.idle_suspend,
            activity: Arc::new(Activity::new(Arc::clone(&self.clock))),
        })
    }

    /// A new connection to a running VM's guest agent. The VM counts as
    /// in use until the [`InUse`] is dropped.
    pub fn agent_connection(&self, id: &VmId) -> Result<(UnixStream, InUse), ErrorMessage> {
        agent_connection(&*self.vm(id)?)
    }

    /// Suspends every running VM left unused for its `idle_suspend` time,
    /// and returns them. A VM that cannot be suspended keeps running and
    /// is tried again on the next call.
    pub fn suspend_idle(&self) -> Vec<VmId> {
        let vms: Vec<Arc<Vm>> = self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .cloned()
            .collect();
        let idle: Vec<VmId> = vms
            .iter()
            .filter(|vm| match &*vm.slot.lock().expect("vm lock") {
                Slot::Running(running) => running.idle_suspend.is_some_and(|limit| {
                    running
                        .activity
                        .idle_for()
                        .is_some_and(|idle| idle >= limit)
                }),
                Slot::Stopped | Slot::Busy => false,
            })
            .map(|vm| vm.id.clone())
            .collect();
        idle.into_iter()
            .filter(|id| match self.suspend(id) {
                Ok(suspended) => suspended,
                Err(e) => {
                    eprintln!(
                        "cave-daemon: {} was idle but could not be suspended: {}",
                        display_name(id),
                        e.message
                    );
                    false
                }
            })
            .collect()
    }

    /// Suspends every running VM at once, before the Mac sleeps, and
    /// returns them. One that cannot be suspended keeps running.
    pub fn suspend_all(&self) -> Vec<VmId> {
        let ids: Vec<VmId> = self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .map(|vm| vm.id.clone())
            .collect();
        thread::scope(|s| {
            let suspending: Vec<_> = ids
                .into_iter()
                .map(|id| s.spawn(move || (self.suspend(&id), id)))
                .collect();
            suspending
                .into_iter()
                .filter_map(|t| match t.join() {
                    Ok((Ok(true), id)) => Some(id),
                    Ok((Err(e), id)) => {
                        eprintln!(
                            "cave-daemon: could not suspend {} before sleep: {}",
                            display_name(&id),
                            e.message
                        );
                        None
                    }
                    Ok((Ok(false), _)) | Err(_) => None,
                })
                .collect()
        })
    }

    /// Calls [`Vms::suspend_idle`] every `interval` on its own thread for
    /// as long as the daemon runs.
    pub fn watch_idle(self: &Arc<Self>, interval: Duration) {
        let vms = Arc::downgrade(self);
        thread::spawn(move || {
            loop {
                thread::sleep(interval);
                let Some(vms) = vms.upgrade() else {
                    return;
                };
                vms.suspend_idle();
            }
        });
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

    /// Removes a project's VM: stops it, then deletes the project's
    /// `.cave/` (disk, snapshots and forks) and the daemon's state for it.
    /// `cave.toml` and the rest of the folder stay. Removing a VM that has
    /// no `.cave/` is not an error.
    pub fn remove(&self, id: &VmId) -> Result<(), ErrorMessage> {
        if let Some(name) = &id.fork {
            let project = VmId {
                fork: None,
                ..id.clone()
            };
            if self.remove_snapshot(&project, name)? || self.remove_fork(&project, name)? {
                return Ok(());
            }
            return Err(error(
                ErrorCode::NotFound,
                format!("no fork or snapshot named {name} in this project"),
            )
            .with_fix("List them with `cave ls`"));
        }
        let project = Path::new(&id.project);
        if !project.is_absolute() {
            return Err(error(
                ErrorCode::BadRequest,
                format!("{} is not an absolute path", id.project),
            ));
        }
        // Forks live in `.cave/` too.
        for fork in self.forks_of(&id.project) {
            self.stop(&fork)?;
            self.forget(&fork);
        }
        self.stop(id)?;
        let vm = self.vm(id)?;
        let slot = vm.idle_slot();
        if !matches!(*slot, Slot::Stopped) {
            return Err(error(
                ErrorCode::Busy,
                format!(
                    "{} was started again while it was being removed",
                    display_name(id)
                ),
            )
            .with_fix("Run `cave rm` again"));
        }
        vm.set(slot, Slot::Busy);
        let result = remove_state(&project.join(STATE_DIR));
        if result.is_ok() {
            let _ = fs::remove_dir_all(&vm.dir);
            self.vms.lock().expect("vms lock").remove(&vm.dir);
        }
        let slot = vm.slot.lock().expect("vm lock");
        vm.set(slot, Slot::Stopped);
        result
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
                    Slot::Stopped if snapshots::is_suspended(&vm.id) => VmState::Suspended,
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
        let mut projects: Vec<String> = match project {
            Some(p) => vec![p.to_string()],
            None => vms.iter().map(|vm| vm.id.project.clone()).collect(),
        };
        projects.sort();
        projects.dedup();
        for project in projects {
            // A suspended VM the daemon has not seen since it started.
            let id = VmId {
                project: project.clone(),
                fork: None,
            };
            let listed = entries
                .iter()
                .any(|e| e.project == project && e.kind == EntryKind::Vm);
            if !listed && snapshots::is_suspended(&id) {
                entries.push(ListEntry {
                    project: project.clone(),
                    name: display_name(&id),
                    kind: EntryKind::Vm,
                    state: Some(VmState::Suspended),
                    auto: false,
                    size: 0,
                    created_unix: 0,
                });
            }
            let known: Vec<String> = entries
                .iter()
                .filter(|e| e.project == project && e.kind == EntryKind::Fork)
                .map(|e| e.name.clone())
                .collect();
            entries.extend(snapshots::stopped_forks(&project, &known));
            entries.extend(snapshots::list(&project));
        }
        entries.sort_by(|a, b| (&a.project, &a.name).cmp(&(&b.project, &b.name)));
        entries
    }

    /// The forks of `project` the daemon knows.
    fn forks_of(&self, project: &str) -> Vec<VmId> {
        self.vms
            .lock()
            .expect("vms lock")
            .values()
            .filter(|vm| vm.id.project == project && vm.id.fork.is_some())
            .map(|vm| vm.id.clone())
            .collect()
    }

    /// Drops a stopped VM and the daemon's folder for it.
    fn forget(&self, id: &VmId) {
        if let Ok(dir) = self.home.vm_dir(id) {
            let _ = fs::remove_dir_all(&dir);
            self.vms.lock().expect("vms lock").remove(&dir);
        }
    }

    /// The base images and how many known projects use each.
    pub fn images(&self) -> Result<Vec<ImageInfo>, ErrorMessage> {
        let projects = self
            .home
            .projects()
            .map_err(|e| error(ErrorCode::Io, format!("cannot list the VM folders: {e}")))?;
        let projects: Vec<PathBuf> = projects.into_iter().filter(|p| p.is_dir()).collect();
        self.source.images(&projects)
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

/// Deletes a project's `.cave/`. A symbolic link there is removed, never
/// followed, so nothing outside the project is deleted.
fn remove_state(state: &Path) -> Result<(), ErrorMessage> {
    let failed = |e: io::Error| {
        error(
            ErrorCode::Io,
            format!("cannot remove {}: {e}", state.display()),
        )
    };
    match fs::symlink_metadata(state) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failed(e)),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(state).map_err(failed),
        Ok(_) => fs::remove_file(state).map_err(failed),
    }
}

fn agent_connection(vm: &Vm) -> Result<(UnixStream, InUse), ErrorMessage> {
    let mut slot = vm.idle_slot();
    let Slot::Running(running) = &mut *slot else {
        return Err(error(
            ErrorCode::NotRunning,
            format!("{} is not running", display_name(&vm.id)),
        )
        .with_fix("start it with `cave run`"));
    };
    let stream = running.link.agent_connect().map_err(|e| match e {
        LinkError::Vmm(e) => e,
        e => error(
            ErrorCode::Internal,
            format!("cannot reach the VMM of {}: {e}", display_name(&vm.id)),
        ),
    })?;
    Ok((stream, InUse::new(&running.activity)))
}

fn bind_all(ports: &[PortMap]) -> Result<Vec<Bound>, ErrorMessage> {
    ports.iter().map(|&map| forward::bind(map)).collect()
}

/// Serves `bound` with agent connections to `vm`. The listener threads
/// hold the VM weakly, so a forgotten VM is not kept alive by them.
fn serve_all(vm: &Arc<Vm>, bound: Vec<Bound>) -> Vec<Forward> {
    let weak = Arc::downgrade(vm);
    let connect: forward::Connect = Arc::new(move || match weak.upgrade() {
        Some(vm) => agent_connection(&vm)
            .map(|(stream, in_use)| (stream, Box::new(in_use) as forward::Held)),
        None => Err(error(ErrorCode::NotRunning, "the VM is gone")),
    });
    bound
        .into_iter()
        .map(|b| b.serve(Arc::clone(&connect)))
        .collect()
}

/// Adds the forwards in `ports` that `running` does not have yet. A Mac
/// port already forwarded to another VM port is refused.
fn add_forwards(
    vm: &Arc<Vm>,
    running: &mut Running,
    ports: &[PortMap],
) -> Result<(), ErrorMessage> {
    let mut new = Vec::new();
    for &map in ports {
        match running.forwards.iter().find(|f| f.map.mac == map.mac) {
            Some(f) if f.map == map => {}
            Some(f) => {
                return Err(error(
                    ErrorCode::AlreadyExists,
                    format!(
                        "port {} on this Mac already forwards to port {} in the VM",
                        map.mac, f.map.vm
                    ),
                )
                .with_fix("Pick another Mac port, or `cave stop` and run again"));
            }
            None if !new.contains(&map) => new.push(map),
            None => {}
        }
    }
    let bound = bind_all(&new)?;
    running.forwards.extend(serve_all(vm, bound));
    for map in new {
        running.policy.ports.push(map);
        running.policy.widened.push(forward::flag(map));
    }
    Ok(())
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
