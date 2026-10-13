use super::*;

fn sample() -> Manifest {
    Manifest {
        format: FORMAT_VERSION,
        name: "app-2026-10-13-debian".into(),
        created_unix: 1_790_000_000,
        cave_version: "0.1.0".into(),
        host_os: "26A434".into(),
        image: "base@sha256:abcd".into(),
        cpus: 2,
        memory_bytes: 2 << 30,
        net: NetMode::Nat,
        credentials: vec!["API_KEY".into()],
        has_memory: true,
        auto: false,
    }
}

#[test]
fn a_manifest_round_trips() {
    let m = sample();
    let text = m.to_json();
    assert!(text.contains("\"format\": 1"), "{text}");
    assert_eq!(Manifest::parse(&m.name, &text).unwrap(), m);
}

#[test]
fn unknown_keys_are_ignored_and_credentials_may_be_left_out() {
    let mut doc: Value = serde_json::from_str(&sample().to_json()).unwrap();
    doc["future"] = json!({"anything": true});
    doc.as_object_mut().unwrap().remove("credentials");
    doc.as_object_mut().unwrap().remove("auto");
    let m = Manifest::parse("x", &doc.to_string()).unwrap();
    assert!(m.credentials.is_empty());
    assert!(!m.auto);
}

#[test]
fn names_are_checked() {
    for good in [
        "before",
        "a",
        "app-2026-10-13-debian",
        "v1.2_rc",
        &"x".repeat(64),
    ] {
        assert_eq!(check_name(good), Ok(()), "{good}");
    }
    for bad in ["", "-x", ".x", "a b", "a/b", "é", &"x".repeat(65)] {
        let err = check_name(bad).unwrap_err();
        assert!(err.contains("\nUse 1 to 64"), "{bad}: {err}");
    }
}

#[test]
fn default_names_follow_the_design_with_a_counter() {
    let none = |_: &str| false;
    assert_eq!(
        default_name("my-project", "2026-10-12", "debian", none),
        "my-project-2026-10-12-debian"
    );
    let used = ["app-2026-10-12-debian", "app-2026-10-12-debian-2"];
    assert_eq!(
        default_name("app", "2026-10-12", "debian", |n| used.contains(&n)),
        "app-2026-10-12-debian-3"
    );
}

#[test]
fn default_names_are_always_valid() {
    for folder in [
        "My Project (old)",
        ".hidden",
        "",
        "日本",
        &"long".repeat(40),
    ] {
        let name = default_name(folder, "2026-10-12", "ubuntu", |_| false);
        assert_eq!(check_name(&name), Ok(()), "{folder}: {name}");
        assert!(name.ends_with("-2026-10-12-ubuntu"), "{name}");
        let counted = default_name(folder, "2026-10-12", "ubuntu", |n| n == name);
        assert_eq!(check_name(&counted), Ok(()), "{counted}");
    }
    assert_eq!(
        default_name("My Project", "2026-10-12", "debian", |_| false),
        "My-Project-2026-10-12-debian"
    );
}

#[test]
fn dates_fall_back_to_utc() {
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(1_790_000_000), "2026-09-21");
    assert_eq!(utc_date(951_782_400), "2000-02-29");
    assert_eq!(local_date(1_790_000_000).len(), 10);
}

#[test]
fn a_newer_format_says_to_update_cave() {
    let mut doc: Value = serde_json::from_str(&sample().to_json()).unwrap();
    doc["format"] = json!(FORMAT_VERSION + 1);
    let err = Manifest::parse("snap", &doc.to_string()).unwrap_err();
    assert_eq!(
        err,
        ManifestError::Newer {
            name: "snap".into(),
            format: FORMAT_VERSION + 1
        }
    );
    assert!(err.to_string().contains("\nUpdate cave"), "{err}");
}

#[test]
fn damaged_manifests_say_what_is_wrong() {
    let mut doc: Value = serde_json::from_str(&sample().to_json()).unwrap();
    doc["vm"]["net"] = json!("wifi");
    for (text, why) in [
        ("not json".to_string(), "expected"),
        ("{}".to_string(), "no format version"),
        (r#"{"format": 1}"#.to_string(), "no vm"),
        (doc.to_string(), "unknown net mode wifi"),
    ] {
        let err = Manifest::parse("snap", &text).unwrap_err();
        assert!(err.to_string().contains(why), "{text}: {err}");
        assert!(err.to_string().contains("damaged"), "{err}");
    }
}

#[test]
fn memory_is_used_only_when_saved_on_the_same_macos_build() {
    let m = sample();
    assert_eq!(m.memory_usable("26A434"), Ok(()));
    let other = m.memory_usable("26B100").unwrap_err();
    assert!(
        other.contains("26A434") && other.contains("26B100"),
        "{other}"
    );
    let without = Manifest {
        has_memory: false,
        ..sample()
    };
    assert!(without.memory_usable("26A434").is_err());
}

#[test]
fn only_finished_snapshots_are_listed() {
    let state = std::env::temp_dir().join(format!("cave-snapshots-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    assert!(names(&state).unwrap().is_empty());

    for name in ["b", "a"] {
        let d = dir(&state, name);
        fs::create_dir_all(&d).unwrap();
        write_manifest(
            &d,
            &Manifest {
                name: name.into(),
                ..sample()
            },
        )
        .unwrap();
    }
    // Unfinished: no manifest yet.
    fs::create_dir_all(dir(&state, "c")).unwrap();
    assert_eq!(names(&state).unwrap(), ["a", "b"]);
    assert!(taken(&state, "a") && !taken(&state, "c"));
    fs::create_dir_all(state.join("forks/f")).unwrap();
    assert!(taken(&state, "f"), "forks share the namespace");
    assert_eq!(read_manifest(&dir(&state, "a")).unwrap().name, "a");
    assert!(!dir(&state, "a").join(".snapshot.json.partial").exists());
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn the_host_build_is_readable() {
    let build = host_os_build().unwrap();
    assert!(build.chars().all(|c| c.is_ascii_alphanumeric()), "{build}");
}
