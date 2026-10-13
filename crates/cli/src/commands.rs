//! `cave run`, `cave stop` and `cave ls`.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use cave_vm_proto::daemon::{
    EntryKind, ListEntry, NetMode, PortMap, Reply, Request, VmId, VmState,
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
    Connection::call(&Request::Stop { vm })?;
    println!("Stopped {name}");
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
