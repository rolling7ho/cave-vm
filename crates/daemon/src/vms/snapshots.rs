//! Snapshots of a project's VM and restores from them (P4.5,
//! `docs/snapshots.md`).
//!
//! A running VM's VMM writes `state`, `memory` and `disk.img` into the
//! snapshot folder while the guest is paused; a stopped VM's snapshot is a
//! clone of its disk. The daemon writes the manifest last.
//!
//! A restore first saves the current state as an automatic snapshot, so it
//! can be undone, then stops the VM, puts the snapshot's disk in place and
//! boots the VMM from the snapshot's memory. If the memory cannot be
//! loaded, the VM boots normally from the restored disk, and the reply says
//! why (design, "Upgrades").

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cave_vm_proto::daemon::{EntryKind, ListEntry, VmId, VmState};
use cave_vm_proto::vmm::{FromVmm, ToVmm};
use cave_vm_proto::{ErrorCode, ErrorMessage};
use cave_vm_vmm::{disk, host};

use super::{Running, Slot, Spawned, Vm, Vms, bind_all, display_name, error, now_unix, serve_all};
use crate::image::ImageRef;
use crate::project::STATE_DIR;
use crate::snapshot::{self, Manifest, SUSPEND_DIR};

/// How long a VMM may take to save a snapshot. It writes out guest memory,
/// so this grows with the VM's memory.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(300);

/// Where [`Vms::take`] saves.
enum Dest<'a> {
    /// The VM's suspension, which its next start resumes.
    Suspend,
    /// A snapshot by this name, or the default one.
    Snapshot(Option<&'a str>),
    /// The folder of the new fork by this name.
    Fork(&'a str),
}

/// A restore's disks: the snapshot's, and the one the VM boots from.
struct Disks {
    snapshot: PathBuf,
    vm: PathBuf,
}

/// What a fork started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    pub name: String,
    /// The snapshot, or the VM whose current state was forked.
    pub from: String,
    /// Why the fork's memory was not restored, if it was not.
    pub fallback: Option<String>,
    /// Credentials the snapshot recorded, by name.
    pub credentials: Vec<String>,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub name: String,
    /// Why the memory was not restored, if it was not.
    pub fallback: Option<String>,
    /// The automatic snapshot taken first.
    pub saved: Option<String>,
    /// Credentials the snapshot recorded, by name.
    pub credentials: Vec<String>,
}

impl Vms {
    /// Saves a project's VM as snapshot `name`, or under the default name.
    /// A running VM's memory is saved too and it runs on; a stopped VM's
    /// disk is saved. Returns the name and the size on disk.
    pub fn snapshot(&self, id: &VmId, name: Option<&str>) -> Result<(String, u64), ErrorMessage> {
        let state = state_dir(id)?;
        if let Some(name) = name {
            check_name(name)?;
        }
        self.take(id, &state, Dest::Snapshot(name), false)
    }

    fn take(
        &self,
        id: &VmId,
        state: &Path,
        dest: Dest<'_>,
        auto: bool,
    ) -> Result<(String, u64), ErrorMessage> {
        let vm = self.vm(id)?;
        let mut slot = vm.idle_slot();
        let mut running = match std::mem::replace(&mut *slot, Slot::Busy) {
            Slot::Running(running) => Some(running),
            _ => None,
        };
        drop(slot);
        let result = self.save(&vm, state, dest, auto, running.as_mut());
        let slot = vm.slot.lock().expect("vm lock");
        vm.set(slot, running.map_or(Slot::Stopped, Slot::Running));
        result
    }

    /// Writes the snapshot while the VM's slot is busy.
    fn save(
        &self,
        vm: &Vm,
        state: &Path,
        dest: Dest<'_>,
        auto: bool,
        running: Option<&mut Running>,
    ) -> Result<(String, u64), ErrorMessage> {
        // A suspended VM's snapshot is its suspension, memory and all.
        let suspended = match (&running, &dest) {
            (Some(_), _) | (None, Dest::Suspend) => None,
            (None, _) => {
                let dir = suspend_dir(&vm.id);
                snapshot::read_manifest(&dir).ok().map(|m| (dir, m))
            }
        };
        let policy = match &running {
            Some(running) => running.policy.clone(),
            None if matches!(dest, Dest::Suspend) => {
                return Err(error(
                    ErrorCode::NotRunning,
                    format!("{} is not running", display_name(&vm.id)),
                ));
            }
            None => {
                if suspended.is_none() && !disk::path(state).is_file() {
                    return Err(error(
                        ErrorCode::NotFound,
                        format!("{} has no VM to snapshot yet", display_name(&vm.id)),
                    )
                    .with_fix("Start it with `cave run`"));
                }
                self.source.prepare(&vm.id, None, &[])?.policy
            }
        };
        let created_unix = now_unix();
        let (name, dir) = match dest {
            Dest::Fork(name) => (name.to_string(), snapshot::fork_dir(state, name)),
            Dest::Suspend => (display_name(&vm.id), suspend_dir(&vm.id)),
            Dest::Snapshot(Some(name)) if snapshot::taken(state, name) => {
                return Err(already_exists(name));
            }
            Dest::Snapshot(name) => {
                let name = name.map_or_else(
                    || {
                        snapshot::default_name(
                            &display_name(&vm.id),
                            &snapshot::local_date(created_unix),
                            &distro(&policy.image),
                            |n| snapshot::taken(state, n),
                        )
                    },
                    str::to_string,
                );
                let dir = snapshot::dir(state, &name);
                (name, dir)
            }
        };
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            error(ErrorCode::Io, format!("cannot {what}: {e}"))
        };
        // An unfinished snapshot of the same name, left by a crash, goes.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| failed("create the snapshot folder", &e))?;
        let has_memory = running.is_some() || suspended.as_ref().is_some_and(|(_, m)| m.has_memory);
        // A snapshot without memory holds no credential the VM was given,
        // short of one written to its disk.
        let credentials = match (&running, &suspended) {
            (Some(running), _) => running.credentials.iter().cloned().collect(),
            (None, Some((_, m))) if m.has_memory => m.credentials.clone(),
            _ => Vec::new(),
        };
        let saved = match (running, &suspended) {
            (Some(running), _) => running
                .link
                .request(
                    &ToVmm::Snapshot {
                        dir: absolute(&dir),
                    },
                    SNAPSHOT_TIMEOUT,
                )
                .map_err(|e| failed("snapshot the VM", &e))
                .and_then(|reply| match reply {
                    FromVmm::Ok => Ok(()),
                    other => Err(failed("snapshot the VM", &format!("{other:?}"))),
                }),
            (None, Some((from, manifest))) => {
                let mut files = vec![snapshot::DISK_FILE];
                if manifest.has_memory {
                    files.extend([snapshot::MEMORY_FILE, snapshot::STATE_FILE]);
                }
                files
                    .into_iter()
                    .try_for_each(|file| clone_flushed(&from.join(file), &dir.join(file)))
                    .map_err(|e| failed("copy the suspended VM", &e))
            }
            (None, None) => clone_flushed(&disk::path(state), &dir.join(snapshot::DISK_FILE))
                .map_err(|e| failed("clone the disk", &e)),
        };
        let manifest = Manifest {
            format: snapshot::FORMAT_VERSION,
            name: name.clone(),
            created_unix,
            cave_version: env!("CARGO_PKG_VERSION").to_string(),
            host_os: snapshot::host_os_build().unwrap_or_else(|_| "unknown".into()),
            image: policy.image,
            cpus: suspended.as_ref().map_or(policy.cpus, |(_, m)| m.cpus),
            memory_bytes: suspended
                .as_ref()
                .map_or(policy.memory_bytes, |(_, m)| m.memory_bytes),
            net: suspended.as_ref().map_or(policy.net, |(_, m)| m.net),
            credentials,
            has_memory,
            auto,
        };
        let written = saved.and_then(|()| {
            snapshot::write_manifest(&dir, &manifest)
                .map_err(|e| failed("write the snapshot manifest", &e))
        });
        if let Err(e) = written {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok((name, snapshot::size(&dir)))
    }

    /// Rolls a project's VM back to snapshot `name`, or to its newest
    /// snapshot, and leaves it running.
    pub fn restore(&self, id: &VmId, name: Option<&str>) -> Result<Restored, ErrorMessage> {
        let state = state_dir(id)?;
        let (name, manifest) = pick(&state, name)?;
        // The next VMM process starts while the current state is saved and
        // the VM stopped.
        let spawned = self.spawn_for(id, &manifest);
        // The undo point, unless automatic snapshots are off. A VM that
        // never booted has nothing to save.
        let settings = self.source.auto_snapshots(id)?;
        let saved = if settings.auto && disk::path(&state).is_file() {
            Some(self.take(id, &state, Dest::Snapshot(None), true)?.0)
        } else {
            None
        };
        // Pruned once the restore is done with the snapshot it restores,
        // which may be an automatic one.
        let spare: Vec<&str> = std::iter::once(name.as_str())
            .chain(saved.as_deref())
            .collect();
        let restored = self.restore_from(id, &state, &name, &manifest, spawned);
        prune(&state, settings.keep, &spare);
        restored.map(|fallback| Restored {
            name: name.clone(),
            fallback,
            saved,
            credentials: manifest.credentials.clone(),
        })
    }

    /// Rolls the VM back to snapshot `name` and boots it from there.
    /// Returns why it booted from the disk alone, if it did.
    fn restore_from(
        &self,
        id: &VmId,
        state: &Path,
        name: &str,
        manifest: &Manifest,
        spawned: Option<Spawned>,
    ) -> Result<Option<String>, ErrorMessage> {
        let dir = snapshot::dir(state, name);
        let ports = self.ports(id)?;
        self.stop(id)?;
        let vm = self.vm(id)?;
        let slot = vm.idle_slot();
        // The VM is rolled back past its suspension, saved above if any.
        let _ = fs::remove_dir_all(suspend_dir(id));
        if !matches!(*slot, Slot::Stopped) {
            return Err(error(
                ErrorCode::Busy,
                format!(
                    "{} was started again while it was being restored",
                    display_name(id)
                ),
            )
            .with_fix("Run `cave restore` again"));
        }
        let bound = bind_all(&ports)?;
        vm.set(slot, Slot::Busy);
        let disks = Disks {
            snapshot: dir.join(snapshot::DISK_FILE),
            vm: disk::path(state),
        };
        let result = self.boot_from(&vm, &dir, manifest, &ports, Some(&disks), spawned);
        let slot = vm.slot.lock().expect("vm lock");
        match result {
            Ok((mut running, fallback)) => {
                running.forwards = serve_all(&vm, bound);
                vm.set(slot, Slot::Running(running));
                Ok(fallback)
            }
            Err(e) => {
                vm.set(slot, Slot::Stopped);
                Err(e)
            }
        }
    }

    /// Starts a VMM process to restore `manifest`'s snapshot in, if one
    /// can be started; the restore starts its own otherwise.
    fn spawn_for(&self, id: &VmId, manifest: &Manifest) -> Option<Spawned> {
        let ports = self.ports(id).ok()?;
        let prepared = self.source.prepare(id, Some(manifest.net), &ports).ok()?;
        self.spawn(&*self.vm(id).ok()?, &prepared.config).ok()
    }

    /// The ports a running VM forwards, to forward again after a restore.
    fn ports(&self, id: &VmId) -> Result<Vec<cave_vm_proto::daemon::PortMap>, ErrorMessage> {
        let vm = self.vm(id)?;
        let slot = vm.idle_slot();
        Ok(match &*slot {
            Slot::Running(running) => running.policy.ports.clone(),
            _ => Vec::new(),
        })
    }

    /// Boots `vm` from the snapshot in `dir`, falling back to a normal boot
    /// when its memory cannot be used. With `disks`, the snapshot's disk is
    /// put in place first, and again before falling back. Returns why it
    /// fell back, if it did.
    fn boot_from(
        &self,
        vm: &std::sync::Arc<Vm>,
        dir: &Path,
        manifest: &Manifest,
        ports: &[cave_vm_proto::daemon::PortMap],
        disks: Option<&Disks>,
        mut spawned: Option<Spawned>,
    ) -> Result<(Running, Option<String>), ErrorMessage> {
        if let Some(disks) = disks {
            replace(&disks.snapshot, &disks.vm)?;
        }
        let host_os = snapshot::host_os_build().unwrap_or_default();
        let why = match manifest.memory_usable(&host_os) {
            Ok(()) => {
                let restoring = self.source.prepare(&vm.id, Some(manifest.net), ports).map(
                    |mut prepared| {
                        prepared.config.restore = Some(absolute(dir));
                        prepared.config.cpus = manifest.cpus;
                        prepared.config.memory_bytes = manifest.memory_bytes;
                        prepared.policy.cpus = manifest.cpus;
                        prepared.policy.memory_bytes = manifest.memory_bytes;
                        prepared
                    },
                )?;
                match self.launch_in(vm, spawned.take(), restoring) {
                    Ok(mut running) => {
                        running.credentials = manifest.credentials.iter().cloned().collect();
                        return Ok((running, None));
                    }
                    Err(e) => {
                        // The guest may have run and written the disk.
                        if let Some(disks) = disks {
                            replace(&disks.snapshot, &disks.vm)?;
                        }
                        e.message
                    }
                }
            }
            Err(why) => why,
        };
        let prepared = self.source.prepare(&vm.id, None, ports)?;
        Ok((self.launch_in(vm, spawned, prepared)?, Some(why)))
    }

    /// Suspends a running VM (D20): saves it, memory and all, where its
    /// next start resumes it, and stops it. Returns false if it was not
    /// running.
    pub fn suspend(&self, id: &VmId) -> Result<bool, ErrorMessage> {
        let vm = self.vm(id)?;
        if !matches!(*vm.idle_slot(), Slot::Running(_)) {
            return Ok(false);
        }
        let state = Path::new(&id.project).join(STATE_DIR);
        // A VM that cannot be saved keeps running rather than lose its
        // processes.
        self.take(id, &state, Dest::Suspend, false).map_err(|e| {
            let message = format!(
                "could not suspend {}: {}; it is still running",
                display_name(id),
                e.message
            );
            ErrorMessage { message, ..e }
        })?;
        self.stop(id)?;
        Ok(true)
    }

    /// Starts a stopped VM: resumes its suspension if it has one, or boots
    /// it. A suspension is used once. A network mode that needs another
    /// set of devices (`off` against `nat` or `lan`) cannot resume, so
    /// the VM boots from its disk.
    pub(super) fn boot(
        &self,
        vm: &std::sync::Arc<Vm>,
        net: Option<cave_vm_proto::daemon::NetMode>,
        ports: &[cave_vm_proto::daemon::PortMap],
    ) -> Result<Running, ErrorMessage> {
        use cave_vm_proto::daemon::NetMode;
        let dir = suspend_dir(&vm.id);
        let Ok(mut manifest) = snapshot::read_manifest(&dir) else {
            let prepared = self.source.prepare(&vm.id, net, ports)?;
            return self.launch(vm, prepared);
        };
        let booted = match net {
            Some(net) if (net == NetMode::Off) != (manifest.net == NetMode::Off) => self
                .source
                .prepare(&vm.id, Some(net), ports)
                .and_then(|prepared| self.launch(vm, prepared)),
            _ => {
                manifest.net = net.unwrap_or(manifest.net);
                self.boot_from(vm, &dir, &manifest, ports, None, None)
                    .map(|(running, _)| running)
            }
        }?;
        let _ = fs::remove_dir_all(&dir);
        Ok(booted)
    }

    /// Makes fork `name` of a project's VM, or `fork-1`, `fork-2` and so on,
    /// from snapshot `from` or from the VM's current state (D18), and
    /// starts it.
    pub fn fork(
        &self,
        id: &VmId,
        name: Option<&str>,
        from: Option<&str>,
    ) -> Result<Forked, ErrorMessage> {
        let state = state_dir(id)?;
        let name = match name {
            Some(name) => {
                check_name(name)?;
                if snapshot::taken(&state, name) {
                    return Err(already_exists(name));
                }
                name.to_string()
            }
            None => (1..)
                .map(|n| format!("fork-{n}"))
                .find(|n| !snapshot::taken(&state, n))
                .expect("a free name"),
        };
        let dir = snapshot::fork_dir(&state, &name);
        let source = match from {
            Some(from) => {
                let (from, manifest) = pick(&state, Some(from))?;
                copy_snapshot(&snapshot::dir(&state, &from), &dir, &name, &manifest)?;
                from
            }
            None => {
                self.take(id, &state, Dest::Fork(&name), false)?;
                display_name(id)
            }
        };
        let fork = VmId {
            project: id.project.clone(),
            fork: Some(name.clone()),
        };
        let booted = self.start_fork(&fork, &dir);
        match booted {
            Ok((fallback, credentials)) => Ok(Forked {
                name,
                from: source,
                fallback,
                credentials,
            }),
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }

    /// Boots a new fork from the snapshot in its folder. The fork's disk
    /// moves on from there, so the snapshot's memory is deleted once
    /// booted: a later boot of the fork starts from its disk. Returns why
    /// its memory was not restored, if it was not, and the credentials the
    /// snapshot recorded.
    fn start_fork(
        &self,
        fork: &VmId,
        dir: &Path,
    ) -> Result<(Option<String>, Vec<String>), ErrorMessage> {
        let manifest = snapshot::read_manifest(dir).map_err(|e| {
            error(
                ErrorCode::Internal,
                format!("cannot read the fork's snapshot: {e}"),
            )
        })?;
        let vm = self.vm(fork)?;
        let slot = vm.idle_slot();
        if !matches!(*slot, Slot::Stopped) {
            return Err(error(
                ErrorCode::Busy,
                format!("{} is already running", display_name(fork)),
            ));
        }
        vm.set(slot, Slot::Busy);
        let result = self.boot_from(&vm, dir, &manifest, &[], None, None);
        let credentials = manifest.credentials.clone();
        if manifest.has_memory {
            let _ = fs::remove_file(dir.join(snapshot::MEMORY_FILE));
            let _ = fs::remove_file(dir.join(snapshot::STATE_FILE));
            let _ = snapshot::write_manifest(
                dir,
                &Manifest {
                    has_memory: false,
                    ..manifest
                },
            );
        }
        let slot = vm.slot.lock().expect("vm lock");
        match result {
            Ok((running, fallback)) => {
                vm.set(slot, Slot::Running(running));
                Ok((fallback, credentials))
            }
            Err(e) => {
                vm.set(slot, Slot::Stopped);
                Err(e)
            }
        }
    }

    /// Removes fork `name` of a project, stopping it first, if it has one
    /// by that name.
    pub(super) fn remove_fork(&self, id: &VmId, name: &str) -> Result<bool, ErrorMessage> {
        let state = Path::new(&id.project).join(STATE_DIR);
        let dir = snapshot::fork_dir(&state, name);
        if check_name(name).is_err() || !dir.is_dir() {
            return Ok(false);
        }
        let fork = VmId {
            project: id.project.clone(),
            fork: Some(name.to_string()),
        };
        self.stop(&fork)?;
        fs::remove_dir_all(&dir)
            .map_err(|e| error(ErrorCode::Io, format!("cannot remove fork {name}: {e}")))?;
        self.forget(&fork);
        Ok(true)
    }

    /// Removes snapshot `name` of a project, if it has one by that name.
    pub(super) fn remove_snapshot(&self, id: &VmId, name: &str) -> Result<bool, ErrorMessage> {
        let state = Path::new(&id.project).join(STATE_DIR);
        let dir = snapshot::dir(&state, name);
        if check_name(name).is_err() || !dir.join(snapshot::MANIFEST_FILE).is_file() {
            return Ok(false);
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| error(ErrorCode::Io, format!("cannot remove snapshot {name}: {e}")))?;
        Ok(true)
    }
}

/// The rows of `cave ls` for a project's forks that are not running, which
/// the daemon may not have seen since it started.
pub(super) fn stopped_forks(project: &str, running: &[String]) -> Vec<ListEntry> {
    let state = Path::new(project).join(STATE_DIR);
    snapshot::fork_names(&state)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !running.contains(name))
        .map(|name| {
            let dir = snapshot::fork_dir(&state, &name);
            let created_unix = fs::metadata(&dir)
                .and_then(|m| m.created())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let fork = VmId {
                project: project.to_string(),
                fork: Some(name.clone()),
            };
            ListEntry {
                project: project.to_string(),
                kind: EntryKind::Fork,
                state: Some(if is_suspended(&fork) {
                    VmState::Suspended
                } else {
                    VmState::Stopped
                }),
                auto: false,
                size: 0,
                created_unix,
                name,
            }
        })
        .collect()
}

/// The rows of `cave ls` for a project's snapshots. A damaged snapshot is
/// listed with what can be read.
pub(super) fn list(project: &str) -> Vec<ListEntry> {
    let state = Path::new(project).join(STATE_DIR);
    snapshot::names(&state)
        .unwrap_or_default()
        .into_iter()
        .map(|name| {
            let dir = snapshot::dir(&state, &name);
            let manifest = snapshot::read_manifest(&dir).ok();
            ListEntry {
                project: project.to_string(),
                kind: EntryKind::Snapshot,
                state: None,
                auto: manifest.as_ref().is_some_and(|m| m.auto),
                size: 0,
                created_unix: manifest.map_or(0, |m| m.created_unix),
                name,
            }
        })
        .collect()
}

/// Deletes the oldest automatic snapshots beyond the newest `keep`,
/// leaving those named in `spare`. Snapshots taken by name are never
/// deleted. Best effort: one that cannot be deleted is tried next time.
fn prune(state: &Path, keep: u32, spare: &[&str]) {
    for name in snapshot::beyond_retention(state, keep) {
        if !spare.contains(&name.as_str()) {
            let _ = fs::remove_dir_all(snapshot::dir(state, &name));
        }
    }
}

/// The snapshot to restore: `name`, or the newest one, preferring those
/// taken by name over automatic ones.
fn pick(state: &Path, name: Option<&str>) -> Result<(String, Manifest), ErrorMessage> {
    let manifest = |name: &str| {
        snapshot::read_manifest(&snapshot::dir(state, name)).map_err(|e| {
            let text = e.to_string();
            match text.split_once('\n') {
                Some((message, fix)) => error(ErrorCode::BadRequest, message).with_fix(fix),
                None => error(ErrorCode::BadRequest, text),
            }
        })
    };
    let names = snapshot::names(state).map_err(|e| {
        error(
            ErrorCode::Io,
            format!("cannot list the snapshots in {}: {e}", state.display()),
        )
    })?;
    if let Some(name) = name {
        if !names.iter().any(|n| n == name) {
            return Err(
                error(ErrorCode::NotFound, format!("no snapshot named {name}"))
                    .with_fix("Run `cave ls` to see this project's snapshots"),
            );
        }
        return Ok((name.to_string(), manifest(name)?));
    }
    let newest = names
        .iter()
        .filter_map(|n| Some((n, snapshot::read_manifest(&snapshot::dir(state, n)).ok()?)))
        .max_by_key(|(n, m)| (!m.auto, m.created_unix, (*n).clone()));
    match newest {
        Some((name, manifest)) => Ok((name.clone(), manifest)),
        None if names.is_empty() => Err(error(
            ErrorCode::NotFound,
            "this project has no snapshots yet",
        )
        .with_fix("Take one with `cave snapshot`")),
        // Only damaged ones: say what is wrong with one.
        None => Err(manifest(&names[0]).expect_err("damaged")),
    }
}

/// Where a VM is suspended to: `.cave/suspended/` for the project's VM, and
/// the same name in a fork's folder for a fork.
pub(super) fn suspend_dir(id: &VmId) -> PathBuf {
    let state = Path::new(&id.project).join(STATE_DIR);
    match &id.fork {
        None => state.join(SUSPEND_DIR),
        Some(name) => snapshot::fork_dir(&state, name).join(SUSPEND_DIR),
    }
}

/// Whether a VM has a suspension to resume.
pub(super) fn is_suspended(id: &VmId) -> bool {
    suspend_dir(id).join(snapshot::MANIFEST_FILE).is_file()
}

/// A project's `.cave/`, for a request on its own VM.
fn state_dir(id: &VmId) -> Result<PathBuf, ErrorMessage> {
    if id.fork.is_some() {
        return Err(error(
            ErrorCode::BadRequest,
            "this build of cave cannot snapshot, restore or fork a fork yet",
        )
        .with_fix("Use the project's own VM, without --fork"));
    }
    let project = Path::new(&id.project);
    if !project.is_absolute() {
        return Err(error(
            ErrorCode::BadRequest,
            format!("{} is not an absolute path", id.project),
        ));
    }
    Ok(project.join(STATE_DIR))
}

fn check_name(name: &str) -> Result<(), ErrorMessage> {
    snapshot::check_name(name).map_err(|text| match text.split_once('\n') {
        Some((message, fix)) => error(ErrorCode::BadRequest, message).with_fix(fix),
        None => error(ErrorCode::BadRequest, text),
    })
}

fn already_exists(name: &str) -> ErrorMessage {
    error(
        ErrorCode::AlreadyExists,
        format!("a snapshot or fork named \"{name}\" already exists"),
    )
    .with_fix(format!(
        "Pick another name, or remove it with `cave rm {name}`"
    ))
}

/// The distribution in a default name: `debian` for `base@sha256:...`.
fn distro(image: &str) -> String {
    image.parse::<ImageRef>().map_or_else(
        |_| image.split('@').next().unwrap_or(image).to_string(),
        |image| image.resolved_name().to_string(),
    )
}

/// Makes `dst`, a new fork's folder, a copy of the snapshot in `src`
/// through APFS clones, with the manifest renamed to the fork.
fn copy_snapshot(
    src: &Path,
    dst: &Path,
    name: &str,
    manifest: &Manifest,
) -> Result<(), ErrorMessage> {
    let failed = |e: std::io::Error| {
        error(
            ErrorCode::Io,
            format!("cannot copy the snapshot for the fork: {e}"),
        )
    };
    let _ = fs::remove_dir_all(dst);
    fs::create_dir_all(dst).map_err(failed)?;
    let mut files = vec![snapshot::DISK_FILE];
    if manifest.has_memory {
        files.extend([snapshot::MEMORY_FILE, snapshot::STATE_FILE]);
    }
    let copied = files
        .into_iter()
        .try_for_each(|file| clone_flushed(&src.join(file), &dst.join(file)))
        .and_then(|()| {
            snapshot::write_manifest(
                dst,
                &Manifest {
                    name: name.to_string(),
                    auto: false,
                    ..manifest.clone()
                },
            )
        });
    if let Err(e) = copied {
        let _ = fs::remove_dir_all(dst);
        return Err(failed(e));
    }
    Ok(())
}

/// Makes `dst` a clone of `src`, replacing it.
fn replace(src: &Path, dst: &Path) -> Result<(), ErrorMessage> {
    let failed = |e: std::io::Error| {
        error(
            ErrorCode::Io,
            format!("cannot put the snapshot's disk in place: {e}"),
        )
    };
    match fs::remove_file(dst) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(failed(e)),
        _ => {}
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(failed)?;
    }
    clone_flushed(src, dst).map_err(failed)
}

/// Makes `dst` an APFS clone of `src` with everything written to `src`.
/// A clone takes the blocks on disk, so pages written but still in the
/// cache, such as a VMM's last writes, are flushed first; that is cheap
/// once they are. Without it a fork's memory sometimes lacked pages and
/// the guest hung.
fn clone_flushed(src: &Path, dst: &Path) -> std::io::Result<()> {
    host::flush(&fs::File::open(src)?)?;
    host::clonefile(src, dst)
}

fn absolute(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_distro_comes_from_the_image() {
        assert_eq!(distro("base"), "debian");
        assert_eq!(distro("ubuntu"), "ubuntu");
        let digest = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(distro(&format!("debian@{digest}")), "debian");
        assert_eq!(distro(&format!("base@{digest}")), "debian");
    }
}
