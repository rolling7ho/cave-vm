//! The daemon: supervises one VMM process per VM and owns policy, the audit
//! log and sync. The CLI starts it on demand (docs/cli.md).

#![deny(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use cave_vm_daemon::home::Home;
use cave_vm_daemon::image::Cache;
use cave_vm_daemon::image::manifest::Origin;
use cave_vm_daemon::launch::ProcessLauncher;
use cave_vm_daemon::project::ProjectSource;
use cave_vm_daemon::server;
use cave_vm_daemon::vms::Vms;

/// How often the daemon looks for VMs to suspend for being unused.
const IDLE_CHECK: Duration = Duration::from_secs(10);

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
    let images = home.root().join("images");
    std::fs::create_dir_all(&images)?;
    let cache = Cache::new(&images);
    cache.remove_partials()?;
    let source = ProjectSource {
        cache,
        origin: Origin::from_env(),
    };
    let mut launcher = ProcessLauncher::new(vmm);
    launcher.sandbox = Some(images);
    let vms = Arc::new(Vms::new(home.clone(), Arc::new(launcher), Arc::new(source)));
    vms.watch_idle(IDLE_CHECK);
    let sleeping = Arc::downgrade(&vms);
    let watched = cave_vm_vmm::host::on_system_sleep(move || {
        if let Some(vms) = sleeping.upgrade() {
            vms.suspend_all();
        }
    });
    if let Err(e) = watched {
        eprintln!("cave-daemon: VMs will not be suspended when the Mac sleeps: {e}");
    }
    let killed = vms.clean_up()?;
    if killed > 0 {
        eprintln!("cave-daemon: stopped {killed} VMM processes left by an earlier daemon");
    }
    let listener = server::bind(&home)?;
    server::serve(&listener, &vms)
}
