use std::os::unix::fs::FileExt;

use cave_vm_proto::daemon::NetMode;

use super::*;
use crate::snapshot::Manifest;

const MIB: u64 = 1 << 20;

#[test]
fn blocks_count_each_byte_once() {
    let mut b = Blocks::default();
    assert_eq!(b.add(100, 50), 50);
    assert_eq!(b.add(120, 10), 0);
    assert_eq!(b.add(140, 20), 10);
    assert_eq!(b.add(0, 100), 100);
    assert_eq!(b.add(300, 10), 10);
    // Spans the gap between two ranges.
    assert_eq!(b.add(150, 200), 180);
    assert_eq!(b.0.len(), 1);
    assert_eq!(b.add(0, 0), 0);
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cave-gc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `len` bytes of `byte` at `offset`, and flushes them so their
/// blocks are placed.
fn write(path: &Path, offset: u64, len: u64, byte: u8) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    let data = vec![byte; usize::try_from(len).unwrap()];
    file.write_all_at(&data, offset).unwrap();
    file.sync_all().unwrap();
}

fn clone(src: &Path, dst: &Path) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    host::clonefile(src, dst).unwrap();
}

fn manifest(dir: &Path, name: &str, created_unix: u64, auto: bool) {
    snapshot::write_manifest(
        dir,
        &Manifest {
            format: snapshot::FORMAT_VERSION,
            name: name.into(),
            created_unix,
            cave_version: "0.1.0".into(),
            host_os: "26A434".into(),
            image: "debian".into(),
            cpus: 2,
            memory_bytes: 2 << 30,
            net: NetMode::Nat,
            credentials: Vec::new(),
            has_memory: false,
            auto,
        },
    )
    .unwrap();
}

fn hex(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

/// An image cache with `debian` in use and `ubuntu` not, sharing one
/// kernel, and a project whose disk and snapshots are clones.
fn setup(root: &Path) -> (PathBuf, PathBuf) {
    let images = root.join("images");
    fs::create_dir_all(&images).unwrap();
    let image = |h: char, name: &str, size: u64| {
        let file = images.join(format!("sha256-{}.img", hex(h)));
        write(&file, 0, size, h as u8);
        fs::write(file.with_extension(KERNEL_RECORD), hex('k')).unwrap();
        fs::write(file.with_extension(NAME_RECORD), name).unwrap();
        file
    };
    let debian = image('a', "debian", 4 * MIB);
    image('b', "ubuntu", 2 * MIB);
    write(
        &images.join(format!("sha256-{}.img", hex('k'))),
        0,
        MIB,
        b'k',
    );

    let project = root.join("app");
    let state = project.join(STATE_DIR);
    fs::create_dir_all(&state).unwrap();
    fs::write(
        project.join(config::FILE),
        format!(
            "[vm]\nimage = \"debian@sha256:{}\"\n\n[snapshots]\nkeep = 1\n",
            hex('a')
        ),
    )
    .unwrap();
    // The VM wrote one MiB of its own over the image.
    let disk = state.join("disk.img");
    clone(&debian, &disk);
    write(&disk, 0, MIB, b'd');
    // A named snapshot, unchanged since; two automatic ones, each with a
    // MiB of its own.
    let named = snapshot::dir(&state, "named");
    clone(&disk, &named.join("disk.img"));
    manifest(&named, "named", 100, false);
    for (n, (name, created)) in [("auto-1", 200), ("auto-2", 300)].into_iter().enumerate() {
        let dir = snapshot::dir(&state, name);
        clone(&disk, &dir.join("disk.img"));
        write(
            &dir.join("disk.img"),
            MIB * (n as u64 + 1),
            MIB,
            b'0' + n as u8,
        );
        manifest(&dir, name, created, true);
    }
    // A fork with a MiB of its own.
    let fork = snapshot::fork_dir(&state, "fork-1");
    clone(&disk, &fork.join("disk.img"));
    write(&fork.join("disk.img"), 3 * MIB, MIB, b'f');
    manifest(&fork, "fork-1", 400, false);
    // A snapshot a crash left unfinished.
    let broken = snapshot::dir(&state, "broken");
    write(&broken.join("memory"), 0, MIB, b'm');
    (images, project)
}

#[test]
fn shared_blocks_are_counted_once_and_only_safe_things_are_offered() {
    let root = temp("survey");
    let (images, project) = setup(&root);
    let later = SystemTime::now() + UNFINISHED_AGE;
    let projects = [Project {
        path: project.clone(),
        busy: false,
    }];
    let usage = survey(&images, &projects, later).usage;

    assert_eq!(
        usage
            .images
            .iter()
            .map(|i| (i.name.as_str(), i.size, i.in_use))
            .collect::<Vec<_>>(),
        [("debian", 5 * MIB, true), ("ubuntu", 2 * MIB, false)]
    );
    // Manifests add a few hundred bytes to the MiBs.
    let about = |size: u64, mib: u64| size >= mib * MIB && size < mib * MIB + 64 * 1024;
    let p = &usage.projects[..];
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].project, project.to_string_lossy());
    assert!(
        about(p[0].disk, 1) && about(p[0].snapshots, 3) && about(p[0].forks, 1),
        "{p:?}"
    );
    let r = &usage.reclaimable;
    assert_eq!(
        r.iter().map(|r| r.what.as_str()).collect::<Vec<_>>(),
        [UNUSED_IMAGES, OLD_SNAPSHOTS, UNFINISHED]
    );
    assert!(
        about(r[0].size, 2) && about(r[1].size, 1) && about(r[2].size, 1),
        "{r:?}"
    );

    // A fresh unfinished snapshot may still be being written, and nothing
    // of a busy VM is offered.
    let now = survey(&images, &projects, SystemTime::now()).usage;
    assert!(!now.reclaimable.iter().any(|r| r.what == UNFINISHED));
    let busy = [Project {
        path: project.clone(),
        busy: true,
    }];
    let usage = survey(&images, &busy, later).usage;
    assert_eq!(usage.reclaimable.len(), 1, "{:?}", usage.reclaimable);

    // Applying deletes exactly what was offered.
    let usage = survey(&images, &projects, later).apply();
    assert!(about(usage.reclaimed.unwrap(), 4), "{:?}", usage.reclaimed);
    let state = project.join(STATE_DIR);
    assert_eq!(snapshot::names(&state).unwrap(), ["auto-2", "named"]);
    assert!(!snapshot::dir(&state, "broken").exists());
    assert!(state.join("disk.img").is_file());
    assert!(snapshot::fork_dir(&state, "fork-1").is_dir());
    let left: Vec<String> = fs::read_dir(&images)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with(&format!("sha256-{}", hex('a'))))
        .collect();
    assert_eq!(left, [format!("sha256-{}.img", hex('k'))]);
    let again = survey(&images, &projects, later).usage;
    assert!(again.reclaimable.is_empty(), "{:?}", again.reclaimable);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn list_sizes_count_shared_blocks_in_the_first_row() {
    let root = temp("sizes");
    let (images, project) = setup(&root);
    let state = project.join(STATE_DIR);
    let groups = vec![
        vm_paths(&state),
        vec![snapshot::dir(&state, "named")],
        vec![snapshot::dir(&state, "auto-1")],
        vec![snapshot::fork_dir(&state, "fork-1")],
    ];
    let sizes = sizes(&images, &groups);
    let kib = |b: u64| b / 1024;
    // The disk's own MiB; the named snapshot shares all of it; the others
    // each add their own MiB. Manifests add a few hundred bytes.
    assert_eq!(
        sizes.iter().map(|&s| kib(s)).collect::<Vec<_>>(),
        [1024, 0, 1024, 1024],
        "{sizes:?}"
    );
    fs::remove_dir_all(&root).unwrap();
}
