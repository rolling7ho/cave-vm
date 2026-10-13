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
    let m = Manifest::parse("x", &doc.to_string()).unwrap();
    assert!(m.credentials.is_empty());
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
    assert_eq!(read_manifest(&dir(&state, "a")).unwrap().name, "a");
    assert!(!dir(&state, "a").join(".snapshot.json.partial").exists());
    fs::remove_dir_all(&state).unwrap();
}

#[test]
fn the_host_build_is_readable() {
    let build = host_os_build().unwrap();
    assert!(build.chars().all(|c| c.is_ascii_alphanumeric()), "{build}");
}
