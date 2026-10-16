use super::*;

fn hashes(pairs: &[(&str, &str)]) -> Hashes {
    pairs
        .iter()
        .map(|(p, h)| (p.to_string(), h.to_string()))
        .collect()
}

fn names(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
}

#[test]
fn a_file_changed_only_on_the_mac_is_pulled() {
    let last = hashes(&[("a", "1"), ("gone", "1")]);
    let mac = hashes(&[("a", "2"), ("new", "1")]);
    let vm = hashes(&[("a", "1"), ("gone", "1")]);
    let pull = plan_pull(&last, &mac, &vm, false).unwrap();
    assert_eq!(names(&pull.write), ["a", "new"]);
    assert_eq!(names(&pull.delete), ["gone"]);
}

#[test]
fn a_file_changed_only_in_the_vm_is_left_alone_by_a_pull() {
    let last = hashes(&[("a", "1"), ("b", "1")]);
    let mac = hashes(&[("a", "1"), ("b", "1")]);
    let vm = hashes(&[("a", "2"), ("c", "1")]);
    assert_eq!(plan_pull(&last, &mac, &vm, false).unwrap(), Pull::default());
}

#[test]
fn a_file_changed_on_both_sides_stops_the_pull_and_is_listed() {
    let last = hashes(&[("app.ts", "1"), ("package.json", "1"), ("ok", "1")]);
    let mac = hashes(&[("app.ts", "2"), ("package.json", "2"), ("ok", "2")]);
    let vm = hashes(&[("app.ts", "3"), ("ok", "1")]);
    assert_eq!(
        plan_pull(&last, &mac, &vm, false).unwrap_err(),
        ["app.ts", "package.json"]
    );
    // Both sides made the same change: nothing to do, and no conflict.
    let vm = hashes(&[("app.ts", "2"), ("package.json", "2"), ("ok", "1")]);
    assert_eq!(
        names(&plan_pull(&last, &mac, &vm, false).unwrap().write),
        ["ok"]
    );
}

#[test]
fn force_applies_the_macs_version_of_files_changed_on_both_sides() {
    let last = hashes(&[("app.ts", "1"), ("gone", "1"), ("vm-only", "1")]);
    let mac = hashes(&[("app.ts", "2"), ("vm-only", "1")]);
    let vm = hashes(&[("app.ts", "3"), ("gone", "3"), ("vm-only", "2")]);
    let pull = plan_pull(&last, &mac, &vm, true).unwrap();
    assert_eq!(names(&pull.write), ["app.ts"]);
    assert_eq!(names(&pull.delete), ["gone"]);
}

#[test]
fn a_sync_writes_vm_changes_and_never_writes_over_mac_edits() {
    let last = hashes(&[("a", "1"), ("b", "1"), ("c", "1"), ("d", "1")]);
    let mac = hashes(&[("a", "1"), ("b", "2"), ("c", "2"), ("d", "1")]);
    let vm = hashes(&[("a", "2"), ("b", "1"), ("c", "3"), ("e", "1")]);
    let sync = plan_sync(&last, &mac, &vm);
    assert_eq!(names(&sync.write), ["a", "e"]);
    assert_eq!(names(&sync.delete), ["d"]);
    assert_eq!(names(&sync.conflicts), ["c"]);
}

#[test]
fn the_first_sync_has_no_last_state() {
    let last = Hashes::new();
    let mac = hashes(&[("same", "1"), ("differ", "1"), ("mac-only", "1")]);
    let vm = hashes(&[("same", "1"), ("differ", "2"), ("vm-only", "1")]);
    let sync = plan_sync(&last, &mac, &vm);
    assert_eq!(names(&sync.write), ["vm-only"]);
    assert_eq!(names(&sync.conflicts), ["differ"]);
    assert_eq!(plan_pull(&last, &mac, &vm, false).unwrap_err(), ["differ"]);
}

#[test]
fn the_last_state_keeps_what_both_sides_agree_on() {
    let last = hashes(&[("pending", "1"), ("gone", "1")]);
    let mac = hashes(&[("same", "5"), ("pending", "2")]);
    let vm = hashes(&[("same", "5"), ("pending", "1")]);
    assert_eq!(
        agreed(&last, &mac, &vm),
        hashes(&[("same", "5"), ("pending", "1")])
    );
}

#[test]
fn dependency_build_and_private_folders_are_always_excluded() {
    let x = Exclusions::new("");
    for path in [
        "node_modules/x/index.js",
        "web/node_modules",
        "target/debug/app",
        ".git/HEAD",
        ".cave/disk.img",
        ".env",
    ] {
        assert!(x.excluded(path, false), "{path}");
    }
    for path in ["src/main.rs", "web/.env", ".env.example", "targets.txt"] {
        assert!(!x.excluded(path, false), "{path}");
    }
}

#[test]
fn gitignore_patterns_follow_gits_rules() {
    let x = Exclusions::new(
        "# build output\n\
         dist/\n\
         *.log\n\
         !keep.log\n\
         /root-only.txt\n\
         docs/**/*.tmp\n\
         cache?\n\
         [Bb]uild\n\
         \\#hash\n",
    );
    let cases = [
        ("dist/app.js", true),
        ("web/dist/app.js", true),
        ("dist", false), // a file called dist: `dist/` is folders only
        ("err.log", true),
        ("sub/err.log", true),
        ("keep.log", false),
        ("root-only.txt", true),
        ("sub/root-only.txt", false),
        ("docs/a/b/x.tmp", true),
        ("docs/x.tmp", true),
        ("x.tmp", false),
        ("cache1", true),
        ("cache12", false),
        ("Build/out", true),
        ("build", true),
        ("#hash", true),
        ("src/lib.rs", false),
    ];
    for (path, want) in cases {
        assert_eq!(x.excluded(path, false), want, "{path}");
    }
    assert!(x.excluded("dist", true));
}

#[test]
fn a_file_in_an_excluded_folder_cannot_be_re_included() {
    let x = Exclusions::new("logs/\n!logs/keep.txt\n");
    assert!(x.excluded("logs/keep.txt", false));
}
