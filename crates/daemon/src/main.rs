//! The daemon: supervises one VMM process per VM and owns policy, the audit
//! log and sync. The CLI starts it on demand (docs/cli.md).

#![deny(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use cave_vm_daemon::home::Home;
use cave_vm_daemon::launch::ProcessLauncher;
use cave_vm_daemon::server;
use cave_vm_daemon::vms::{Prepared, VmSource, Vms};
use cave_vm_proto::daemon::{NetMode, PortMap, VmId};
use cave_vm_proto::{ErrorCode, ErrorMessage};

/// Until reading `cave.toml` lands, starting a VM says so.
struct NotYet;

impl VmSource for NotYet {
    fn prepare(&self, _: &VmId, _: NetMode, _: &[PortMap]) -> Result<Prepared, ErrorMessage> {
        Err(ErrorMessage::new(
            ErrorCode::Internal,
            "this build of the daemon cannot read cave.toml yet",
        ))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cave-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> std::io::Result<()> {
    let home = Home::from_env()?;
    let exe = std::env::current_exe()?;
    let vmm = exe.with_file_name("cave-vmm");
    let vms = Arc::new(Vms::new(
        home.clone(),
        Arc::new(ProcessLauncher::new(vmm)),
        Arc::new(NotYet),
    ));
    let killed = vms.clean_up()?;
    if killed > 0 {
        eprintln!("cave-daemon: stopped {killed} VMM processes left by an earlier daemon");
    }
    let listener = server::bind(&home)?;
    server::serve(&listener, &vms)
}
