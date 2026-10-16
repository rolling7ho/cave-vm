//! Sets the guest's timezone to the Mac's, sent with the clock.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use cave_vm_proto::agent::Timezone;

/// Points `root`/etc/localtime at `zone`: the guest's own zone file when
/// it has one, otherwise the Mac's file, and writes its name to
/// `root`/etc/timezone. Nothing is written when both already match.
pub fn set(root: &Path, zone: &Timezone) -> io::Result<()> {
    if !valid_name(&zone.name) || !zone.tzif.0.starts_with(b"TZif") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not a timezone: {}", zone.name),
        ));
    }
    let etc = root.join("etc");
    let localtime = etc.join("localtime");
    let own = root.join("usr/share/zoneinfo").join(&zone.name);
    let target = format!("/usr/share/zoneinfo/{}", zone.name);
    let current = fs::read_link(&localtime).ok();
    if own.is_file() {
        if current.as_deref() != Some(Path::new(&target)) {
            let temp = etc.join(".localtime.cave");
            let _ = fs::remove_file(&temp);
            symlink(&target, &temp)?;
            fs::rename(&temp, &localtime)?;
        }
    } else if current.is_some() || fs::read(&localtime).ok().as_ref() != Some(&zone.tzif.0) {
        let temp = etc.join(".localtime.cave");
        fs::write(&temp, &zone.tzif.0)?;
        fs::rename(&temp, &localtime)?;
    }
    let name = format!("{}\n", zone.name);
    if fs::read_to_string(etc.join("timezone")).ok().as_deref() != Some(name.as_str()) {
        fs::write(etc.join("timezone"), name)?;
    }
    Ok(())
}

/// A zone name such as `America/Argentina/Buenos_Aires` or `Etc/GMT+8`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cave_vm_proto::Data;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cave-tz-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc")).unwrap();
        dir
    }

    fn zone(name: &str) -> Timezone {
        Timezone {
            name: name.into(),
            tzif: Data(b"TZif2 mac".to_vec()),
        }
    }

    #[test]
    fn the_guests_own_zone_file_is_linked() {
        let root = temp("own");
        fs::create_dir_all(root.join("usr/share/zoneinfo/Asia")).unwrap();
        fs::write(root.join("usr/share/zoneinfo/Asia/Manila"), b"TZif2 guest").unwrap();
        fs::write(root.join("etc/localtime"), b"TZif2 utc").unwrap();
        set(&root, &zone("Asia/Manila")).unwrap();
        assert_eq!(
            fs::read_link(root.join("etc/localtime")).unwrap(),
            Path::new("/usr/share/zoneinfo/Asia/Manila")
        );
        assert_eq!(
            fs::read_to_string(root.join("etc/timezone")).unwrap(),
            "Asia/Manila\n"
        );
        set(&root, &zone("Asia/Manila")).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_zone_database_the_macs_file_is_used() {
        let root = temp("mac");
        symlink("/usr/share/zoneinfo/Etc/UTC", root.join("etc/localtime")).unwrap();
        set(&root, &zone("Europe/Paris")).unwrap();
        assert!(fs::read_link(root.join("etc/localtime")).is_err());
        assert_eq!(fs::read(root.join("etc/localtime")).unwrap(), b"TZif2 mac");
        assert_eq!(
            fs::read_to_string(root.join("etc/timezone")).unwrap(),
            "Europe/Paris\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bad_name_or_file_is_refused_and_nothing_written() {
        let root = temp("bad");
        assert!(set(&root, &zone("../../etc/passwd")).is_err());
        let mut not_tzif = zone("Asia/Manila");
        not_tzif.tzif = Data(b"#!/bin/sh".to_vec());
        assert!(set(&root, &not_tzif).is_err());
        assert!(!root.join("etc/localtime").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
