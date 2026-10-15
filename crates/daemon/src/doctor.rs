//! The daemon's part of `cave doctor` (docs/cli.md, "cave doctor"): every
//! file in the image cache is hashed again and checked against the digest
//! in its name. Nothing is changed and nothing is downloaded.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use cave_vm_proto::daemon::Check;
use sha2::{Digest as _, Sha256};

use crate::project::{KERNEL_RECORD, NAME_RECORD};

/// One line per cached image and kernel, or one saying there are none yet.
pub fn image_checks(cache: &Path) -> Vec<Check> {
    let files = match cached(cache) {
        Ok(files) => files,
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => {
            return vec![Check {
                name: format!("cannot read the image cache {}: {e}", cache.display()),
                ok: false,
                fix: Some("Check the folder's permissions".into()),
            }];
        }
    };
    if files.is_empty() {
        return vec![Check {
            name: "no images downloaded yet; the first cave run fetches one".into(),
            ok: true,
            fix: None,
        }];
    }
    // What each file is, from the records kept next to the images.
    let mut labels = BTreeMap::new();
    for hex in files.keys() {
        let record = |ext| fs::read_to_string(cache.join(format!("sha256-{hex}.{ext}")));
        let Ok(name) = record(NAME_RECORD) else {
            continue;
        };
        let name = name.trim().to_string();
        if let Ok(kernel) = record(KERNEL_RECORD) {
            labels.insert(kernel.trim().to_string(), format!("kernel for {name}"));
        }
        labels.insert(hex.clone(), format!("{name} image"));
    }
    files
        .iter()
        .map(|(hex, path)| {
            let label = labels
                .get(hex)
                .cloned()
                .unwrap_or_else(|| "cached file".into());
            let short = &hex[..8];
            match hash(path) {
                Ok(got) if got == *hex => Check {
                    name: format!("{label} sha256:{short} verified"),
                    ok: true,
                    fix: None,
                },
                Ok(_) => Check {
                    name: format!(
                        "{label} sha256:{short} is damaged: its contents do not match its digest"
                    ),
                    ok: false,
                    fix: Some(format!(
                        "Delete {}; the next cave run downloads it again",
                        path.display()
                    )),
                },
                Err(e) => Check {
                    name: format!("cannot read {label} sha256:{short}: {e}"),
                    ok: false,
                    fix: Some(format!(
                        "Delete {}; the next cave run downloads it again",
                        path.display()
                    )),
                },
            }
        })
        .collect()
}

/// One line for each cached image the update detector has seen a newer
/// version of, from its record in `updates`. Nothing is looked up here.
pub fn update_checks(cache: &Path, updates: &Path) -> Vec<Check> {
    let newest = crate::updates::newest(updates);
    let Ok(files) = cached(cache) else {
        return Vec::new();
    };
    let mut seen = BTreeMap::new();
    for hex in files.keys() {
        let record = cache.join(format!("sha256-{hex}.{NAME_RECORD}"));
        let Ok(name) = fs::read_to_string(record) else {
            continue;
        };
        let name = name.trim().to_string();
        // Nothing to say once the newest version is cached too.
        if let Some(latest) = newest.get(&name)
            && !latest
                .strip_prefix("sha256:")
                .is_some_and(|h| files.contains_key(h))
        {
            seen.entry(name).or_insert_with(|| latest.clone());
        }
    }
    seen.into_iter()
        .map(|(name, latest)| {
            let short = latest.get(..15).unwrap_or(&latest);
            Check {
                name: format!(
                    "a newer {name} image exists, {short}; cave never updates an image by itself, see docs/config.md, [updates]"
                ),
                ok: true,
                fix: None,
            }
        })
        .collect()
}

/// The cached files, `sha256-<hex>.img`, by digest.
fn cached(cache: &Path) -> io::Result<BTreeMap<String, std::path::PathBuf>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(hex) = name
            .strip_prefix("sha256-")
            .and_then(|n| n.strip_suffix(".img"))
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            files.insert(hex.to_string(), entry.path());
        }
    }
    Ok(files)
}

fn hash(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn cached_files_are_named_checked_and_never_changed() {
        let dir = std::env::temp_dir().join(format!("cave-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".partial")).unwrap();
        assert_eq!(
            image_checks(&dir),
            [Check {
                name: "no images downloaded yet; the first cave run fetches one".into(),
                ok: true,
                fix: None
            }]
        );

        let (disk, kernel, bad) = (hex(b"disk"), hex(b"kernel"), hex(b"bad"));
        fs::write(dir.join(format!("sha256-{disk}.img")), b"disk").unwrap();
        fs::write(dir.join(format!("sha256-{disk}.name")), "debian\n").unwrap();
        fs::write(dir.join(format!("sha256-{disk}.kernel")), &kernel).unwrap();
        fs::write(dir.join(format!("sha256-{kernel}.img")), b"kernel").unwrap();
        let damaged = dir.join(format!("sha256-{bad}.img"));
        fs::write(&damaged, b"changed").unwrap();

        let checks = image_checks(&dir);
        let line = |prefix: &str| {
            checks
                .iter()
                .find(|c| c.name.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix}: {checks:?}"))
                .clone()
        };
        let image = line("debian image");
        assert!(image.ok && image.name == format!("debian image sha256:{} verified", &disk[..8]));
        assert!(line("kernel for debian").ok);
        let broken = line("cached file");
        assert!(!broken.ok && broken.name.contains("is damaged"));
        assert!(broken.fix.unwrap().contains(&damaged.display().to_string()));
        // Report only.
        assert!(damaged.is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_image_the_detector_saw_is_reported_until_it_is_cached() {
        let dir = std::env::temp_dir().join(format!("cave-doctor-updates-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let record = dir.join("updates.json");
        let (old, new) = (hex(b"old"), hex(b"new"));
        fs::write(dir.join(format!("sha256-{old}.img")), b"old").unwrap();
        fs::write(dir.join(format!("sha256-{old}.name")), "debian\n").unwrap();
        // No record yet: nothing to say.
        assert!(update_checks(&dir, &record).is_empty());

        let doc =
            serde_json::json!({ "checked": 1, "newest": { "debian": format!("sha256:{new}") } });
        fs::write(&record, doc.to_string()).unwrap();
        let checks = update_checks(&dir, &record);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert!(checks[0].ok);
        assert!(
            checks[0].name.starts_with(&format!(
                "a newer debian image exists, sha256:{}",
                &new[..8]
            )),
            "{}",
            checks[0].name
        );

        fs::write(dir.join(format!("sha256-{new}.img")), b"new").unwrap();
        fs::write(dir.join(format!("sha256-{new}.name")), "debian\n").unwrap();
        assert!(update_checks(&dir, &record).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
