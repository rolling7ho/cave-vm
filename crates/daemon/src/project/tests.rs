use std::fs;
use std::path::PathBuf;

use cave_vm_proto::ErrorCode;
use cave_vm_proto::daemon::{NetMode, PortMap, Storage, VmId};

use super::*;
use crate::image::manifest::MANIFEST_FILE;

fn sha(data: &[u8]) -> Digest {
    use sha2::{Digest as _, Sha256};
    Digest::from_bytes(Sha256::digest(data).into())
}

struct Setup {
    root: PathBuf,
    project: PathBuf,
    images: PathBuf,
    disk: Digest,
    source: ProjectSource,
}

const DISK: &[u8] = b"a tiny disk image";
const KERNEL: &[u8] = b"kernel";

fn setup(name: &str, toml: &str) -> Setup {
    let root = std::env::temp_dir().join(format!("cave-project-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (project, images) = (root.join("proj"), root.join("images"));
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&images).unwrap();
    fs::write(project.join(config::FILE), toml).unwrap();
    fs::write(images.join("debian.img"), DISK).unwrap();
    fs::write(images.join("Image"), KERNEL).unwrap();
    let disk = sha(DISK);
    fs::write(
        images.join(MANIFEST_FILE),
        format!(
            r#"{{"version": 1, "images": [{{"name": "debian",
                "disk": {{"url": "debian.img", "sha256": "{}", "size": {}}},
                "kernel": {{"url": "Image", "sha256": "{}", "size": {}}}}}]}}"#,
            disk.hex(),
            DISK.len(),
            sha(KERNEL).hex(),
            KERNEL.len()
        ),
    )
    .unwrap();
    let source = ProjectSource {
        cache: Cache::new(root.join("cache")),
        origin: Origin::Dir(images.clone()),
    };
    Setup {
        root,
        project,
        images,
        disk,
        source,
    }
}

fn vm(project: &Path) -> VmId {
    VmId {
        project: project.to_string_lossy().into_owned(),
        fork: None,
    }
}

const TOML: &str = "# my project\n[vm]\nimage = \"base\"  # pinned on first run\ncpus = 4\n\n[storage]\ndisk = \"1M\"\n";

#[test]
fn first_boot_fetches_pins_and_creates_the_disk() {
    let s = setup("first", TOML);
    let prepared = s.source.prepare(&vm(&s.project), None, &[]).unwrap();

    let pinned = format!("base@{}", s.disk);
    assert_eq!(prepared.policy.image, pinned);
    assert_eq!(prepared.policy.name, "proj");
    assert_eq!(prepared.policy.cpus, 4);
    assert_eq!(prepared.policy.net, NetMode::Nat);
    assert_eq!(prepared.policy.storage, Storage::Sealed);
    assert!(prepared.policy.widened.is_empty());

    let text = fs::read_to_string(s.project.join(config::FILE)).unwrap();
    assert!(text.starts_with("# my project\n"), "{text}");
    assert!(
        text.contains(&format!("image = \"{pinned}\"  # pinned on first run")),
        "{text}"
    );

    let disk = s.project.join(".cave").join(disk::DISK_FILE);
    assert_eq!(prepared.config.disk, disk.to_string_lossy());
    assert_eq!(fs::metadata(&disk).unwrap().len(), 1 << 20);
    assert_eq!(&fs::read(&disk).unwrap()[..DISK.len()], DISK);
    assert_eq!(fs::read(&prepared.config.kernel).unwrap(), KERNEL);
    assert_eq!(prepared.config.cpus, 4);
    fs::remove_dir_all(&s.root).unwrap();
}

#[test]
fn a_pinned_project_starts_without_the_manifest() {
    let s = setup("pinned", TOML);
    s.source.prepare(&vm(&s.project), None, &[]).unwrap();
    fs::remove_dir_all(&s.images).unwrap();

    let ports = [
        PortMap {
            mac: 3000,
            vm: 3000,
        },
        PortMap { mac: 8080, vm: 80 },
    ];
    let prepared = s
        .source
        .prepare(&vm(&s.project), Some(NetMode::Lan), &ports)
        .unwrap();
    assert_eq!(prepared.config.net, NetMode::Lan);
    assert_eq!(prepared.policy.ports, ports);
    assert_eq!(
        prepared.policy.widened,
        ["--net lan", "-p 3000", "-p 8080:80"]
    );
    fs::remove_dir_all(&s.root).unwrap();
}

#[test]
fn a_larger_disk_size_grows_the_disk() {
    let s = setup("grow", TOML);
    s.source.prepare(&vm(&s.project), None, &[]).unwrap();
    let toml = fs::read_to_string(s.project.join(config::FILE))
        .unwrap()
        .replace("\"1M\"", "\"2M\"");
    fs::write(s.project.join(config::FILE), toml).unwrap();
    let prepared = s.source.prepare(&vm(&s.project), None, &[]).unwrap();
    assert_eq!(fs::metadata(&prepared.config.disk).unwrap().len(), 2 << 20);
    fs::remove_dir_all(&s.root).unwrap();
}

#[test]
fn problems_say_how_to_fix_them() {
    let s = setup("errors", "[vm]\nimage = \"fedora\"\n");
    let err = s.source.prepare(&vm(&s.project), None, &[]).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains("no image called fedora"), "{err:?}");
    assert!(err.fix.is_some());

    fs::remove_file(s.project.join(config::FILE)).unwrap();
    let err = s.source.prepare(&vm(&s.project), None, &[]).unwrap_err();
    assert!(err.fix.unwrap().contains("cave init"));
    fs::remove_dir_all(&s.root).unwrap();
}

#[test]
fn images_count_the_projects_that_use_them() {
    let s = setup("images", TOML);
    let before = s.source.images(std::slice::from_ref(&s.project)).unwrap();
    assert_eq!(before.len(), 1);
    assert!(!before[0].downloaded);
    // Unpinned, it counts toward the newest version, which it will pin.
    assert_eq!(before[0].used_by, 1);
    assert!(before[0].base);

    s.source.prepare(&vm(&s.project), None, &[]).unwrap();
    let after = s.source.images(std::slice::from_ref(&s.project)).unwrap();
    assert_eq!(after[0].name, "debian");
    assert_eq!(after[0].digest, s.disk.to_string());
    assert!(after[0].downloaded);
    assert_eq!(after[0].used_by, 1);
    assert_eq!(after[0].update, None);
    fs::remove_dir_all(&s.root).unwrap();
}

#[test]
fn an_older_version_in_use_is_listed_with_its_update() {
    let hex = |n: u8| format!("{n:02x}").repeat(32);
    let manifest = Manifest::parse(&format!(
        r#"{{"version": 1, "images": [
            {{"name": "debian", "disk": {{"url": "d2", "sha256": "{d2}", "size": 1}}, "kernel": {{"url": "k", "sha256": "{k}", "size": 1}}}},
            {{"name": "ubuntu", "disk": {{"url": "u", "sha256": "{u}", "size": 1}}, "kernel": {{"url": "k", "sha256": "{k}", "size": 1}}}},
            {{"name": "debian", "disk": {{"url": "d1", "sha256": "{d1}", "size": 1}}, "kernel": {{"url": "k", "sha256": "{k}", "size": 1}}}},
            {{"name": "debian", "disk": {{"url": "d0", "sha256": "{d0}", "size": 1}}, "kernel": {{"url": "k", "sha256": "{k}", "size": 1}}}}
        ]}}"#,
        d2 = hex(0xd2),
        u = hex(0x01),
        d1 = hex(0xd1),
        d0 = hex(0xd0),
        k = hex(0xbb),
    ))
    .unwrap();
    let digest = |n: u8| -> Digest { format!("sha256:{}", hex(n)).parse().unwrap() };
    let rows = image_rows(&manifest, &[digest(0xd1), digest(0xd1)], |_| false);
    let summary: Vec<_> = rows
        .iter()
        .map(|r| (r.name.as_str(), r.used_by, r.base, r.update.is_some()))
        .collect();
    // d0 is neither used nor downloaded, so it is left out.
    assert_eq!(
        summary,
        [
            ("debian", 0, true, false),
            ("ubuntu", 0, false, false),
            ("debian", 2, false, true)
        ]
    );
    assert_eq!(
        rows[2].update.as_deref(),
        Some(digest(0xd2).to_string().as_str())
    );
}
