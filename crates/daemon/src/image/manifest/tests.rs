use std::fs;

use super::*;
use crate::image::Cache;

fn hex(n: u8) -> String {
    format!("{:064x}", n)
}

fn sample() -> String {
    format!(
        r#"{{
  "version": 1,
  "future": "ignored",
  "images": [
    {{ "name": "debian", "description": "Debian 13 (trixie)",
       "disk":   {{ "url": "debian-2.img", "sha256": "{d2}", "size": 20 }},
       "kernel": {{ "url": "Image-2",      "sha256": "{k2}", "size": 10 }} }},
    {{ "name": "ubuntu",
       "disk":   {{ "url": "https://example.invalid/ubuntu.img", "sha256": "{u1}", "size": 30 }},
       "kernel": {{ "url": "https://example.invalid/Image", "sha256": "{k2}", "size": 10 }} }},
    {{ "name": "debian", "description": "Debian 13 (trixie)",
       "disk":   {{ "url": "debian-1.img", "sha256": "{d1}", "size": 20 }},
       "kernel": {{ "url": "Image-1",      "sha256": "{k1}", "size": 10 }} }}
  ]
}}"#,
        d2 = hex(0xd2),
        k2 = hex(0xb2),
        u1 = hex(0x01),
        d1 = hex(0xd1),
        k1 = hex(0xb1),
    )
}

fn sha(data: &[u8]) -> Digest {
    use sha2::{Digest as _, Sha256};
    Digest::from_bytes(Sha256::digest(data).into())
}

fn digest(n: u8) -> Digest {
    format!("sha256:{}", hex(n)).parse().unwrap()
}

#[test]
fn a_name_resolves_to_its_newest_version() {
    let m = Manifest::parse(&sample()).unwrap();
    let base: ImageRef = "base".parse().unwrap();
    let e = m.resolve(&base).unwrap();
    assert_eq!(e.disk.digest, digest(0xd2));
    assert_eq!(e.kernel.digest, digest(0xb2));
    assert_eq!(e.description, "Debian 13 (trixie)");
    let ubuntu = m.resolve(&"ubuntu".parse().unwrap()).unwrap();
    assert_eq!(ubuntu.disk.size, 30);
}

#[test]
fn a_pin_finds_its_own_version_and_kernel() {
    let m = Manifest::parse(&sample()).unwrap();
    let pinned: ImageRef = format!("base@sha256:{}", hex(0xd1)).parse().unwrap();
    let e = m.resolve(&pinned).unwrap();
    assert_eq!(e.kernel.digest, digest(0xb1));

    let gone: ImageRef = format!("base@sha256:{}", hex(0x99)).parse().unwrap();
    let err = m.resolve(&gone).unwrap_err();
    assert_eq!(err, ManifestError::UnknownDigest(gone));
    assert!(err.to_string().contains("set image = \"base\""), "{err}");

    let err = m.resolve(&"fedora".parse().unwrap()).unwrap_err();
    assert!(err.to_string().contains("no image called fedora"), "{err}");
}

#[test]
fn newest_lists_each_name_once() {
    let m = Manifest::parse(&sample()).unwrap();
    let names: Vec<_> = m.newest().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["debian", "ubuntu"]);
}

#[test]
fn malformed_manifests_say_what_is_wrong() {
    for (text, why) in [
        ("not json", "expected"),
        (r#"{"images": []}"#, "no version"),
        (r#"{"version": 2, "images": []}"#, "newer than this cave"),
        (r#"{"version": 1}"#, "no images list"),
        (
            r#"{"version": 1, "images": [{"name": "debian"}]}"#,
            "debian) disk is not an object",
        ),
        (
            r#"{"version": 1, "images": [{"name": "x", "disk": {"url": "a", "sha256": "zz", "size": 1}, "kernel": {}}]}"#,
            "bad sha256",
        ),
    ] {
        let err = Manifest::parse(text).unwrap_err();
        assert!(err.to_string().contains(why), "{text}: {err}");
    }
}

#[test]
fn a_local_folder_serves_the_manifest_and_its_files_into_the_cache() {
    let dir = std::env::temp_dir().join(format!("cave-manifest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("cache")).unwrap();
    let disk = b"a tiny disk image".to_vec();
    let disk_digest = sha(&disk);
    let kernel = b"kernel".to_vec();
    let kernel_digest = sha(&kernel);
    fs::write(dir.join("debian.img"), &disk).unwrap();
    fs::write(dir.join("Image"), &kernel).unwrap();
    fs::write(
        dir.join(MANIFEST_FILE),
        format!(
            r#"{{"version": 1, "images": [{{"name": "debian",
                "disk": {{"url": "debian.img", "sha256": "{}", "size": {}}},
                "kernel": {{"url": "Image", "sha256": "{}", "size": {}}}}}]}}"#,
            disk_digest.hex(),
            disk.len(),
            kernel_digest.hex(),
            kernel.len()
        ),
    )
    .unwrap();

    let origin = Origin::Dir(dir.clone());
    let manifest = Manifest::parse(&origin.manifest().unwrap()).unwrap();
    let entry = manifest.resolve(&"base".parse().unwrap()).unwrap();
    let source = origin.source(&manifest);
    let cache = Cache::new(dir.join("cache"));
    let got = cache
        .fetch(&entry.disk.digest, &source, &mut |_, _| {})
        .unwrap();
    assert_eq!(fs::read(got).unwrap(), disk);
    let got = cache
        .fetch(&entry.kernel.digest, &source, &mut |_, _| {})
        .unwrap();
    assert_eq!(fs::read(got).unwrap(), kernel);

    // A local manifest cannot point outside its folder.
    assert!(origin.open("../secret").is_err());
    assert!(origin.open("http://example.invalid/x").is_err());
    let _ = fs::remove_dir_all(&dir);
}
