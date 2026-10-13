//! `cave run`, `cave stop`, `cave ls` and `cave images`.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use cave_vm_proto::daemon::{
    EntryKind, ImageInfo, ListEntry, NetMode, PortMap, Reply, Request, VmId, VmState,
};

use crate::client::Connection;
use crate::error::{CliError, Result};
use crate::init;
use crate::policy;
use crate::project;

pub fn vm_id(project: &Path, fork: Option<String>) -> VmId {
    VmId {
        project: project::wire_path(project),
        fork,
    }
}

/// `-p 3000` or `-p 3001:3000`.
pub fn parse_port(arg: &str) -> std::result::Result<PortMap, String> {
    let port = |s: &str| {
        s.parse::<u16>()
            .ok()
            .filter(|&p| p > 0)
            .ok_or_else(|| format!("\"{s}\" is not a port number from 1 to 65535"))
    };
    match arg.split_once(':') {
        Some((mac, vm)) => Ok(PortMap {
            mac: port(mac)?,
            vm: port(vm)?,
        }),
        None => {
            let p = port(arg)?;
            Ok(PortMap { mac: p, vm: p })
        }
    }
}

/// The VM's entry in `list`, if the daemon knows it.
fn entry(vm: &VmId) -> Result<Option<ListEntry>> {
    let Reply::List { entries } = Connection::call(&Request::List {
        project: Some(vm.project.clone()),
    })?
    else {
        return Err(CliError::new("the daemon sent an unexpected reply to list"));
    };
    let kind = if vm.fork.is_some() {
        EntryKind::Fork
    } else {
        EntryKind::Vm
    };
    let name = vm.fork.clone();
    Ok(entries
        .into_iter()
        .find(|e| e.kind == kind && name.as_ref().is_none_or(|n| *n == e.name)))
}

fn is_running(vm: &VmId) -> Result<bool> {
    Ok(entry(vm)?.is_some_and(|e| e.state == Some(VmState::Running)))
}

/// Starts the VM if needed. Prints the policy when it boots it, and when
/// `always_print` is set (`cave run`) also if it was already running.
pub fn ensure_running(
    vm: &VmId,
    net: Option<NetMode>,
    ports: Vec<PortMap>,
    always_print: bool,
) -> Result<()> {
    let was_running = is_running(vm)?;
    let started = Instant::now();
    let Reply::Policy(policy) = Connection::call(&Request::Start {
        vm: vm.clone(),
        net,
        ports,
    })?
    else {
        return Err(CliError::new(
            "the daemon sent an unexpected reply to start",
        ));
    };
    if was_running {
        if always_print {
            eprintln!("{}", policy::render(&policy, "already running"));
        }
    } else {
        let headline = format!("started in {}", policy::seconds(started.elapsed()));
        eprintln!("{}", policy::render(&policy, &headline));
    }
    Ok(())
}

/// `cave run`.
pub fn run(net: Option<NetMode>, ports: Vec<PortMap>, fork: Option<String>) -> Result<()> {
    let project = init::project_or_set_up()?;
    ensure_running(&vm_id(&project, fork), net, ports, true)
}

/// `cave stop`.
pub fn stop(fork: Option<String>) -> Result<()> {
    let project = project::require()?;
    let vm = vm_id(&project, fork);
    let name = vm.fork.clone().unwrap_or_else(|| folder_name(&project));
    if !is_running(&vm)? {
        println!("{name} is not running");
        return Ok(());
    }
    let started = std::time::Instant::now();
    Connection::call(&Request::Stop { vm })?;
    println!(
        "Suspended {name} ({})",
        crate::policy::seconds(started.elapsed())
    );
    Ok(())
}

fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn size(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1 << 30))
}

/// Local time as `2026-10-12 09:14`, through date(1), since std has no time
/// zones.
fn local_time(unix: u64) -> String {
    Command::new("/bin/date")
        .args(["-r", &unix.to_string(), "+%Y-%m-%d %H:%M"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || unix.to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        )
}

/// `cave ls`.
pub fn ls(all: bool, json: bool) -> Result<()> {
    let cwd = project::current_dir()?;
    let project = if all {
        None
    } else {
        project::find(&cwd, project::home().as_deref())
    };
    let Reply::List { entries } = Connection::call(&Request::List {
        project: project.as_deref().map(project::wire_path),
    })?
    else {
        return Err(CliError::new("the daemon sent an unexpected reply to list"));
    };
    if json {
        let doc = serde_json::json!({ "entries": entries });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).map_err(|e| CliError::new(e.to_string()))?
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("No VMs yet. Start one with: cave run");
        return Ok(());
    }
    let width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4)
        + 2;
    let mut out = format!(
        "{:<width$}{:<10}{:<11}{:<8}CREATED\n",
        "NAME", "KIND", "STATE", "SIZE"
    );
    for e in &entries {
        let kind = match e.kind {
            EntryKind::Vm => "vm",
            EntryKind::Fork => "fork",
            EntryKind::Snapshot => "snapshot",
        };
        let state = match (e.state, e.auto) {
            (Some(VmState::Running), _) => "running",
            (Some(VmState::Suspended), _) => "suspended",
            (Some(VmState::Stopped), _) => "stopped",
            (None, true) => "auto",
            (None, false) => "-",
        };
        out.push_str(&format!(
            "{:<width$}{kind:<10}{state:<11}{:<8}{}\n",
            e.name,
            size(e.size),
            local_time(e.created_unix)
        ));
    }
    print!("{out}");
    Ok(())
}

/// `sha256:` and the first 8 hex digits.
fn short_digest(digest: &str) -> &str {
    let end = digest.len().min("sha256:".len() + 8);
    &digest[..end]
}

/// The `cave images` table.
fn images_table(images: &[ImageInfo]) -> String {
    let width = images
        .iter()
        .map(|i| i.name.len())
        .max()
        .unwrap_or(4)
        .max(4)
        + 2;
    let mut out = format!(
        "{:<width$}{:<17}{:<12}USED BY\n",
        "NAME", "DIGEST", "DOWNLOADED"
    );
    for image in images {
        let mut used = match image.used_by {
            0 => "-".to_string(),
            1 => "1 project".to_string(),
            n => format!("{n} projects"),
        };
        if image.base {
            used = format!("{used:<13}(base)");
        }
        if let Some(update) = &image.update {
            used = format!("{used:<13}(newer: {})", short_digest(update));
        }
        let downloaded = if image.downloaded { "yes" } else { "no" };
        out.push_str(&format!(
            "{:<width$}{:<17}{downloaded:<12}{}\n",
            image.name,
            short_digest(&image.digest),
            used.trim_end()
        ));
    }
    out
}

/// `cave images`.
pub fn images(json: bool) -> Result<()> {
    let Reply::Images { images } = Connection::call(&Request::Images)? else {
        return Err(CliError::new(
            "the daemon sent an unexpected reply to images",
        ));
    };
    if json {
        let doc = serde_json::json!({ "images": images });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).map_err(|e| CliError::new(e.to_string()))?
        );
        return Ok(());
    }
    print!("{}", images_table(&images));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str, digest: &str, downloaded: bool, used_by: u32) -> ImageInfo {
        ImageInfo {
            name: name.into(),
            digest: format!("sha256:{digest}"),
            downloaded,
            used_by,
            base: false,
            update: None,
        }
    }

    #[test]
    fn the_images_table_matches_the_docs() {
        let mut debian = image("debian", "3f2a9c1e00000000", true, 3);
        debian.base = true;
        let ubuntu = image("ubuntu", "8b17d04a00000000", false, 0);
        let mut old = image("debian", "11112222ffff", true, 1);
        old.update = Some("sha256:3f2a9c1e00000000".into());
        assert_eq!(
            images_table(&[debian, ubuntu, old]),
            "NAME    DIGEST           DOWNLOADED  USED BY\n\
             debian  sha256:3f2a9c1e  yes         3 projects   (base)\n\
             ubuntu  sha256:8b17d04a  no          -\n\
             debian  sha256:11112222  yes         1 project    (newer: sha256:3f2a9c1e)\n"
        );
    }
}
