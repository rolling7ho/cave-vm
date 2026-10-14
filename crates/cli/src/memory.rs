//! The warning when the Mac's memory gets tight (design, "Defaults": no
//! hard cap on the number of VMs, with a warning when memory gets tight;
//! P6.1). It is printed after a VM boots, and never stops one from booting.
//!
//! Memory is tight when macOS reports memory pressure, or when less memory
//! is available than the memory cap of the VM that just booted, so it could
//! not grow to its cap without the Mac swapping.

use cave_vm_proto::daemon::{Reply, Request, VmState};

use crate::client::Connection;
use crate::doctor::sysctl;

const GB: f64 = (1u64 << 30) as f64;

/// What macOS says about its memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// Physical memory, in bytes.
    pub total: u64,
    /// How much of it is available, in percent (`kern.memorystatus_level`).
    pub available_percent: u64,
    /// `kern.memorystatus_vm_pressure_level`: 1 is normal, 2 a warning and
    /// 4 critical.
    pub pressure: u64,
}

impl Host {
    /// Reads the Mac's memory figures, or `None` if `sysctl` cannot.
    pub fn read() -> Option<Host> {
        let number = |name| sysctl(name)?.parse().ok();
        Some(Host {
            total: number("hw.memsize")?,
            available_percent: number("kern.memorystatus_level")?,
            pressure: number("kern.memorystatus_vm_pressure_level")?,
        })
    }

    fn available(&self) -> u64 {
        self.total / 100 * self.available_percent.min(100)
    }
}

/// The warning's text, if memory is tight with `running` VMs up and a VM
/// with `vm_memory` bytes just booted.
pub fn warning(running: usize, host: &Host, vm_memory: u64) -> Option<String> {
    if host.pressure < 2 && host.available() >= vm_memory {
        return None;
    }
    let vms = if running == 1 { "VM is" } else { "VMs are" };
    Some(format!(
        "memory is getting tight: {running} {vms} running and {:.1} GB is free",
        host.available() as f64 / GB
    ))
}

/// Prints the warning if memory is tight after a VM with `vm_memory` bytes
/// booted. Best effort: a figure that cannot be read means no warning.
pub fn warn_if_tight(vm_memory: u64) {
    let Some(host) = Host::read() else {
        return;
    };
    let Ok(Reply::List { entries }) = Connection::call(&Request::List { project: None }) else {
        return;
    };
    let running = entries
        .iter()
        .filter(|e| e.state == Some(VmState::Running))
        .count();
    if let Some(text) = warning(running, &host, vm_memory) {
        eprintln!("warning: {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_GB: u64 = 2 << 30;

    fn host(total_gb: u64, available_percent: u64, pressure: u64) -> Host {
        Host {
            total: total_gb << 30,
            available_percent,
            pressure,
        }
    }

    #[test]
    fn plenty_of_memory_says_nothing() {
        assert_eq!(warning(3, &host(24, 60, 1), TWO_GB), None);
    }

    #[test]
    fn less_free_than_the_vms_cap_is_tight() {
        assert_eq!(
            warning(4, &host(16, 10, 1), TWO_GB).as_deref(),
            Some("memory is getting tight: 4 VMs are running and 1.6 GB is free")
        );
        assert_eq!(
            warning(1, &host(8, 20, 1), TWO_GB).as_deref(),
            Some("memory is getting tight: 1 VM is running and 1.6 GB is free")
        );
    }

    #[test]
    fn memory_pressure_is_tight_whatever_is_free() {
        assert!(warning(2, &host(24, 40, 2), TWO_GB).is_some());
        assert!(warning(2, &host(24, 40, 4), TWO_GB).is_some());
    }

    #[test]
    fn the_macs_figures_read() {
        // On any Mac the three sysctl values exist.
        if cfg!(target_os = "macos") {
            let host = Host::read().expect("sysctl");
            assert!(host.total > 0 && host.available_percent <= 100 && host.pressure >= 1);
        }
    }
}
