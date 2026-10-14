//! The versioned snapshot format (P4.1, `docs/snapshots.md`).
//!
//! A snapshot is a folder, `.cave/snapshots/<name>/`, holding:
//!
//! - `snapshot.json`, the manifest: format version, VM config and metadata,
//!   and which parts the snapshot has. The daemon writes it last, so a
//!   folder without one is an unfinished snapshot.
//! - `disk.img`, the project disk as an APFS clone.
//! - `memory` and `state`, guest RAM and the machine and device state,
//!   written by the VMM. A snapshot taken of a stopped VM has neither.
//!
//! Snapshots survive a `cave` upgrade. A newer format than this build reads
//! is refused with a fix. Memory state that cannot be loaded, because it is
//! missing or was saved on another macOS build (whose interrupt-controller
//! state may differ), is not an error: restore falls back to the disk and a
//! normal boot (design, "Upgrades").

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use cave_vm_proto::daemon::NetMode;
use serde_json::{Value, json};

/// The format this build writes, and the newest it reads.
pub const FORMAT_VERSION: u64 = 1;

/// The manifest's file name in a snapshot folder.
pub const MANIFEST_FILE: &str = "snapshot.json";
/// The disk clone's file name in a snapshot folder.
pub const DISK_FILE: &str = "disk.img";
/// Guest RAM, written by the VMM.
pub const MEMORY_FILE: &str = "memory";
/// Machine and device state, written by the VMM.
pub const STATE_FILE: &str = "state";

/// The folder in `.cave/` that holds snapshots.
pub const SNAPSHOTS_DIR: &str = "snapshots";
/// The folder in `.cave/` that holds forks, whose names share one
/// namespace with snapshots (`docs/cli.md`).
pub const FORKS_DIR: &str = "forks";

/// A suspended VM's folder, in `.cave/` or in its fork's folder (D20).
pub const SUSPEND_DIR: &str = "suspended";

/// The longest snapshot or fork name.
pub const MAX_NAME_LEN: usize = 64;
/// Room kept at the end of a default name for a counter such as `-12`.
const COUNTER_ROOM: usize = 4;

/// What a snapshot is, apart from its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub format: u64,
    pub name: String,
    pub created_unix: u64,
    /// The `cave` that wrote it, such as `0.1.0`.
    pub cave_version: String,
    /// The macOS build it was taken on, such as `26A434`. Memory state from
    /// another build is not loaded.
    pub host_os: String,
    /// The pinned image, as in `cave.toml`.
    pub image: String,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub net: NetMode,
    /// Names of the credentials injected before the snapshot was taken,
    /// never their values (design, "Credentials"). Filled in by P5.3.
    pub credentials: Vec<String>,
    /// Whether `memory` and `state` were saved.
    pub has_memory: bool,
    /// Taken automatically, such as before a restore, rather than asked
    /// for.
    pub auto: bool,
}

/// Why a manifest cannot be read. The messages say how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Written by a newer `cave`.
    Newer { name: String, format: u64 },
    /// Not JSON, or a field missing or malformed.
    Malformed { name: String, why: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Newer { name, format } => write!(
                f,
                "snapshot {name} was written by a newer cave (format {format}; this cave reads up to {FORMAT_VERSION})\nUpdate cave to restore it"
            ),
            ManifestError::Malformed { name, why } => write!(
                f,
                "snapshot {name} is damaged: {why}\nRestore another snapshot; `cave ls` lists them"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

fn net_name(net: NetMode) -> &'static str {
    match net {
        NetMode::Off => "off",
        NetMode::Nat => "nat",
        NetMode::Lan => "lan",
    }
}

impl Manifest {
    pub fn to_json(&self) -> String {
        let doc = json!({
            "format": self.format,
            "name": self.name,
            "created_unix": self.created_unix,
            "cave_version": self.cave_version,
            "host_os": self.host_os,
            "image": self.image,
            "vm": {
                "cpus": self.cpus,
                "memory_bytes": self.memory_bytes,
                "net": net_name(self.net),
            },
            "credentials": self.credentials,
            "has_memory": self.has_memory,
            "auto": self.auto,
        });
        let mut text = serde_json::to_string_pretty(&doc).expect("a JSON value");
        text.push('\n');
        text
    }

    /// Reads a manifest. `name` is the snapshot folder's, for messages.
    /// Unknown keys are ignored, so a later format of the same version can
    /// add some.
    pub fn parse(name: &str, text: &str) -> Result<Manifest, ManifestError> {
        let bad = |why: String| ManifestError::Malformed {
            name: name.to_string(),
            why,
        };
        let doc: Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
        let format = doc
            .get("format")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("no format version".into()))?;
        if format > FORMAT_VERSION {
            return Err(ManifestError::Newer {
                name: name.to_string(),
                format,
            });
        }
        let string = |v: &Value, key: &str| -> Result<String, ManifestError> {
            v.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| bad(format!("no {key}")))
        };
        let number = |v: &Value, key: &str| -> Result<u64, ManifestError> {
            v.get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| bad(format!("no {key}")))
        };
        let vm = doc.get("vm").ok_or_else(|| bad("no vm".into()))?;
        let net = match string(vm, "net")?.as_str() {
            "off" => NetMode::Off,
            "nat" => NetMode::Nat,
            "lan" => NetMode::Lan,
            other => return Err(bad(format!("unknown net mode {other}"))),
        };
        let credentials = match doc.get("credentials") {
            None => Vec::new(),
            Some(list) => list
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| bad("credentials is not a list of names".into()))?,
        };
        Ok(Manifest {
            format,
            name: string(&doc, "name")?,
            created_unix: number(&doc, "created_unix")?,
            cave_version: string(&doc, "cave_version")?,
            host_os: string(&doc, "host_os")?,
            image: string(&doc, "image")?,
            cpus: u32::try_from(number(vm, "cpus")?).map_err(|_| bad("too many cpus".into()))?,
            memory_bytes: number(vm, "memory_bytes")?,
            net,
            credentials,
            has_memory: doc
                .get("has_memory")
                .and_then(Value::as_bool)
                .ok_or_else(|| bad("no has_memory".into()))?,
            auto: doc.get("auto").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Whether the memory state can be restored on a Mac running
    /// `host_os`, or why restore must boot from the disk instead.
    pub fn memory_usable(&self, host_os: &str) -> Result<(), String> {
        if !self.has_memory {
            return Err("it was taken without memory state".into());
        }
        if self.host_os != host_os {
            return Err(format!(
                "it was taken on macOS build {}, and this Mac runs {host_os}",
                self.host_os
            ));
        }
        Ok(())
    }
}

/// The folder for snapshot `name` in a project's `.cave/`.
pub fn dir(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(SNAPSHOTS_DIR).join(name)
}

/// The folder for fork `name` in a project's `.cave/`. It holds the fork's
/// disk, and until the fork first boots, the snapshot it starts from in
/// the same format as a snapshot folder.
pub fn fork_dir(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(FORKS_DIR).join(name)
}

/// The names of the forks in a project's `.cave/`, sorted.
pub fn fork_names(state_dir: &Path) -> io::Result<Vec<String>> {
    let mut names = match fs::read_dir(state_dir.join(FORKS_DIR)) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().join(DISK_FILE).is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    names.sort();
    Ok(names)
}

/// Writes the manifest into `dir`, replacing it in one step so a crash
/// leaves the old manifest or the new one, never half of one.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let temp = dir.join(format!(".{MANIFEST_FILE}.partial"));
    fs::write(&temp, manifest.to_json())?;
    fs::rename(&temp, dir.join(MANIFEST_FILE))
}

/// Reads the manifest of the snapshot in `dir`.
pub fn read_manifest(dir: &Path) -> Result<Manifest, ManifestError> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text =
        fs::read_to_string(dir.join(MANIFEST_FILE)).map_err(|e| ManifestError::Malformed {
            name: name.clone(),
            why: format!("cannot read {MANIFEST_FILE}: {e}"),
        })?;
    Manifest::parse(&name, &text)
}

/// Checks a snapshot or fork name: 1 to 64 letters, digits, `.`, `_` and
/// `-`, starting with a letter or digit (`docs/cli.md`). The error is a
/// message, a newline and the fix.
pub fn check_name(name: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let starts_well = name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if name.len() <= MAX_NAME_LEN && starts_well && name.chars().all(allowed) {
        return Ok(());
    }
    Err(format!(
        "\"{name}\" is not a snapshot name\nUse 1 to {MAX_NAME_LEN} letters, digits, `.`, `_` and `-`, starting with a letter or digit"
    ))
}

/// Whether a finished snapshot or a fork in a project's `.cave/` is named
/// `name`.
pub fn taken(state_dir: &Path, name: &str) -> bool {
    dir(state_dir, name).join(MANIFEST_FILE).is_file()
        || state_dir.join(FORKS_DIR).join(name).exists()
}

/// The default name, `[ProjectFolder]-[YYYY-MM-DD]-[Distro]` (design,
/// "Snapshot naming"), with `-2`, `-3` and so on appended while `taken`
/// says the name is in use. Characters a name cannot hold become `-`, and
/// a long folder name is shortened so the whole fits.
pub fn default_name(
    project_folder: &str,
    date: &str,
    distro: &str,
    taken: impl Fn(&str) -> bool,
) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let suffix = format!("-{}-{}", clean(date), clean(distro));
    let room = MAX_NAME_LEN.saturating_sub(COUNTER_ROOM + suffix.len());
    let folder = clean(project_folder);
    let folder = folder.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let folder: String = folder.chars().take(room).collect();
    let base = if folder.is_empty() {
        format!("vm{suffix}")
    } else {
        format!("{folder}{suffix}")
    };
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !taken(name))
        .expect("a free counter")
}

/// The local date of `unix` as `YYYY-MM-DD`, through date(1) since std has
/// no time zones; UTC if that fails.
pub fn local_date(unix: u64) -> String {
    Command::new("/bin/date")
        .args(["-r", &unix.to_string(), "+%Y-%m-%d"])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|d| d.len() == 10)
        .unwrap_or_else(|| utc_date(unix))
}

/// The UTC date of `unix` as `YYYY-MM-DD` (the civil-from-days algorithm).
fn utc_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The space the files in `dir` take on disk. Blocks a clone shares with
/// its source are counted in both.
pub fn size(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(fs::Metadata::is_file)
            .map(|m| m.blocks() * 512)
            .sum()
    })
}

/// The names of the finished snapshots in a project's `.cave/`, sorted.
/// Folders without a manifest are unfinished and left out.
pub fn names(state_dir: &Path) -> io::Result<Vec<String>> {
    let mut names = match fs::read_dir(state_dir.join(SNAPSHOTS_DIR)) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().join(MANIFEST_FILE).is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    names.sort();
    Ok(names)
}

/// The automatic snapshots in a project's `.cave/` beyond the newest
/// `keep`, oldest last. Snapshots taken by name are never among them.
pub fn beyond_retention(state_dir: &Path, keep: u32) -> Vec<String> {
    let mut auto: Vec<(u64, std::time::SystemTime, String)> = names(state_dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| {
            let dir = dir(state_dir, &name);
            let manifest = read_manifest(&dir).ok()?;
            // The manifest is written last, to the nanosecond, which
            // orders snapshots taken in the same second.
            let written = fs::metadata(dir.join(MANIFEST_FILE))
                .and_then(|m| m.modified())
                .ok()?;
            manifest
                .auto
                .then_some((manifest.created_unix, written, name))
        })
        .collect();
    // Newest first.
    auto.sort_by(|a, b| b.cmp(a));
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    auto.into_iter()
        .skip(keep)
        .map(|(_, _, name)| name)
        .collect()
}

/// This Mac's macOS build, such as `26A434`. Looked up once: an update
/// restarts the Mac, and the daemon with it.
pub fn host_os_build() -> io::Result<String> {
    static BUILD: OnceLock<String> = OnceLock::new();
    if let Some(build) = BUILD.get() {
        return Ok(build.clone());
    }
    let build = read_host_os_build()?;
    Ok(BUILD.get_or_init(|| build).clone())
}

fn read_host_os_build() -> io::Result<String> {
    let out = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.osversion"])
        .stdin(Stdio::null())
        .output()?;
    let build = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || build.is_empty() {
        return Err(io::Error::other("sysctl kern.osversion failed"));
    }
    Ok(build)
}

#[cfg(test)]
mod tests;
