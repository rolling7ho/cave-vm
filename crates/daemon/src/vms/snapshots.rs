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

use cave_vm_proto::daemon::{EntryKind, ListEntry, VmId};
use cave_vm_proto::vmm::{FromVmm, ToVmm};
use cave_vm_proto::{ErrorCode, ErrorMessage};
use cave_vm_vmm::{disk, host};

use super::{Running, Slot, Vm, Vms, bind_all, display_name, error, now_unix, serve_all};
use crate::image::ImageRef;
use crate::project::STATE_DIR;
use crate::snapshot::{self, Manifest};

/// How long a VMM may take to save a snapshot. It writes out guest memory,
/// so this grows with the VM's memory.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(300);

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub name: String,
    /// Why the memory was not restored, if it was not.
    pub fallback: Option<String>,
    /// The automatic snapshot taken first.
    pub saved: Option<String>,
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
        self.take(id, &state, name, false)
    }

    fn take(
        &self,
        id: &VmId,
        state: &Path,
        name: Option<&str>,
        auto: bool,
    ) -> Result<(String, u64), ErrorMessage> {
        let vm = self.vm(id)?;
        let mut slot = vm.idle_slot();
        let mut running = match std::mem::replace(&mut *slot, Slot::Busy) {
            Slot::Running(running) => Some(running),
            _ => None,
        };
        drop(slot);
        let result = self.save(&vm, state, name, auto, running.as_mut());
        let slot = vm.slot.lock().expect("vm lock");
        vm.set(slot, running.map_or(Slot::Stopped, Slot::Running));
        result
    }

    /// Writes the snapshot while the VM's slot is busy.
    fn save(
        &self,
        vm: &Vm,
        state: &Path,
        name: Option<&str>,
        auto: bool,
        running: Option<&mut Running>,
    ) -> Result<(String, u64), ErrorMessage> {
        let policy = match &running {
            Some(running) => running.policy.clone(),
            None => {
                if !disk::path(state).is_file() {
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
        let name = match name {
            Some(name) if snapshot::taken(state, name) => return Err(already_exists(name)),
            Some(name) => name.to_string(),
            None => snapshot::default_name(
                &display_name(&vm.id),
                &snapshot::local_date(created_unix),
                &distro(&policy.image),
                |n| snapshot::taken(state, n),
            ),
        };
        let dir = snapshot::dir(state, &name);
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            error(ErrorCode::Io, format!("cannot {what}: {e}"))
        };
        // An unfinished snapshot of the same name, left by a crash, goes.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| failed("create the snapshot folder", &e))?;
        let has_memory = running.is_some();
        let saved = match running {
            Some(running) => running
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
            None => host::clonefile(&disk::path(state), &dir.join(snapshot::DISK_FILE))
                .map_err(|e| failed("clone the disk", &e)),
        };
        let manifest = Manifest {
            format: snapshot::FORMAT_VERSION,
            name: name.clone(),
            created_unix,
            cave_version: env!("CARGO_PKG_VERSION").to_string(),
            host_os: snapshot::host_os_build().unwrap_or_else(|_| "unknown".into()),
            image: policy.image,
            cpus: policy.cpus,
            memory_bytes: policy.memory_bytes,
            net: policy.net,
            credentials: Vec::new(),
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
        let dir = snapshot::dir(&state, &name);
        // The undo point. A VM that never booted has nothing to save.
        let saved = if disk::path(&state).is_file() {
            Some(self.take(id, &state, None, true)?.0)
        } else {
            None
        };
        let ports = self.ports(id)?;
        self.stop(id)?;
        let vm = self.vm(id)?;
        let slot = vm.idle_slot();
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
        let result = self.roll_back(&vm, &state, &dir, &manifest, &ports);
        let slot = vm.slot.lock().expect("vm lock");
        match result {
            Ok((mut running, fallback)) => {
                running.forwards = serve_all(&vm, bound);
                vm.set(slot, Slot::Running(running));
                Ok(Restored {
                    name,
                    fallback,
                    saved,
                })
            }
            Err(e) => {
                vm.set(slot, Slot::Stopped);
                Err(e)
            }
        }
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

    /// Puts the snapshot's disk in place and boots from the snapshot,
    /// falling back to a normal boot. Returns why it fell back, if it did.
    fn roll_back(
        &self,
        vm: &std::sync::Arc<Vm>,
        state: &Path,
        dir: &Path,
        manifest: &Manifest,
        ports: &[cave_vm_proto::daemon::PortMap],
    ) -> Result<(Running, Option<String>), ErrorMessage> {
        let disk = disk::path(state);
        let snapshot_disk = dir.join(snapshot::DISK_FILE);
        replace(&snapshot_disk, &disk)?;
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
                match self.launch(vm, restoring) {
                    Ok(running) => return Ok((running, None)),
                    Err(e) => {
                        // The guest may have run and written the disk.
                        replace(&snapshot_disk, &disk)?;
                        e.message
                    }
                }
            }
            Err(why) => why,
        };
        let prepared = self.source.prepare(&vm.id, None, ports)?;
        Ok((self.launch(vm, prepared)?, Some(why)))
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
                size: snapshot::size(&dir),
                created_unix: manifest.map_or(0, |m| m.created_unix),
                name,
            }
        })
        .collect()
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

/// A project's `.cave/`, for a request on its own VM.
fn state_dir(id: &VmId) -> Result<PathBuf, ErrorMessage> {
    if id.fork.is_some() {
        return Err(error(
            ErrorCode::BadRequest,
            "this build of cave cannot snapshot or restore forks yet",
        )
        .with_fix("Run it without --fork"));
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
    host::clonefile(src, dst).map_err(failed)
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
