//! Messages on the daemon API (the `daemon.sock` Unix socket).
//!
//! The daemon speaks the same framing, handshake, errors and stream
//! messages as the agent link. Because a request and its reply can share a
//! `type` (`snapshot`, for one), the two directions have separate enums:
//! [`Request`] is what a client sends, [`Reply`] what the daemon sends.
//!
//! Reply objects mirror the `--json` output of the matching CLI command.

use serde::{Deserialize, Serialize};

use crate::agent::{Exec, PtyOpen, Signal, Stream};
use crate::data::Data;
use crate::error::ErrorMessage;
use crate::frame;
use crate::handshake::Hello;

/// A VM: the project folder's absolute path, and a fork name for a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmId {
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Storage {
    Sealed,
    Synced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetMode {
    Off,
    Nat,
    Lan,
}

/// A forwarded port: `localhost:<mac>` on the Mac reaches `<vm>` in the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMap {
    pub mac: u16,
    pub vm: u16,
}

/// What a client sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello(Hello),

    Init {
        project: String,
        storage: Storage,
        image: String,
    },
    Start {
        vm: VmId,
        /// `--net`; the mode in `cave.toml` when left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        net: Option<NetMode>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ports: Vec<PortMap>,
    },
    Stop {
        vm: VmId,
    },
    /// Every running VM on the Mac, stopped at once without saving
    /// (`cave stop --force`, D43): SIGTERM, then SIGKILL.
    StopAll,
    /// The agent's `exec`, plus credential names. The daemon resolves the
    /// values and passes them only on the agent link.
    Exec {
        vm: VmId,
        #[serde(flatten)]
        exec: Exec,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        credentials: Vec<String>,
    },
    Shell {
        vm: VmId,
        #[serde(flatten)]
        pty: PtyOpen,
    },
    /// A connection to sshd in a running VM, for remote editing. After
    /// `ok` the connection carries raw bytes, without framing.
    Ssh {
        vm: VmId,
    },
    FileRead {
        vm: VmId,
        path: String,
    },
    FileWrite {
        vm: VmId,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        create_dirs: bool,
    },
    Snapshot {
        vm: VmId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The automatic snapshot before an agent run, which counts
        /// towards `[snapshots] keep` (D51). `name` is then ignored.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        before: bool,
    },
    Restore {
        vm: VmId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Fork {
        vm: VmId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// One project, or every known project when `project` is left out.
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    Remove {
        vm: VmId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Inspect {
        vm: VmId,
        /// Most audit entries to return; all of them when left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_limit: Option<u32>,
    },
    Sync {
        vm: VmId,
    },
    /// `cave push`: Mac edits into the VM.
    Pull {
        vm: VmId,
        /// Apply the Mac's version of files changed on both sides too.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    Gc {
        #[serde(default, skip_serializing_if = "is_false")]
        apply: bool,
    },
    Images,
    Doctor,

    // Stream messages from the client during `exec`, `shell` and
    // `file_write`.
    Input {
        data: Data,
    },
    InputEnd,
    Signal {
        signal: Signal,
    },
    PtyData {
        data: Data,
    },
    PtyResize {
        cols: u16,
        rows: u16,
    },
    FileData {
        data: Data,
    },
    FileEnd {
        sha256: String,
    },

    /// A `type` this build does not know; never returned by
    /// [`frame::decode`].
    #[serde(other, skip_serializing)]
    Unknown,
}

/// What the daemon sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Hello(Hello),
    Error(ErrorMessage),
    Ok,
    /// Sent before the result of a long-running request.
    Progress {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        done: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    /// The reply to `start`: what the VM can reach, as the startup policy
    /// prints it.
    Policy(Policy),
    /// The reply to `stop_all`: the VMs it stopped.
    StoppedAll {
        vms: Vec<VmId>,
    },

    // The agent's stream messages, relayed.
    Output {
        stream: Stream,
        data: Data,
    },
    Exit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    PtyData {
        data: Data,
    },
    FileInfo {
        size: u64,
        mode: u32,
    },
    FileData {
        data: Data,
    },
    FileEnd {
        sha256: String,
    },
    FileWritten {
        size: u64,
    },

    Snapshot {
        name: String,
        size: u64,
    },
    Restored {
        /// The snapshot restored.
        #[serde(default)]
        name: String,
        /// `false` when the memory could not be restored and the VM booted
        /// from its disk instead.
        memory_restored: bool,
        /// Why the memory was not restored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The automatic snapshot of the state before the restore, which
        /// undoes it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved: Option<String>,

        /// Credentials injected before the snapshot was taken, by name.
        /// The VM's memory may still hold them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        credentials: Vec<String>,
    },
    Forked {
        name: String,
        /// The snapshot it was made from, or the VM whose current state
        /// was forked.
        #[serde(default)]
        from: String,
        /// Why the fork's memory was not restored, so it booted from the
        /// disk, if it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,

        /// Credentials injected before the snapshot it was made from was
        /// taken, by name. The fork's memory may hold them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        credentials: Vec<String>,
    },
    /// The reply to `sync`. Paths are relative to the project.
    Synced {
        /// Files copied from the VM to the Mac mirror.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        written: Vec<String>,
        /// Files deleted from the mirror because they were deleted in the
        /// VM.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deleted: Vec<String>,
        /// Files changed on both sides, left alone.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        conflicts: Vec<String>,
        /// Files changed only on the Mac, waiting for `pull`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending: Vec<String>,
        /// Files changed only in the VM that it may not change on the Mac,
        /// such as `cave.toml` and `.claude/` (D59), left alone.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        held: Vec<String>,
    },
    /// The reply to `pull`. Paths are relative to the project.
    Pulled {
        /// The snapshot taken before anything was applied; none when there
        /// was nothing to apply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        /// Files copied from the Mac into the VM.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        applied: Vec<String>,
        /// Files deleted in the VM because they were deleted on the Mac.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deleted: Vec<String>,
    },
    List {
        entries: Vec<ListEntry>,
    },
    Inspect {
        policy: Policy,
        resources: Resources,
        audit: Vec<AuditEntry>,
        /// Entries in the whole log, of which `audit` holds the latest.
        audit_total: u64,
        state: VmState,
        /// When the VM was started, while it runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_unix: Option<u64>,
    },
    Usage(Usage),
    Images {
        images: Vec<ImageInfo>,
    },
    Checks {
        checks: Vec<Check>,
    },

    /// A `type` this build does not know; never returned by
    /// [`frame::decode`].
    #[serde(other, skip_serializing)]
    Unknown,
}

/// The startup policy (docs/cli.md, `cave run`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The VM's display name: the project folder's name, or the fork's.
    pub name: String,
    /// The pinned image, as in `cave.toml`.
    pub image: String,
    pub storage: Storage,
    pub net: NetMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
    pub cpus: u32,
    pub memory_bytes: u64,
    /// Seconds until the VM is removed, when a TTL is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// Flags that widened the defaults, echoed back (such as `--net lan`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widened: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Vm,
    Fork,
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Running,
    Suspended,
    Stopped,
}

/// One row of `cave ls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    pub project: String,
    pub name: String,
    pub kind: EntryKind,
    /// For VMs and forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<VmState>,
    /// For snapshots: taken automatically rather than by name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto: bool,
    /// Bytes on disk, counting blocks shared through APFS clones once.
    pub size: u64,
    pub created_unix: u64,
}

/// The resources line of `cave inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub disk_used: u64,
    pub disk_size: u64,
}

/// One audit-log entry. Credentials appear by name only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub time_unix_ms: u64,
    #[serde(flatten)]
    pub event: AuditEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    Exec {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        credentials: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Write {
        path: String,
        size: u64,
    },
    Connect {
        host: String,
        port: u16,
        allowed: bool,
        /// Why a connection was denied, such as `LAN`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// The reply to `gc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub images: Vec<ImageUsage>,
    pub projects: Vec<ProjectUsage>,
    pub reclaimable: Vec<Reclaimable>,
    /// Bytes freed, when `apply` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaimed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUsage {
    pub name: String,
    pub digest: String,
    pub size: u64,
    pub in_use: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectUsage {
    pub project: String,
    pub disk: u64,
    pub snapshots: u64,
    pub forks: u64,
}

/// Something `gc` can delete: unused images, old automatic snapshots, or
/// leftovers from crashed VMM processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reclaimable {
    pub what: String,
    pub size: u64,
}

/// One row of `cave images`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    pub name: String,
    pub digest: String,
    pub downloaded: bool,
    /// Number of projects pinned to this image.
    pub used_by: u32,
    /// Whether `base` stands for this image.
    #[serde(default, skip_serializing_if = "is_false")]
    pub base: bool,
    /// A newer digest the update detector has seen, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
}

/// One line of `cave doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn argv_rule(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() {
        Err("\"argv\" is empty".into())
    } else {
        Ok(())
    }
}

impl Request {
    /// The VM the request acts on, if it names one.
    pub fn vm(&self) -> Option<&VmId> {
        match self {
            Request::Start { vm, .. }
            | Request::Stop { vm }
            | Request::Exec { vm, .. }
            | Request::Shell { vm, .. }
            | Request::FileRead { vm, .. }
            | Request::FileWrite { vm, .. }
            | Request::Snapshot { vm, .. }
            | Request::Restore { vm, .. }
            | Request::Fork { vm, .. }
            | Request::Remove { vm, .. }
            | Request::Inspect { vm, .. }
            | Request::Sync { vm }
            | Request::Pull { vm, .. } => Some(vm),
            _ => None,
        }
    }
}

impl frame::Message for Request {
    fn is_unknown(&self) -> bool {
        matches!(self, Request::Unknown)
    }

    fn validate(&self) -> Result<(), String> {
        if self.vm().is_some_and(|vm| !vm.project.starts_with('/')) {
            return Err("\"project\" must be an absolute path".into());
        }
        match self {
            Request::Exec { exec, .. } => argv_rule(&exec.argv),
            Request::Shell {
                pty: PtyOpen {
                    argv: Some(argv), ..
                },
                ..
            } => argv_rule(argv),
            Request::Init { project, .. } if !project.starts_with('/') => {
                Err("\"project\" must be an absolute path".into())
            }
            _ => Ok(()),
        }
    }
}

impl frame::Message for Reply {
    fn is_unknown(&self) -> bool {
        matches!(self, Reply::Unknown)
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Reply::Exit { code, signal } if code.is_some() == signal.is_some() => {
                Err("needs exactly one of \"code\" and \"signal\"".into())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
