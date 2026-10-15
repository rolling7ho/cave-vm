//! `cave run`, `cave stop`, `cave ls` and `cave images`.

use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use cave_vm_proto::daemon::{
    EntryKind, ImageInfo, ListEntry, NetMode, Policy, PortMap, Reply, Request, Storage, VmId,
    VmState,
};

use crate::client::Connection;
use crate::error::{CliError, Result};
use crate::init;
use crate::memory;
use crate::policy;
use crate::project;
use crate::style::Paint;

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
/// Returns the policy.
pub fn ensure_running(
    vm: &VmId,
    net: Option<NetMode>,
    ports: Vec<PortMap>,
    always_print: bool,
) -> Result<Policy> {
    let was_running = is_running(vm)?;
    let first_boot = vm.fork.is_none() && !Path::new(&vm.project).join(BOOTED_FILE).exists();
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
            eprintln!(
                "{}",
                policy::render(&policy, "already running", Paint::stderr())
            );
        }
    } else {
        let headline = format!("started in {}", policy::seconds(started.elapsed()));
        eprintln!("{}", policy::render(&policy, &headline, Paint::stderr()));
        memory::warn_if_tight(policy.memory_bytes);
    }
    if first_boot && !was_running {
        eprintln!("{}", first_copy_line(policy.storage));
    }
    Ok(policy)
}

/// The daemon's record of a project VM's first boot, in the project
/// folder; the folder's files were copied in on that boot (D13).
const BOOTED_FILE: &str = ".cave/booted";

/// What the first boot did with the folder's files (D13).
fn first_copy_line(storage: Storage) -> &'static str {
    match storage {
        Storage::Sealed => {
            "Copied this folder's files into the VM, where the code lives now. The files on your Mac are left as they are and no longer updated"
        }
        Storage::Synced => {
            "Copied this folder's files into the VM. The VM's copy is the one to work on; cave sync updates this folder from it"
        }
    }
}

/// `cave run`. On a VM that is already running, in a terminal and with no
/// flags, it offers to start another VM as a fork of it (D40).
pub fn run(net: Option<NetMode>, ports: Vec<PortMap>, fork: Option<String>) -> Result<()> {
    let project = init::project_or_set_up()?;
    let vm = vm_id(&project, fork);
    let plain = net.is_none() && ports.is_empty() && vm.fork.is_none();
    if plain && std::io::stdin().is_terminal() && is_running(&vm)? {
        let question = format!(
            "{} is already running. Start another VM, a fork of it? [y/N] ",
            folder_name(&project)
        );
        if crate::remove::confirmed(&question)? {
            return crate::snapshot::fork(None, None);
        }
        return Ok(());
    }
    ensure_running(&vm, net, ports, true).map(drop)
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

/// `cave stop --force`: every running VM on the Mac, stopped at once
/// without saving, after asking (D43).
pub fn force_stop() -> Result<()> {
    let Reply::List { entries } = Connection::call(&Request::List { project: None })? else {
        return Err(CliError::new("the daemon sent an unexpected reply to list"));
    };
    let running: Vec<String> = entries
        .iter()
        .filter(|e| e.state == Some(VmState::Running))
        .map(|e| match e.kind {
            EntryKind::Fork => format!(
                "{} (fork of {})",
                e.name,
                folder_name(Path::new(&e.project))
            ),
            _ => e.name.clone(),
        })
        .collect();
    if running.is_empty() {
        println!("No VMs are running");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(
            CliError::new("cave stop --force needs a terminal to ask first")
                .fix("Run it in Terminal, or stop each VM with cave stop, which saves it"),
        );
    }
    if !crate::remove::confirmed(&force_question(&running))? {
        return Err(CliError::new("nothing was stopped"));
    }
    let Reply::StoppedAll { vms } = Connection::call(&Request::StopAll)? else {
        return Err(CliError::new(
            "the daemon sent an unexpected reply to stop_all",
        ));
    };
    let names: Vec<String> = vms
        .iter()
        .map(|vm| {
            vm.fork
                .clone()
                .unwrap_or_else(|| folder_name(Path::new(&vm.project)))
        })
        .collect();
    println!(
        "Stopped {} without saving: {}. The next start boots each from its disk",
        vms_count(names.len()),
        names.join(", ")
    );
    Ok(())
}

fn vms_count(n: usize) -> String {
    if n == 1 {
        "1 VM".into()
    } else {
        format!("{n} VMs")
    }
}

/// The question `cave stop --force` asks.
fn force_question(running: &[String]) -> String {
    format!(
        "Stop {} now without saving: {}?\nProcesses running in them are lost; their disks keep what was written. [y/N] ",
        vms_count(running.len()),
        running.join(", ")
    )
}

pub fn folder_name(path: &Path) -> String {
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
    let paint = Paint::stdout();
    // Headings underlined, then padded, so the columns stay aligned.
    let column = |name: &str, width: usize| {
        format!("{}{}", paint.heading(name), " ".repeat(width - name.len()))
    };
    let mut out = format!(
        "{}{}{}{}{}\n",
        column("NAME", width),
        column("KIND", 10),
        column("STATE", 11),
        column("SIZE", 8),
        paint.heading("CREATED")
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
        let state = format!("{state:<11}");
        let state = if e.state == Some(VmState::Running) {
            paint.good(&state)
        } else {
            state
        };
        out.push_str(&format!(
            "{:<width$}{kind:<10}{state}{:<8}{}\n",
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

    #[test]
    fn the_force_question_names_every_vm() {
        assert_eq!(
            force_question(&["try-a (fork of app)".into(), "app".into()]),
            "Stop 2 VMs now without saving: try-a (fork of app), app?\nProcesses running in them are lost; their disks keep what was written. [y/N] "
        );
        assert!(force_question(&["app".into()]).starts_with("Stop 1 VM now"));
    }

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
