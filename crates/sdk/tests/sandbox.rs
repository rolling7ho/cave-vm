//! The SDK against a fake daemon on a Unix socket.

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use cave_vm_proto::agent::Stream;
use cave_vm_proto::daemon::{NetMode, Policy, Reply, Request, Storage, VmId};
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};
use cave_vm_sdk::{Command, Daemon, Sandbox};

/// Answers one request, given the requests before it on its connection.
type Script = dyn Fn(&Request, &[Request]) -> Vec<Reply> + Send + Sync;

/// A fake daemon: a data directory with `daemon.sock`, and every request
/// it got after the handshake.
struct Fake {
    root: PathBuf,
    project: PathBuf,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn new(script: impl Fn(&Request, &[Request]) -> Vec<Reply> + Send + Sync + 'static) -> Fake {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cave-sdk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("app");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("cave.toml"), "").unwrap();
        let listener = UnixListener::bind(root.join("daemon.sock")).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Script> = Arc::new(script);
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let script = Arc::clone(&script);
                let seen = Arc::clone(&seen);
                thread::spawn(move || {
                    let Ok(Some(Request::Hello(hello))) = read_frame(&mut stream) else {
                        return;
                    };
                    let reply = hello.accept(VERSIONS, "fake").unwrap();
                    write_frame(&mut stream, &Reply::Hello(reply)).unwrap();
                    let mut history = Vec::new();
                    while let Ok(Some(request)) = read_frame::<_, Request>(&mut stream) {
                        seen.lock().unwrap().push(request.clone());
                        for reply in script(&request, &history) {
                            write_frame(&mut stream, &reply).unwrap();
                        }
                        history.push(request);
                    }
                });
            }
        });
        Fake {
            root,
            project,
            requests,
        }
    }

    fn sandbox(&self) -> Sandbox {
        Sandbox::open_with(Daemon::at(&self.root), self.project.join("src")).unwrap()
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// The VM id the SDK sends for the project, or one of its forks.
    fn vm(&self, fork: Option<&str>) -> VmId {
        VmId {
            project: self
                .project
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            fork: fork.map(Into::into),
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn policy() -> Policy {
    Policy {
        name: "app".into(),
        image: "debian".into(),
        storage: Storage::Sealed,
        net: NetMode::Nat,
        ports: Vec::new(),
        cpus: 2,
        memory_bytes: 2 << 30,
        ttl_seconds: None,
        widened: Vec::new(),
    }
}

fn output(stream: Stream, text: &str) -> Reply {
    Reply::Output {
        stream,
        data: text.as_bytes().to_vec().into(),
    }
}

#[test]
fn open_finds_the_project_from_a_subfolder_and_fails_outside_one() {
    let fake = Fake::new(|_, _| Vec::new());
    let sb = fake.sandbox();
    assert_eq!(sb.project(), fake.project.canonicalize().unwrap());
    assert_eq!(sb.fork_name(), None);

    let outside = fake.root.join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let e = Sandbox::open_with(Daemon::at(&fake.root), &outside).unwrap_err();
    assert!(e.message.starts_with("not a cave project"), "{e}");
    assert!(e.fix.unwrap().starts_with("Run `cave init`"));
}

#[test]
fn exec_starts_the_vm_and_collects_the_output() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![
            Reply::Progress {
                message: "starting".into(),
                done: None,
                total: None,
            },
            output(Stream::Stdout, "Linux "),
            output(Stream::Stderr, "warn\n"),
            output(Stream::Stdout, "cave\n"),
            Reply::Exit {
                code: Some(3),
                signal: None,
            },
        ],
        _ => Vec::new(),
    });
    let out = fake.sandbox().exec(["uname", "-a"]).unwrap();
    assert_eq!(out.stdout, b"Linux cave\n");
    assert_eq!(out.stderr, b"warn\n");
    assert_eq!(out.code, Some(3));
    assert!(!out.success());

    let requests = fake.requests();
    assert_eq!(
        requests[0],
        Request::Start {
            vm: fake.vm(None),
            net: None,
            ports: Vec::new()
        }
    );
    let Request::Exec {
        vm,
        exec,
        credentials,
    } = &requests[1]
    else {
        panic!("{requests:?}");
    };
    assert_eq!(*vm, fake.vm(None));
    assert_eq!(exec.argv, ["uname", "-a"]);
    assert!(!exec.stdin);
    assert!(credentials.is_empty());
}

#[test]
fn a_command_names_its_credentials() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![Reply::Exit {
            code: Some(0),
            signal: None,
        }],
        _ => Vec::new(),
    });
    let command = Command::new("./deploy.sh")
        .credential("API_KEY")
        .credential("DB");
    fake.sandbox().run(&command).unwrap();
    let requests = fake.requests();
    let Request::Exec {
        exec, credentials, ..
    } = &requests[1]
    else {
        panic!("{requests:?}");
    };
    assert_eq!(credentials, &["API_KEY", "DB"]);
    assert!(exec.env.is_empty());
}

#[test]
fn a_command_sends_its_stdin_cwd_and_env() {
    let fake = Fake::new(|request, history| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::InputEnd => {
            let mut echoed = Vec::new();
            for r in history {
                if let Request::Input { data } = r {
                    echoed.extend_from_slice(&data.0);
                }
            }
            vec![
                Reply::Output {
                    stream: Stream::Stdout,
                    data: echoed.into(),
                },
                Reply::Exit {
                    code: Some(0),
                    signal: None,
                },
            ]
        }
        _ => Vec::new(),
    });
    // Larger than one input message, so it goes up in several.
    let input: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
    let command = Command::new("cat")
        .cwd("/tmp")
        .env("LANG", "C")
        .stdin(input.clone());
    let out = fake.sandbox().run(&command).unwrap();
    assert!(out.success());
    assert_eq!(out.stdout, input);

    let requests = fake.requests();
    let Request::Exec { exec, .. } = &requests[1] else {
        panic!("{requests:?}");
    };
    assert_eq!(exec.argv, ["cat"]);
    assert_eq!(exec.cwd.as_deref(), Some("/tmp"));
    assert_eq!(exec.env.get("LANG").map(String::as_str), Some("C"));
    assert!(exec.stdin);
    let inputs = requests
        .iter()
        .filter(|r| matches!(r, Request::Input { .. }))
        .count();
    assert!(inputs >= 3, "{inputs} input messages");
}

#[test]
fn a_signal_ends_the_output_without_a_code() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![Reply::Exit {
            code: None,
            signal: Some("KILL".into()),
        }],
        _ => Vec::new(),
    });
    let out = fake.sandbox().exec(["sleep", "100"]).unwrap();
    assert_eq!(out.code, None);
    assert_eq!(out.signal.as_deref(), Some("KILL"));
}

#[test]
fn an_empty_command_is_refused_before_reaching_the_daemon() {
    let fake = Fake::new(|_, _| Vec::new());
    let e = fake.sandbox().exec(Vec::<String>::new()).unwrap_err();
    assert_eq!(e.message, "the command is empty");
    assert!(fake.requests().is_empty());
}

#[test]
fn snapshot_restore_fork_and_destroy_send_the_matching_requests() {
    let fake = Fake::new(|request, _| match request {
        Request::Snapshot { name, .. } => vec![Reply::Snapshot {
            name: name.clone().unwrap(),
            size: 4096,
        }],
        Request::Restore { name, .. } => vec![Reply::Restored {
            name: name.clone().unwrap(),
            memory_restored: true,
            reason: None,
            saved: Some("app-2026-10-14-debian".into()),
            credentials: vec!["API_KEY".into()],
        }],
        Request::Fork { from, .. } => vec![Reply::Forked {
            name: "fork-1".into(),
            from: from.clone().unwrap_or_else(|| "app".into()),
            reason: Some("format 1 is not supported".into()),
            credentials: Vec::new(),
        }],
        Request::Remove { .. } => vec![Reply::Ok],
        _ => Vec::new(),
    });
    let sb = fake.sandbox();
    let snapshot = sb.snapshot("before").unwrap();
    assert_eq!((snapshot.name.as_str(), snapshot.size), ("before", 4096));
    let restored = sb.restore("before").unwrap();
    assert!(restored.memory_restored);
    assert_eq!(restored.saved.as_deref(), Some("app-2026-10-14-debian"));
    assert_eq!(restored.credentials, ["API_KEY"]);
    let forked = sb.fork_from("before").unwrap();
    assert_eq!(forked.from, "before");
    assert_eq!(forked.reason.as_deref(), Some("format 1 is not supported"));
    let fork = forked.sandbox;
    assert_eq!(fork.fork_name(), Some("fork-1"));
    assert_eq!(fork.project(), sb.project());
    assert_eq!(sb.fork().unwrap().from, "app");
    fork.destroy().unwrap();
    sb.destroy().unwrap();

    let vm = fake.vm(None);
    assert_eq!(
        fake.requests(),
        [
            Request::Snapshot {
                vm: vm.clone(),
                name: Some("before".into()),
                before: false,
            },
            Request::Restore {
                vm: vm.clone(),
                name: Some("before".into())
            },
            Request::Fork {
                vm: vm.clone(),
                name: None,
                from: Some("before".into())
            },
            Request::Fork {
                vm: vm.clone(),
                name: None,
                from: None
            },
            Request::Remove {
                vm: fake.vm(Some("fork-1")),
                name: None
            },
            Request::Remove {
                vm: vm.clone(),
                name: None
            },
        ]
    );
}

#[test]
fn a_fork_runs_commands_in_itself() {
    let fake = Fake::new(|request, _| match request {
        Request::Fork { .. } => vec![Reply::Forked {
            name: "fork-1".into(),
            from: "app".into(),
            reason: None,
            credentials: Vec::new(),
        }],
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![Reply::Exit {
            code: Some(0),
            signal: None,
        }],
        _ => Vec::new(),
    });
    let fork = fake.sandbox().fork().unwrap().sandbox;
    fork.exec(["true"]).unwrap();
    let requests = fake.requests();
    assert_eq!(requests[1].vm().cloned(), Some(fake.vm(Some("fork-1"))));
    assert_eq!(requests[2].vm().cloned(), Some(fake.vm(Some("fork-1"))));
}

/// SHA-256 of `hello`.
const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

#[test]
fn read_file_collects_the_data_and_checks_its_digest() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::FileRead { path, .. } => {
            let sha256 = if path.ends_with("bad") {
                "0".repeat(64)
            } else {
                HELLO_SHA256.into()
            };
            vec![
                Reply::FileInfo {
                    size: 5,
                    mode: 0o644,
                },
                Reply::FileData {
                    data: b"hel".to_vec().into(),
                },
                Reply::FileData {
                    data: b"lo".to_vec().into(),
                },
                Reply::FileEnd { sha256 },
            ]
        }
        _ => Vec::new(),
    });
    let sb = fake.sandbox();
    assert_eq!(sb.read_file("src/a.txt").unwrap(), b"hello");
    assert_eq!(sb.read_file("/etc/hostname").unwrap(), b"hello");
    let e = sb.read_file("bad").unwrap_err();
    assert!(e.message.contains("does not match"), "{e}");

    let paths: Vec<String> = fake
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::FileRead { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(
        paths,
        ["/workspace/src/a.txt", "/etc/hostname", "/workspace/bad"]
    );
}

#[test]
fn write_file_sends_the_data_in_chunks_with_its_digest() {
    let fake = Fake::new(|request, history| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::FileEnd { .. } => {
            let size = history
                .iter()
                .map(|r| match r {
                    Request::FileData { data } => data.0.len() as u64,
                    _ => 0,
                })
                .sum();
            vec![Reply::FileWritten { size }]
        }
        _ => Vec::new(),
    });
    let sb = fake.sandbox();
    assert_eq!(sb.write_file("./notes/a.txt", b"hello").unwrap(), 5);
    let requests = fake.requests();
    assert_eq!(
        requests[1],
        Request::FileWrite {
            vm: fake.vm(None),
            path: "/workspace/notes/a.txt".into(),
            mode: None,
            create_dirs: true,
        }
    );
    assert_eq!(
        requests[3],
        Request::FileEnd {
            sha256: HELLO_SHA256.into()
        }
    );

    // Larger than one message, so it goes in several.
    let big = vec![7u8; 600_000];
    assert_eq!(sb.write_file("big", &big).unwrap(), 600_000);
    let chunks = fake
        .requests()
        .iter()
        .filter(|r| matches!(r, Request::FileData { .. }))
        .count();
    assert_eq!(chunks, 1 + 3);
}

#[test]
fn daemon_errors_keep_their_code_message_and_fix() {
    let fake = Fake::new(|_, _| {
        vec![Reply::Error(
            ErrorMessage::new(ErrorCode::NotFound, "no snapshot named \"gone\"")
                .with_fix("Run `cave ls` to see the snapshots"),
        )]
    });
    let e = fake.sandbox().restore("gone").unwrap_err();
    assert_eq!(e.code, Some(ErrorCode::NotFound));
    assert_eq!(
        e.to_string(),
        "no snapshot named \"gone\"\n  fix: Run `cave ls` to see the snapshots"
    );
}

#[test]
fn a_missing_daemon_says_how_to_start_it() {
    let dir = Path::new("/nonexistent/cave-sdk");
    let fake = Fake::new(|_, _| Vec::new());
    let sb = Sandbox::open_with(Daemon::at(dir), &fake.project).unwrap();
    let e = sb.snapshot("before").unwrap_err();
    assert!(e.message.starts_with("cannot reach the cave daemon"), "{e}");
    assert!(e.fix.unwrap().contains("cave ls"));
}

#[test]
fn a_reply_that_does_not_answer_the_request_is_an_error() {
    let fake = Fake::new(|_, _| vec![Reply::Ok]);
    let e = fake.sandbox().snapshot("before").unwrap_err();
    assert!(
        e.message
            .starts_with("the cave daemon sent an unexpected reply to snapshot"),
        "{e}"
    );
}
