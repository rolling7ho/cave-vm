//! Hands the guest's boot-time file cache back once boot has settled.
//!
//! Booting reads well over 100 MB of files the VM mostly never reads again,
//! and the guest keeps them cached. That memory counts against the VM on
//! the Mac, which already caches the same disk blocks. Dropping the clean
//! page cache once lets free page reporting return it (P4.12d, decision
//! 0012). Later reads come from the virtio disk, served from the Mac's
//! cache.

use std::path::Path;
use std::time::Duration;

/// How long after the agent starts the cache is dropped: the agent starts
/// early in boot, and the rest of boot settles within about 3 seconds.
pub const BOOT_SETTLE: Duration = Duration::from_secs(3);

/// The kernel's knob for dropping caches.
pub const DROP_CACHES: &str = "/proc/sys/vm/drop_caches";

/// Drops the clean page cache through `knob` (`1`: page cache only, not
/// dentries and inodes).
pub fn drop_page_cache(knob: &Path) -> std::io::Result<()> {
    std::fs::write(knob, b"1")
}

/// Waits for boot to settle, then drops the page cache once, on a thread of
/// its own.
pub fn drop_boot_cache_later() {
    std::thread::spawn(|| {
        std::thread::sleep(BOOT_SETTLE);
        if let Err(e) = drop_page_cache(Path::new(DROP_CACHES)) {
            eprintln!("cave-agent: cannot drop the boot-time file cache: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_only_the_page_cache() {
        let knob = std::env::temp_dir().join(format!("cave-drop-caches-{}", std::process::id()));
        drop_page_cache(&knob).unwrap();
        assert_eq!(std::fs::read(&knob).unwrap(), b"1");
        std::fs::remove_file(&knob).unwrap();
    }
}
