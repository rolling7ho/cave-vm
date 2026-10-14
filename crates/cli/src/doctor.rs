//! `cave doctor` (docs/cli.md, "cave doctor"): the first-run checks,
//! again. The Mac's own checks run here, so they work even when the daemon
//! cannot start; the daemon adds the image cache. Report only: nothing is
//! changed.

use std::path::Path;
use std::process::Command;

use cave_vm_proto::daemon::{Check, Reply, Request};

use crate::client::{self, Connection};
use crate::error::{EXIT_FAILED, Result};

/// The oldest macOS cave supports.
const MIN_MACOS: u32 = 26;

/// Less free space than this on the projects' volume fails the check.
const MIN_FREE_BYTES: u64 = 5 << 30;

/// `cave doctor`. Returns the exit code: 1 if any check fails.
pub fn run() -> Result<u8> {
    let mut checks = vec![chip(), hypervisor(), macos(), signature(), disk_space()];
    match Connection::call(&Request::Doctor) {
        Ok(Reply::Checks { checks: daemon }) => {
            checks.push(pass("the cave daemon is running"));
            checks.extend(daemon);
        }
        Ok(_) => checks.push(fail(
            "the cave daemon sent an unexpected reply to doctor",
            "Update cave, so the command and the daemon match",
        )),
        Err(e) => checks.push(Check {
            name: format!("the cave daemon did not answer: {}", e.message),
            ok: false,
            fix: e.fix,
        }),
    }
    println!("{}", render(&checks));
    // The failures are printed above, each with its fix.
    Ok(if checks.iter().all(|c| c.ok) {
        0
    } else {
        EXIT_FAILED
    })
}

/// The lines `cave doctor` prints.
fn render(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| match (&c.fix, c.ok) {
            (_, true) => format!("ok    {}", c.name),
            (Some(fix), false) => format!("fail  {}\n      fix: {fix}", c.name),
            (None, false) => format!("fail  {}", c.name),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pass(name: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: true,
        fix: None,
    }
}

fn fail(name: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: false,
        fix: Some(fix.into()),
    }
}

/// A program's trimmed stdout, if it ran and succeeded.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn sysctl(name: &str) -> Option<String> {
    output("/usr/sbin/sysctl", &["-n", name])
}

pub fn chip() -> Check {
    chip_check(sysctl("machdep.cpu.brand_string").as_deref())
}

fn chip_check(brand: Option<&str>) -> Check {
    match brand.and_then(|b| b.strip_prefix("Apple ")) {
        Some(model) => pass(format!("Apple Silicon ({model})")),
        None => fail(
            format!("this Mac's chip is {}", brand.unwrap_or("unknown")),
            "cave needs a Mac with Apple Silicon (M1 or later)",
        ),
    }
}

pub fn hypervisor() -> Check {
    if sysctl("kern.hv_support").as_deref() == Some("1") {
        pass("Hypervisor.framework is available")
    } else {
        fail(
            "this Mac does not offer Hypervisor.framework",
            "Run cave directly on an Apple Silicon Mac, not inside a virtual machine",
        )
    }
}

pub fn macos() -> Check {
    macos_check(output("/usr/bin/sw_vers", &["-productVersion"]).as_deref())
}

fn macos_check(version: Option<&str>) -> Check {
    let major = version.and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    match (version, major) {
        (Some(v), Some(major)) if major >= MIN_MACOS => pass(format!("macOS {v}")),
        _ => fail(
            format!(
                "macOS {} is too old",
                version.unwrap_or("of unknown version")
            ),
            format!(
                "Update to macOS {MIN_MACOS} or later in System Settings > General > Software Update"
            ),
        ),
    }
}

/// `cave-vmm`, next to the daemon, carries the hypervisor entitlement.
fn signature() -> Check {
    let vmm = match client::daemon_program() {
        Ok(daemon) => daemon.with_file_name("cave-vmm"),
        Err(e) => {
            return fail(
                format!("cannot find cave-vmm: {}", e.message),
                "Reinstall cave",
            );
        }
    };
    signature_check(&vmm, entitlements(&vmm).as_deref())
}

pub fn entitlements(vmm: &Path) -> Option<String> {
    let path = vmm.to_str()?;
    output(
        "/usr/bin/codesign",
        &["-d", "--entitlements", "-", "--xml", path],
    )
}

fn signature_check(vmm: &Path, entitlements: Option<&str>) -> Check {
    if !vmm.is_file() {
        return fail(
            format!(
                "cave-vmm is missing from {}",
                vmm.parent().unwrap_or(vmm).display()
            ),
            "Reinstall cave (brew reinstall cave-vm)",
        );
    }
    if entitlements.is_some_and(|e| e.contains("com.apple.security.hypervisor")) {
        pass("cave is signed with the hypervisor entitlement")
    } else {
        fail(
            "cave is not signed with the hypervisor entitlement",
            "Reinstall cave (brew reinstall cave-vm); cave signs cave-vmm when it next starts its daemon",
        )
    }
}

pub fn disk_space() -> Check {
    let free = output("/bin/df", &["-k", "."]).and_then(|text| free_bytes(&text));
    disk_check(free)
}

/// The available bytes from `df -k` output for one path.
fn free_bytes(df: &str) -> Option<u64> {
    let line = df.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib * 1024)
}

fn disk_check(free: Option<u64>) -> Check {
    let gb = |bytes: u64| bytes / 1_000_000_000;
    match free {
        Some(bytes) if bytes >= MIN_FREE_BYTES => pass(format!(
            "{} GB free on the volume with your projects",
            gb(bytes)
        )),
        Some(bytes) => fail(
            format!(
                "only {} GB free on the volume with your projects; cave needs {} GB",
                gb(bytes),
                gb(MIN_FREE_BYTES)
            ),
            "Free some space; cave gc shows what cave can reclaim",
        ),
        None => fail(
            "cannot tell how much space is free on this volume",
            "Check that df works in this folder",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_say_ok_or_fail_with_the_fix() {
        let checks = [
            pass("Apple Silicon (M3 Pro)"),
            fail(
                "cave is not signed with the hypervisor entitlement",
                "Reinstall cave (brew reinstall cave-vm); cave signs cave-vmm when it next starts its daemon",
            ),
        ];
        assert_eq!(
            render(&checks),
            "ok    Apple Silicon (M3 Pro)\n\
             fail  cave is not signed with the hypervisor entitlement\n      \
             fix: Reinstall cave (brew reinstall cave-vm); cave signs cave-vmm when it next starts its daemon"
        );
    }

    #[test]
    fn each_check_fails_with_a_fix() {
        assert_eq!(
            chip_check(Some("Apple M3 Pro")).name,
            "Apple Silicon (M3 Pro)"
        );
        assert!(!chip_check(Some("Intel(R) Core(TM) i9")).ok);
        assert!(!chip_check(None).ok);

        assert_eq!(macos_check(Some("26.1")).name, "macOS 26.1");
        assert!(macos_check(Some("27.0")).ok);
        let old = macos_check(Some("15.4"));
        assert!(!old.ok && old.fix.unwrap().contains("macOS 26 or later"));
        assert!(!macos_check(None).ok);

        let df = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n\
                  /dev/disk3s5 971350180 400000000 46875000 90% 1 2 0% /System/Volumes/Data\n";
        let free = free_bytes(df).unwrap();
        assert_eq!(free, 46_875_000 * 1024);
        assert_eq!(
            disk_check(Some(free)).name,
            "48 GB free on the volume with your projects"
        );
        assert!(!disk_check(Some(1 << 30)).ok);
        assert!(!disk_check(None).ok);

        let exe = std::env::current_exe().unwrap();
        let entitled =
            "<plist><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>";
        assert!(signature_check(&exe, Some(entitled)).ok);
        let unsigned = signature_check(&exe, None);
        assert!(!unsigned.ok && unsigned.fix.is_some());
        let missing = signature_check(Path::new("/nonexistent/cave-vmm"), Some(entitled));
        assert!(!missing.ok && missing.name.starts_with("cave-vmm is missing"));
    }
}
