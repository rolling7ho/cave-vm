use super::*;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cave-audit-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
    dir
}

fn vm(project: &Path, fork: Option<&str>) -> VmId {
    VmId {
        project: project.to_string_lossy().into_owned(),
        fork: fork.map(Into::into),
    }
}

fn exec(argv: &[&str], credentials: &[&str], exit_code: Option<i32>) -> AuditEvent {
    AuditEvent::Exec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        credentials: credentials.iter().map(|s| s.to_string()).collect(),
        exit_code,
    }
}

#[test]
fn each_vm_has_its_own_log_in_its_folder() {
    let project = Path::new("/p/app");
    assert_eq!(
        path(&vm(project, None)),
        Some(PathBuf::from("/p/app/.cave/audit.log"))
    );
    assert_eq!(
        path(&vm(project, Some("fork-1"))),
        Some(PathBuf::from("/p/app/.cave/forks/fork-1/audit.log"))
    );
    assert_eq!(path(&vm(project, Some("../escape"))), None);
}

#[test]
fn entries_are_appended_one_per_line_and_read_back_in_order() {
    let project = temp("order");
    let id = vm(&project, None);
    record(&id, 1, exec(&["npm", "test"], &[], Some(0)));
    record(
        &id,
        2,
        AuditEvent::Write {
            path: "src/app.ts".into(),
            size: 1200,
        },
    );
    record(
        &id,
        3,
        AuditEvent::Connect {
            host: "192.168.1.10".into(),
            port: 445,
            allowed: false,
            reason: Some("LAN".into()),
        },
    );
    let log = path(&id).unwrap();
    let text = fs::read_to_string(&log).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert_eq!(
        text.lines().next().unwrap(),
        r#"{"time_unix_ms":1,"event":"exec","argv":["npm","test"],"exit_code":0}"#
    );

    let (entries, total) = read(&log, None).unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        entries.iter().map(|e| e.time_unix_ms).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let (latest, total) = read(&log, Some(2)).unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        latest.iter().map(|e| e.time_unix_ms).collect::<Vec<_>>(),
        [2, 3]
    );
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn credentials_are_recorded_by_name() {
    let project = temp("credentials");
    let id = vm(&project, None);
    record(&id, 1, exec(&["./deploy.sh"], &["API_KEY"], Some(0)));
    let text = fs::read_to_string(path(&id).unwrap()).unwrap();
    assert!(text.contains(r#""credentials":["API_KEY"]"#), "{text}");
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn a_missing_log_is_empty_and_bad_lines_are_skipped() {
    let project = temp("bad");
    let log = path(&vm(&project, None)).unwrap();
    assert_eq!(read(&log, None).unwrap(), (Vec::new(), 0));
    fs::write(
        &log,
        "not json\n{\"time_unix_ms\":5,\"event\":\"write\",\"path\":\"a\",\"size\":1}\n",
    )
    .unwrap();
    let (entries, total) = read(&log, None).unwrap();
    assert_eq!(total, 1);
    assert_eq!(entries[0].time_unix_ms, 5);
    let _ = fs::remove_dir_all(&project);
}

#[test]
fn a_log_that_cannot_be_written_does_not_panic() {
    // The project's .cave/ does not exist, so the log cannot be created.
    record(
        &vm(Path::new("/nonexistent/cave-audit"), None),
        1,
        exec(&["true"], &[], Some(0)),
    );
}
