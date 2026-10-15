//! First-run setup (design, principle 2: "First run does everything";
//! P6.3), done by the command line before it starts the daemon:
//!
//! - On the first run, when the data folder does not exist yet, the Mac is
//!   checked as `cave doctor` checks it, and a failed check stops here with
//!   its fix.
//! - The data folder is created.
//! - `cave-vmm` is signed ad hoc with the hypervisor entitlement when it
//!   does not carry it, on this Mac, with `codesign -s -` and no keys or
//!   certificates (A5, checked in docs/spike/p2.8-signing.md).
//!
//! The daemon then fetches the kernel and the base image on the first
//! `start`, with progress, and the first boot runs from there.

use std::fs;
use std::path::Path;
use std::process::Command;

use cave_vm_proto::daemon::Check;

use crate::doctor;
use crate::error::{CliError, Result};
use crate::style::Paint;

/// The entitlement `cave-vmm` needs to create VMs.
const ENTITLEMENTS: &str = include_str!("../../../scripts/hypervisor.entitlements");

/// Where the entitlements file is written, in the data folder, for
/// `codesign` to read.
const ENTITLEMENTS_FILE: &str = "hypervisor.entitlements";

const HYPERVISOR_ENTITLEMENT: &str = "com.apple.security.hypervisor";

/// Sets up what the daemon in `daemon` needs before it starts, with its
/// data in `dir`.
pub fn before_daemon(dir: &Path, daemon: &Path) -> Result<()> {
    let first_run = !dir.exists();
    if first_run {
        stop_on_failure(&[doctor::chip(), doctor::hypervisor(), doctor::macos()])?;
    }
    fs::create_dir_all(dir)?;
    // Without the daemon, starting it fails with an error that names it.
    if daemon.is_file() && sign_if_needed(dir, &daemon.with_file_name("cave-vmm"))? {
        eprintln!("Signed cave-vmm for Hypervisor.framework (ad hoc, on this Mac)");
    }
    if first_run {
        // Little free space does not stop the first run; the download says
        // so if it runs out.
        let disk = doctor::disk_space();
        if !disk.ok {
            eprintln!("{} {}", Paint::stderr().warning("warning:"), disk.name);
        }
    }
    Ok(())
}

/// The first failed check as an error, with its fix.
fn stop_on_failure(checks: &[Check]) -> Result<()> {
    match checks.iter().find(|c| !c.ok) {
        None => Ok(()),
        Some(check) => {
            let error = CliError::new(check.name.clone());
            Err(match &check.fix {
                Some(fix) => error.fix(fix.clone()),
                None => error,
            })
        }
    }
}

/// Whether `vmm` carries the hypervisor entitlement.
fn entitled(vmm: &Path) -> bool {
    doctor::entitlements(vmm).is_some_and(|e| e.contains(HYPERVISOR_ENTITLEMENT))
}

/// Signs `vmm` ad hoc with the hypervisor entitlement unless it already
/// carries it. Returns whether it signed.
fn sign_if_needed(dir: &Path, vmm: &Path) -> Result<bool> {
    if !vmm.is_file() {
        return Err(CliError::new(format!(
            "cave-vmm is missing from {}",
            vmm.parent().unwrap_or(vmm).display()
        ))
        .fix("Reinstall cave (brew reinstall cave-vm)"));
    }
    if entitled(vmm) {
        return Ok(false);
    }
    let plist = dir.join(ENTITLEMENTS_FILE);
    fs::write(&plist, ENTITLEMENTS)?;
    let out = Command::new("/usr/bin/codesign")
        .args(["--sign", "-", "--force", "--entitlements"])
        .arg(&plist)
        .arg(vmm)
        .output()?;
    let folder = vmm.parent().unwrap_or(vmm).display();
    if !out.status.success() || !entitled(vmm) {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(CliError::new(format!(
            "cannot sign cave-vmm for Hypervisor.framework: {}",
            why.trim()
        ))
        .fix(format!(
            "Check that you can write to {folder}, or reinstall cave (brew reinstall cave-vm)"
        )));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_check_stops_with_its_fix() {
        let ok = Check {
            name: "macOS 26.1".into(),
            ok: true,
            fix: None,
        };
        assert!(stop_on_failure(std::slice::from_ref(&ok)).is_ok());
        let bad = Check {
            name: "macOS 15.2 is too old".into(),
            ok: false,
            fix: Some("Update to macOS 26 or later".into()),
        };
        let e = stop_on_failure(&[ok, bad]).unwrap_err();
        assert_eq!(e.message, "macOS 15.2 is too old");
        assert_eq!(e.fix.as_deref(), Some("Update to macOS 26 or later"));
    }

    #[test]
    fn the_entitlements_file_grants_the_hypervisor() {
        assert!(ENTITLEMENTS.contains(&format!("<key>{HYPERVISOR_ENTITLEMENT}</key>")));
    }

    /// Signs a copy of a system binary, so it needs only `codesign`.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unsigned_vmm_is_signed_once_and_a_missing_one_says_how_to_fix_it() {
        let dir = std::env::temp_dir().join(format!("cave-setup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let vmm = dir.join("cave-vmm");
        fs::copy("/bin/echo", &vmm).unwrap();
        assert!(!entitled(&vmm));

        assert!(sign_if_needed(&dir, &vmm).unwrap());
        assert!(entitled(&vmm));
        assert!(!sign_if_needed(&dir, &vmm).unwrap(), "already signed");

        let missing = sign_if_needed(&dir, &dir.join("nothing")).unwrap_err();
        assert!(missing.message.starts_with("cave-vmm is missing"));
        let _ = fs::remove_dir_all(&dir);
    }
}
