//! The MCP server against a fake daemon on a Unix socket.

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use cave_vm_mcp::{PROTOCOL_VERSIONS, Tools, serve};
use cave_vm_proto::agent::Stream;
use cave_vm_proto::daemon::{NetMode, Policy, Reply, Request, Storage, VmId};
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};
use cave_vm_sdk::Daemon;
use serde_json::{Value, json};

/// Answers one request, given the requests before it on its connection.
type Script = dyn Fn(&Request, &[Request]) -> Vec<Reply> + Send + Sync;

/// A fake daemon in a temporary folder with a project in it.
struct Fake {
    root: PathBuf,
    project: PathBuf,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn new(script: impl Fn(&Request, &[Request]) -> Vec<Reply> + Send + Sync + 'static) -> Fake {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cave-mcp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("app");
        fs::create_dir_all(&project).unwrap();
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

    /// The responses to `messages`, one JSON-RPC message per line.
    fn session(&self, messages: &[Value]) -> Vec<Value> {
        let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
        let mut output = Vec::new();
        let tools = Tools::with_daemon(&self.project, Daemon::at(&self.root));
        serve(input.as_bytes(), &mut output, &tools).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The result of calling tool `name` with `arguments`.
    fn call(&self, name: &str, arguments: Value) -> Value {
        let mut responses = self.session(&[json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        })]);
        responses.remove(0)["result"].take()
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

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
        fresh_boot: None,
    }
}

/// The text of a tool result, and whether it is an error.
fn text(result: &Value) -> (String, bool) {
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap(),
    )
}

#[test]
fn the_lifecycle_methods_answer_and_notifications_do_not() {
    let fake = Fake::new(|_, _| Vec::new());
    let responses = fake.session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": "two", "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}),
    ]);
    assert_eq!(responses.len(), 5, "{responses:?}");
    let init = &responses[0]["result"];
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(init["protocolVersion"], "2025-03-26");
    assert_eq!(init["serverInfo"]["name"], "cave");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["instructions"].as_str().unwrap().contains("snapshot"));

    assert_eq!(
        responses[1],
        json!({"jsonrpc": "2.0", "id": "two", "result": {}})
    );

    let names: Vec<&str> = responses[2]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "exec",
            "read_file",
            "write_file",
            "snapshot",
            "restore",
            "fork",
            "destroy"
        ]
    );
    for tool in responses[2]["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 20);
    }

    assert_eq!(responses[3]["error"]["code"], -32601);
    assert_eq!(
        responses[4]["result"]["protocolVersion"],
        PROTOCOL_VERSIONS[0]
    );
    // Nothing reached the daemon.
    assert!(fake.requests().is_empty());
}

#[test]
fn bad_messages_get_json_rpc_errors() {
    let fake = Fake::new(|_, _| Vec::new());
    let input = "not json\n[1, 2]\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"nope\"}}\n";
    let mut output = Vec::new();
    let tools = Tools::with_daemon(&fake.project, Daemon::at(&fake.root));
    serve(input.as_bytes(), &mut output, &tools).unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[1]["error"]["code"], -32600);
    assert_eq!(responses[2]["error"]["code"], -32602);
    assert_eq!(responses[2]["id"], 7);
}

#[test]
fn exec_runs_the_command_with_bash_and_reports_how_it_ended() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![
            Reply::Output {
                stream: Stream::Stdout,
                data: b"hello\n".to_vec().into(),
            },
            Reply::Output {
                stream: Stream::Stderr,
                data: b"warning\n".to_vec().into(),
            },
            Reply::Exit {
                code: Some(1),
                signal: None,
                redacted: false,
            },
        ],
        _ => Vec::new(),
    });
    let result = fake.call(
        "exec",
        json!({"command": "echo hello && false", "cwd": "/tmp"}),
    );
    assert_eq!(
        text(&result),
        (
            "exit code 1\n--- stdout ---\nhello\n\n--- stderr ---\nwarning\n".into(),
            false
        )
    );
    let requests = fake.requests();
    let Request::Exec { vm, exec, .. } = &requests[1] else {
        panic!("{requests:?}");
    };
    assert_eq!(*vm, fake.vm(None));
    assert_eq!(exec.argv, ["bash", "-c", "echo hello && false"]);
    assert_eq!(exec.cwd.as_deref(), Some("/tmp"));

    // In a fork.
    fake.call("exec", json!({"command": "true", "fork": "fork-1"}));
    let requests = fake.requests();
    assert_eq!(requests[3].vm(), Some(&fake.vm(Some("fork-1"))));
}

#[test]
fn exec_names_credentials_and_never_handles_their_values() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        // The daemon has already redacted the value.
        Request::Exec { .. } => vec![
            Reply::Output {
                stream: Stream::Stdout,
                data: b"API_KEY: [API_KEY]\n".to_vec().into(),
            },
            Reply::Exit {
                code: Some(0),
                signal: None,
                redacted: false,
            },
        ],
        _ => Vec::new(),
    });
    let result = fake.call(
        "exec",
        json!({"command": "./deploy.sh", "credentials": ["API_KEY", "DB"]}),
    );
    assert_eq!(
        text(&result),
        (
            "exit code 0\n--- stdout ---\nAPI_KEY: [API_KEY]\n".into(),
            false
        )
    );
    let requests = fake.requests();
    let Request::Exec {
        exec, credentials, ..
    } = &requests[1]
    else {
        panic!("{requests:?}");
    };
    assert_eq!(credentials, &["API_KEY", "DB"]);
    assert!(exec.env.is_empty(), "values are the daemon's to add");

    assert_eq!(
        text(&fake.call("exec", json!({"command": "true", "credentials": "API_KEY"}))),
        ("\"credentials\" must be a list of strings".into(), true)
    );
}

#[test]
fn missing_or_wrong_arguments_are_tool_errors() {
    let fake = Fake::new(|_, _| Vec::new());
    assert_eq!(
        text(&fake.call("exec", json!({}))),
        ("\"command\" is required".into(), true)
    );
    assert_eq!(
        text(&fake.call("read_file", json!({"path": 3}))),
        ("\"path\" must be a string".into(), true)
    );
    assert!(fake.requests().is_empty());
}

#[test]
fn files_are_read_and_written_as_text() {
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const BINARY_SHA256: &str = "b3d510ef04275ca8e698e5b3cbb0ece3949ef9252f0cdc839e9ee347409a2209";
    let fake = Fake::new(|request, history| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::FileRead { path, .. } if path.ends_with("bin") => vec![
            Reply::FileInfo {
                size: 2,
                mode: 0o644,
            },
            Reply::FileData {
                data: vec![0xff, 0xfe].into(),
            },
            Reply::FileEnd {
                sha256: BINARY_SHA256.into(),
            },
        ],
        Request::FileRead { .. } => vec![
            Reply::FileInfo {
                size: 5,
                mode: 0o644,
            },
            Reply::FileData {
                data: b"hello".to_vec().into(),
            },
            Reply::FileEnd {
                sha256: HELLO_SHA256.into(),
            },
        ],
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
    assert_eq!(
        text(&fake.call("read_file", json!({"path": "a.txt"}))),
        ("hello".into(), false)
    );
    let (message, is_error) = text(&fake.call("read_file", json!({"path": "x.bin"})));
    assert!(is_error);
    assert!(message.contains("not UTF-8"), "{message}");
    assert_eq!(
        text(&fake.call(
            "write_file",
            json!({"path": "src/a.txt", "content": "hello"})
        )),
        ("Wrote 5 bytes to /workspace/src/a.txt".into(), false)
    );
    assert!(fake.requests().contains(&Request::FileEnd {
        sha256: HELLO_SHA256.into()
    }));
}

#[test]
fn snapshot_restore_fork_and_destroy() {
    let fake = Fake::new(|request, _| match request {
        Request::Snapshot { name, .. } => vec![Reply::Snapshot {
            name: name.clone().unwrap(),
            size: 1,
        }],
        Request::Restore { name, .. } => vec![Reply::Restored {
            name: name.clone().unwrap(),
            memory_restored: false,
            reason: Some("format 1 is not supported".into()),
            saved: Some("app-2026-10-14-debian".into()),
            credentials: Vec::new(),
        }],
        Request::Fork { from, .. } => vec![Reply::Forked {
            name: "fork-1".into(),
            from: from.clone().unwrap_or_else(|| "app".into()),
            reason: None,
            credentials: vec!["API_KEY".into()],
        }],
        Request::Remove { .. } => vec![Reply::Ok],
        _ => Vec::new(),
    });
    assert_eq!(
        text(&fake.call("snapshot", json!({"name": "before"}))),
        (
            "Saved snapshot before. Restore it with the restore tool.".into(),
            false
        )
    );
    let (restored, _) = text(&fake.call("restore", json!({"name": "before"})));
    assert!(restored.starts_with("Restored before."), "{restored}");
    assert!(restored.contains("format 1 is not supported"));
    assert!(restored.contains("saved as app-2026-10-14-debian"));
    let (forked, _) = text(&fake.call("fork", json!({"from": "before"})));
    assert!(forked.starts_with("Forked fork-1 from before."), "{forked}");
    assert!(forked.contains("fork: \"fork-1\""));
    assert!(
        forked.contains("taken after API_KEY had been injected"),
        "{forked}"
    );
    assert!(!restored.contains("injected"), "{restored}");
    assert_eq!(
        text(&fake.call("destroy", json!({"fork": "fork-1"}))),
        ("Removed fork fork-1.".into(), false)
    );
    // destroy never removes the project's own VM.
    assert_eq!(
        text(&fake.call("destroy", json!({}))),
        ("\"fork\" is required".into(), true)
    );

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
            Request::Remove {
                vm: fake.vm(Some("fork-1")),
                name: None
            },
        ]
    );
}

#[test]
fn daemon_errors_reach_the_agent_with_their_fix() {
    let fake = Fake::new(|_, _| {
        vec![Reply::Error(
            ErrorMessage::new(ErrorCode::NotFound, "no snapshot named \"gone\"")
                .with_fix("Run `cave ls` to see the snapshots"),
        )]
    });
    assert_eq!(
        text(&fake.call("restore", json!({"name": "gone"}))),
        (
            "no snapshot named \"gone\"\n  fix: Run `cave ls` to see the snapshots".into(),
            true
        )
    );
}

#[test]
fn outside_a_project_the_tools_say_how_to_set_one_up() {
    let fake = Fake::new(|_, _| Vec::new());
    let outside = fake.root.join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let tools = Tools::with_daemon(&outside, Daemon::at(&fake.root));
    let message = tools
        .call("exec", &json!({"command": "ls"}))
        .unwrap()
        .unwrap_err();
    assert!(message.starts_with("not a cave project"), "{message}");
    assert!(message.contains("fix: Run `cave init`"), "{message}");
}

#[test]
fn tools_set_to_a_fork_act_on_it_unless_a_call_names_another() {
    let fake = Fake::new(|request, _| match request {
        Request::Start { .. } => vec![Reply::Policy(policy())],
        Request::Exec { .. } => vec![Reply::Exit {
            code: Some(0),
            signal: None,
            redacted: false,
        }],
        Request::Fork { .. } => vec![Reply::Forked {
            name: "fork-2".into(),
            from: "app".into(),
            reason: None,
            credentials: Vec::new(),
        }],
        _ => Vec::new(),
    });
    let tools =
        Tools::with_daemon(&fake.project, Daemon::at(&fake.root)).on_fork(Some("try".into()));
    let call = |name: &str, arguments: Value| tools.call(name, &arguments).unwrap().unwrap();
    call("exec", json!({"command": "true"}));
    call("exec", json!({"command": "true", "fork": "other"}));
    call("fork", json!({}));
    let requests = fake.requests();
    assert_eq!(requests[1].vm(), Some(&fake.vm(Some("try"))));
    assert_eq!(requests[3].vm(), Some(&fake.vm(Some("other"))));
    // Forks are made from the project's own VM.
    assert_eq!(requests[4].vm(), Some(&fake.vm(None)));
}
