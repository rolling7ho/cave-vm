use super::*;
use crate::error::ErrorCode;
use crate::frame::{FrameError, decode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn parse<M: frame::Message>(value: &Value) -> Result<M, FrameError> {
    decode(&serde_json::to_vec(value).unwrap())
}

/// `value` decodes to `message` and encodes back to the same JSON.
fn check<M>(value: Value, message: M)
where
    M: frame::Message + PartialEq + std::fmt::Debug + DeserializeOwned,
{
    assert_eq!(parse::<M>(&value).unwrap(), message, "{value}");
    assert_eq!(serde_json::to_value(&message).unwrap(), value);
}

fn vm(project: &str) -> VmId {
    VmId {
        project: project.into(),
        fork: None,
    }
}

#[test]
fn exec_takes_credential_names_beside_the_agent_fields() {
    // The example from docs/protocol.md.
    check(
        json!({"type": "exec", "vm": {"project": "/Users/ivan/p"}, "argv": ["./deploy.sh"], "credentials": ["API_KEY"]}),
        Request::Exec {
            vm: vm("/Users/ivan/p"),
            exec: Exec {
                argv: vec!["./deploy.sh".into()],
                ..Exec::default()
            },
            credentials: vec!["API_KEY".into()],
        },
    );
}

#[test]
fn vm_object_example() {
    check(
        json!({"type": "stop", "vm": {"project": "/Users/ivan/Desktop/Projects/my-project", "fork": "try-a"}}),
        Request::Stop {
            vm: VmId {
                project: "/Users/ivan/Desktop/Projects/my-project".into(),
                fork: Some("try-a".into()),
            },
        },
    );
}

#[test]
fn requests_round_trip() {
    let p = || vm("/p");
    let cases: Vec<(Value, Request)> = vec![
        (
            json!({"type": "init", "project": "/p", "storage": "sealed", "image": "base"}),
            Request::Init {
                project: "/p".into(),
                storage: Storage::Sealed,
                image: "base".into(),
            },
        ),
        (
            json!({"type": "start", "vm": {"project": "/p"}, "net": "lan", "ports": [{"mac": 3000, "vm": 3000}]}),
            Request::Start {
                vm: p(),
                net: Some(NetMode::Lan),
                ports: vec![PortMap {
                    mac: 3000,
                    vm: 3000,
                }],
            },
        ),
        (
            json!({"type": "start", "vm": {"project": "/p"}}),
            Request::Start {
                vm: p(),
                net: None,
                ports: Vec::new(),
            },
        ),
        (
            json!({"type": "ssh", "vm": {"project": "/p"}}),
            Request::Ssh { vm: p() },
        ),
        (
            json!({"type": "shell", "vm": {"project": "/p"}, "term": "xterm-256color", "cols": 80, "rows": 24}),
            Request::Shell {
                vm: p(),
                pty: PtyOpen {
                    argv: None,
                    cwd: None,
                    env: Default::default(),
                    term: Some("xterm-256color".into()),
                    cols: 80,
                    rows: 24,
                },
            },
        ),
        (
            json!({"type": "file_write", "vm": {"project": "/p"}, "path": "a.txt", "create_dirs": true}),
            Request::FileWrite {
                vm: p(),
                path: "a.txt".into(),
                mode: None,
                create_dirs: true,
            },
        ),
        (
            json!({"type": "snapshot", "vm": {"project": "/p"}, "name": "before"}),
            Request::Snapshot {
                vm: p(),
                name: Some("before".into()),
            },
        ),
        (
            json!({"type": "fork", "vm": {"project": "/p"}, "name": "try-a", "from": "before"}),
            Request::Fork {
                vm: p(),
                name: Some("try-a".into()),
                from: Some("before".into()),
            },
        ),
        (json!({"type": "list"}), Request::List { project: None }),
        (
            json!({"type": "inspect", "vm": {"project": "/p"}, "log_limit": 20}),
            Request::Inspect {
                vm: p(),
                log_limit: Some(20),
            },
        ),
        (
            json!({"type": "gc", "apply": true}),
            Request::Gc { apply: true },
        ),
        (json!({"type": "images"}), Request::Images),
        (json!({"type": "doctor"}), Request::Doctor),
        (
            json!({"type": "pty_resize", "cols": 100, "rows": 30}),
            Request::PtyResize {
                cols: 100,
                rows: 30,
            },
        ),
    ];
    for (value, message) in cases {
        check(value, message);
    }
}

#[test]
fn replies_round_trip() {
    let policy = Policy {
        name: "my-project".into(),
        image: "base@sha256:3f2a".into(),
        storage: Storage::Sealed,
        net: NetMode::Nat,
        ports: vec![PortMap {
            mac: 3000,
            vm: 3000,
        }],
        cpus: 2,
        memory_bytes: 2 << 30,
        ttl_seconds: None,
        widened: vec![],
    };
    let policy_json = json!({"name": "my-project", "image": "base@sha256:3f2a", "storage": "sealed", "net": "nat", "ports": [{"mac": 3000, "vm": 3000}], "cpus": 2, "memory_bytes": 2147483648u64});
    let mut policy_reply = policy_json.clone();
    policy_reply["type"] = json!("policy");
    check(policy_reply, Reply::Policy(policy.clone()));

    check(
        json!({"type": "progress", "message": "Downloading the debian image", "done": 104857600, "total": 524288000}),
        Reply::Progress {
            message: "Downloading the debian image".into(),
            done: Some(104_857_600),
            total: Some(524_288_000),
        },
    );
    check(
        json!({"type": "snapshot", "name": "before", "size": 1400}),
        Reply::Snapshot {
            name: "before".into(),
            size: 1400,
        },
    );
    check(
        json!({"type": "forked", "name": "fork-1", "from": "app"}),
        Reply::Forked {
            name: "fork-1".into(),
            from: "app".into(),
            reason: None,
            credentials: Vec::new(),
        },
    );
    check(
        json!({"type": "forked", "name": "fork-2", "from": "snap", "credentials": ["API_KEY"]}),
        Reply::Forked {
            name: "fork-2".into(),
            from: "snap".into(),
            reason: None,
            credentials: vec!["API_KEY".into()],
        },
    );
    check(
        json!({"type": "restored", "name": "before", "memory_restored": true, "saved": "app-2026-10-13-debian"}),
        Reply::Restored {
            name: "before".into(),
            memory_restored: true,
            reason: None,
            saved: Some("app-2026-10-13-debian".into()),
            credentials: Vec::new(),
        },
    );
    check(
        json!({"type": "restored", "name": "before", "memory_restored": false, "reason": "it was taken without memory state", "credentials": ["API_KEY"]}),
        Reply::Restored {
            name: "before".into(),
            memory_restored: false,
            reason: Some("it was taken without memory state".into()),
            saved: None,
            credentials: vec!["API_KEY".into()],
        },
    );
    check(
        json!({"type": "list", "entries": [
            {"project": "/p", "name": "my-project", "kind": "vm", "state": "running", "size": 3100, "created_unix": 1791800000},
            {"project": "/p", "name": "snap-2", "kind": "snapshot", "auto": true, "size": 600, "created_unix": 1791800100}
        ]}),
        Reply::List {
            entries: vec![
                ListEntry {
                    project: "/p".into(),
                    name: "my-project".into(),
                    kind: EntryKind::Vm,
                    state: Some(VmState::Running),
                    auto: false,
                    size: 3100,
                    created_unix: 1_791_800_000,
                },
                ListEntry {
                    project: "/p".into(),
                    name: "snap-2".into(),
                    kind: EntryKind::Snapshot,
                    state: None,
                    auto: true,
                    size: 600,
                    created_unix: 1_791_800_100,
                },
            ],
        },
    );
    check(
        json!({"type": "inspect", "policy": policy_json,
            "resources": {"cpus": 2, "memory_bytes": 2147483648u64, "disk_used": 3, "disk_size": 20},
            "audit": [
                {"time_unix_ms": 1, "event": "exec", "argv": ["./deploy.sh"], "credentials": ["API_KEY"], "exit_code": 0},
                {"time_unix_ms": 2, "event": "write", "path": "src/app.ts", "size": 1200},
                {"time_unix_ms": 3, "event": "connect", "host": "192.168.1.10", "port": 445, "allowed": false, "reason": "LAN"}
            ],
            "audit_total": 412, "state": "running", "started_unix": 100}),
        Reply::Inspect {
            policy,
            resources: Resources {
                cpus: 2,
                memory_bytes: 2 << 30,
                disk_used: 3,
                disk_size: 20,
            },
            audit: vec![
                AuditEntry {
                    time_unix_ms: 1,
                    event: AuditEvent::Exec {
                        argv: vec!["./deploy.sh".into()],
                        credentials: vec!["API_KEY".into()],
                        exit_code: Some(0),
                    },
                },
                AuditEntry {
                    time_unix_ms: 2,
                    event: AuditEvent::Write {
                        path: "src/app.ts".into(),
                        size: 1200,
                    },
                },
                AuditEntry {
                    time_unix_ms: 3,
                    event: AuditEvent::Connect {
                        host: "192.168.1.10".into(),
                        port: 445,
                        allowed: false,
                        reason: Some("LAN".into()),
                    },
                },
            ],
            audit_total: 412,
            state: VmState::Running,
            started_unix: Some(100),
        },
    );
    check(
        json!({"type": "images", "images": [{"name": "debian", "digest": "sha256:3f2a", "downloaded": true, "used_by": 3, "base": true}]}),
        Reply::Images {
            images: vec![ImageInfo {
                name: "debian".into(),
                digest: "sha256:3f2a".into(),
                downloaded: true,
                used_by: 3,
                base: true,
                update: None,
            }],
        },
    );
    check(
        json!({"type": "checks", "checks": [{"name": "Apple Silicon", "ok": true}, {"name": "signature", "ok": false, "fix": "brew reinstall cave-vm"}]}),
        Reply::Checks {
            checks: vec![
                Check {
                    name: "Apple Silicon".into(),
                    ok: true,
                    fix: None,
                },
                Check {
                    name: "signature".into(),
                    ok: false,
                    fix: Some("brew reinstall cave-vm".into()),
                },
            ],
        },
    );
    check(
        json!({"type": "usage", "images": [{"name": "ubuntu", "digest": "sha256:8b17", "size": 19, "in_use": false}],
            "projects": [{"project": "/p", "disk": 31, "snapshots": 15, "forks": 2}],
            "reclaimable": [{"what": "unused images", "size": 19}]}),
        Reply::Usage(Usage {
            images: vec![ImageUsage {
                name: "ubuntu".into(),
                digest: "sha256:8b17".into(),
                size: 19,
                in_use: false,
            }],
            projects: vec![ProjectUsage {
                project: "/p".into(),
                disk: 31,
                snapshots: 15,
                forks: 2,
            }],
            reclaimable: vec![Reclaimable {
                what: "unused images".into(),
                size: 19,
            }],
            reclaimed: None,
        }),
    );
}

#[test]
fn pull_conflict_is_an_error_with_files() {
    let mut error = ErrorMessage::new(ErrorCode::Conflict, "2 files changed on both sides");
    error.files = Some(vec!["src/app.ts".into(), "package.json".into()]);
    check(
        json!({"type": "error", "code": "conflict", "message": "2 files changed on both sides", "files": ["src/app.ts", "package.json"]}),
        Reply::Error(error),
    );
}

#[test]
fn a_request_and_its_reply_can_share_a_type() {
    let request = json!({"type": "snapshot", "vm": {"project": "/p"}});
    let reply = json!({"type": "snapshot", "name": "s", "size": 1});
    assert!(parse::<Request>(&request).is_ok());
    assert!(parse::<Reply>(&reply).is_ok());
    assert_eq!(
        parse::<Request>(&reply).unwrap_err().code(),
        ErrorCode::BadRequest
    );
}

#[test]
fn bad_requests_get_the_right_codes() {
    let code = |value: Value| parse::<Request>(&value).unwrap_err().code();
    assert_eq!(code(json!({"type": "teleport"})), ErrorCode::UnknownType);
    assert_eq!(
        code(json!({"type": "stop", "vm": {"project": "relative/path"}})),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(json!({"type": "init", "project": "p", "storage": "sealed", "image": "base"})),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(json!({"type": "exec", "vm": {"project": "/p"}, "argv": []})),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(json!({"type": "start", "vm": {"project": "/p"}, "net": "everywhere"})),
        ErrorCode::BadRequest
    );
    assert_eq!(
        parse::<Reply>(&json!({"type": "exit"})).unwrap_err().code(),
        ErrorCode::BadRequest
    );
    assert_eq!(
        parse::<Reply>(&json!({"type": "quantum"}))
            .unwrap_err()
            .code(),
        ErrorCode::UnknownType
    );
}
