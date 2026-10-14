//! The `cave` binary against a fake daemon listening on
//! `$CAVE_HOME/daemon.sock`.

use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use cave_vm_proto::agent::Stream;
use cave_vm_proto::daemon::{
    EntryKind, ListEntry, NetMode, Policy, PortMap, Reclaimable, Reply, Request, Storage, Usage,
    VmState,
};
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A scratch layout: a fake home, a data directory and a project folder.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        // Short, so the socket path fits in sun_path.
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "cvc{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "data", "home/proj"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Scratch { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn project(&self) -> PathBuf {
        self.root.join("home/proj")
    }

    fn cave(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cave"))
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("CAVE_HOME", self.data())
            .env("CAVE_DAEMON", self.root.join("no-daemon-here"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// What the fake daemon knows and has been asked.
#[derive(Default)]
struct State {
    running: bool,
    requests: Vec<Request>,
    fail_start: Option<ErrorMessage>,
}

fn policy(net: Option<NetMode>, ports: Vec<PortMap>) -> Policy {
    let widened = match net {
        Some(NetMode::Lan) => vec!["--net lan".to_string()],
        _ => Vec::new(),
    };
    Policy {
        name: "proj".into(),
        image: "base@sha256:3f2a9c1e0000".into(),
        storage: Storage::Sealed,
        net: net.unwrap_or(NetMode::Nat),
        ports,
        cpus: 2,
        memory_bytes: 2 << 30,
        ttl_seconds: None,
        widened,
    }
}

fn handle(state: &Mutex<State>, request: Request) -> Reply {
    let mut s = state.lock().unwrap();
    s.requests.push(request.clone());
    match request {
        Request::List { project } => Reply::List {
            entries: if s.running || project.is_none() {
                vec![ListEntry {
                    project: project.unwrap_or_default(),
                    name: "proj".into(),
                    kind: EntryKind::Vm,
                    state: Some(if s.running {
                        VmState::Running
                    } else {
                        VmState::Stopped
                    }),
                    auto: false,
                    size: 3 << 29,
                    created_unix: 1_790_000_000,
                }]
            } else {
                Vec::new()
            },
        },
        Request::Start { net, ports, .. } => {
            if let Some(e) = s.fail_start.clone() {
                return Reply::Error(e);
            }
            s.running = true;
            Reply::Policy(policy(net, ports))
        }
        Request::Stop { .. } => {
            s.running = false;
            Reply::Ok
        }
        Request::Remove { name: None, .. } => {
            s.running = false;
            Reply::Ok
        }
        Request::Gc { apply } => Reply::Usage(Usage {
            images: Vec::new(),
            projects: Vec::new(),
            reclaimable: vec![Reclaimable {
                what: "unused images".into(),
                size: 3 << 29,
            }],
            reclaimed: apply.then_some(3 << 29),
        }),
        _ => Reply::Error(ErrorMessage::new(ErrorCode::BadRequest, "not in the fake")),
    }
}

fn serve(mut stream: UnixStream, state: Arc<Mutex<State>>) {
    let Ok(Some(Request::Hello(hello))) = read_frame::<_, Request>(&mut stream) else {
        return;
    };
    let reply = hello.accept(VERSIONS, "fake daemon").unwrap();
    write_frame(&mut stream, &Reply::Hello(reply)).unwrap();
    while let Ok(Some(request)) = read_frame::<_, Request>(&mut stream) {
        if let Request::Exec { exec, .. } = &request {
            state.lock().unwrap().requests.push(request.clone());
            fake_exec(&mut stream, &exec.argv);
            return;
        }
        let reply = handle(&state, request);
        if write_frame(&mut stream, &reply).is_err() {
            return;
        }
    }
}

/// A fake guest command: `cat` echoes stdin to stdout and says "done" on
/// stderr, `false` exits 1, `crash` is killed, anything else is not found.
fn fake_exec(stream: &mut UnixStream, argv: &[String]) {
    let send = |stream: &mut UnixStream, reply: Reply| write_frame(stream, &reply).unwrap();
    let output = |s: Stream, data: &[u8]| Reply::Output {
        stream: s,
        data: data.to_vec().into(),
    };
    let exit = |code: Option<i32>, signal: Option<&str>| Reply::Exit {
        code,
        signal: signal.map(Into::into),
    };
    match argv[0].as_str() {
        "cat" => {
            let mut input = Vec::new();
            while let Ok(Some(request)) = read_frame::<_, Request>(stream) {
                match request {
                    Request::Input { data } => input.extend_from_slice(&data.0),
                    Request::InputEnd => break,
                    other => panic!("{other:?}"),
                }
            }
            send(stream, output(Stream::Stdout, &input));
            send(stream, output(Stream::Stderr, b"done\n"));
            send(stream, exit(Some(0), None));
        }
        "false" => send(stream, exit(Some(1), None)),
        "crash" => send(stream, exit(None, Some("KILL"))),
        other => {
            let message = format!("command not found in the VM: {other}\n");
            send(stream, output(Stream::Stderr, message.as_bytes()));
            send(stream, exit(Some(127), None));
        }
    }
}

fn fake_daemon(scratch: &Scratch) -> Arc<Mutex<State>> {
    let state = Arc::new(Mutex::new(State::default()));
    let listener = UnixListener::bind(scratch.data().join("daemon.sock")).unwrap();
    let shared = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let state = shared.clone();
            thread::spawn(move || serve(stream, state));
        }
    });
    state
}

fn starts(state: &Mutex<State>) -> Vec<Request> {
    state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| matches!(r, Request::Start { .. }))
        .cloned()
        .collect()
}

#[test]
fn init_writes_the_config_and_ignores_the_state_folder() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["init", "--sealed"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Created cave.toml (sealed, debian, 2 CPUs, 2 GB)\n\
         Added .cave/ to .gitignore\n\
         Wrote .claude/skills/cave\n\
         Next: cave shell, or cave agent claude\n"
    );
    let skill = fs::read_to_string(s.project().join(".claude/skills/cave/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: cave\n"), "{skill}");
    let config = fs::read_to_string(s.project().join("cave.toml")).unwrap();
    assert!(config.contains("mode    = \"sealed\""), "{config}");
    assert!(s.project().join(".cave").is_dir());
    assert_eq!(
        fs::read_to_string(s.project().join(".gitignore")).unwrap(),
        ".cave/\n"
    );

    // Again: nothing changes.
    fs::write(s.project().join("cave.toml"), "# mine\n").unwrap();
    let out = s.cave(&s.project(), &["init", "--synced"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("already exists"), "{}", stdout(&out));
    assert_eq!(
        fs::read_to_string(s.project().join("cave.toml")).unwrap(),
        "# mine\n"
    );
    assert_eq!(
        fs::read_to_string(s.project().join(".gitignore")).unwrap(),
        ".cave/\n"
    );
}

#[test]
fn init_without_a_terminal_picks_sealed_and_says_so() {
    let s = Scratch::new();
    fs::write(s.project().join(".gitignore"), "target").unwrap();
    let out = s.cave(&s.project(), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("No terminal to ask, so the project is sealed"));
    assert_eq!(
        fs::read_to_string(s.project().join(".gitignore")).unwrap(),
        "target\n.cave/\n"
    );
}

#[test]
fn init_refuses_the_home_folder_and_nested_projects() {
    let s = Scratch::new();
    let out = s.cave(&s.home(), &["init", "--sealed"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("error: cave cannot set up a project in your home folder"));
    assert!(stderr(&out).contains("\n  fix: "));

    s.cave(&s.project(), &["init", "--sealed"]);
    let inner = s.project().join("sub");
    fs::create_dir(&inner).unwrap();
    let out = s.cave(&inner, &["init", "--sealed"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("inside the project at"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn init_rejects_an_unknown_image() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["init", "--image", "arch"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown image \"arch\""));
    assert!(!s.project().join("cave.toml").exists());
}

#[test]
fn run_sets_up_the_folder_starts_and_prints_the_policy() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    let out = s.cave(
        &s.project(),
        &["run", "--net", "lan", "-p", "3000", "-p", "8081:80"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(s.project().join("cave.toml").is_file());
    let err = stderr(&out);
    assert!(
        err.contains("Set up this folder as a cave project"),
        "{err}"
    );
    assert!(err.contains("proj: started in "), "{err}");
    assert!(
        err.contains("lan: outbound, and LAN ranges are reachable   [--net lan]"),
        "{err}"
    );
    assert!(
        err.contains("localhost:3000 -> 3000, localhost:8081 -> 80"),
        "{err}"
    );

    let Request::Start { vm, net, ports } = &starts(&state)[0] else {
        unreachable!()
    };
    assert_eq!(vm.project, s.project().to_string_lossy());
    assert_eq!(vm.fork, None);
    assert_eq!(*net, Some(NetMode::Lan));
    assert_eq!(
        *ports,
        vec![
            PortMap {
                mac: 3000,
                vm: 3000
            },
            PortMap { mac: 8081, vm: 80 }
        ]
    );

    // A second run leaves the network mode to cave.toml and reports the
    // VM was already up.
    let out = s.cave(&s.project(), &["run"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("proj: already running"),
        "{}",
        stderr(&out)
    );
    let Request::Start { net, .. } = &starts(&state)[1] else {
        unreachable!()
    };
    assert_eq!(*net, None);
}

#[test]
fn run_rejects_bad_ports_before_contacting_the_daemon() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["run", "-p", "70000"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("not a port number"),
        "{}",
        stderr(&out)
    );
    assert!(!s.project().join("cave.toml").exists());
}

#[test]
fn daemon_errors_keep_their_fix_line() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    state.lock().unwrap().fail_start = Some(
        ErrorMessage::new(ErrorCode::Busy, "another VM is using 3000")
            .with_fix("Pick another port"),
    );
    let out = s.cave(&s.project(), &["run", "-p", "3000"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out).lines().rev().take(2).collect::<Vec<_>>(),
        [
            "  fix: Pick another port",
            "error: another VM is using 3000"
        ]
    );
}

#[test]
fn stop_says_when_nothing_is_running() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    s.cave(&s.project(), &["init", "--sealed"]);

    let out = s.cave(&s.project(), &["stop"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "proj is not running\n");

    state.lock().unwrap().running = true;
    let out = s.cave(&s.project(), &["stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = stdout(&out);
    assert!(
        out.starts_with("Suspended proj (") && out.ends_with("s)\n"),
        "{out}"
    );
    assert!(!state.lock().unwrap().running);
}

#[test]
fn stop_outside_a_project_says_how_to_fix_it() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["stop"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not a cave project"));
    assert!(stderr(&out).contains("fix: Run `cave init`"));
}

#[test]
fn ls_prints_a_table_or_json() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    state.lock().unwrap().running = true;
    s.cave(&s.project(), &["init", "--sealed"]);

    let out = s.cave(&s.project(), &["ls"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let table = stdout(&out);
    let lines: Vec<&str> = table.lines().collect();
    assert!(
        lines[0].starts_with("NAME  KIND      STATE      SIZE    CREATED"),
        "{table}"
    );
    assert!(
        lines[1].starts_with("proj  vm        running    1.5 GB  20"),
        "{table}"
    );

    let out = s.cave(&s.project(), &["ls", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["entries"][0]["state"], "running");

    let requests = &state.lock().unwrap().requests;
    let Request::List { project } = &requests[requests.len() - 1] else {
        unreachable!()
    };
    assert_eq!(project.as_deref(), Some(&*s.project().to_string_lossy()));
}

#[test]
fn a_daemon_that_cannot_start_names_the_program() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["ls"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("cannot start the cave daemon"), "{err}");
    assert!(err.contains("no-daemon-here"), "{err}");
}

fn cave_with_stdin(s: &Scratch, args: &[&str], input: &[u8]) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_cave"))
        .args(args)
        .current_dir(s.project())
        .env("HOME", s.home())
        .env("CAVE_HOME", s.data())
        .env("CAVE_DAEMON", s.root.join("no-daemon-here"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn exec_streams_stdin_and_output_and_exits_with_the_code() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    let out = cave_with_stdin(&s, &["exec", "--", "cat", "-n"], b"hello\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "hello\n");
    // Set up on first use and booted, then the command's own stderr.
    let err = stderr(&out);
    assert!(
        err.contains("Set up this folder as a cave project"),
        "{err}"
    );
    assert!(err.contains("proj: started in"), "{err}");
    assert!(err.ends_with("done\n"), "{err}");
    let execs: Vec<_> = state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter_map(|r| match r {
            Request::Exec { exec, .. } => Some(exec.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].argv, ["cat", "-n"]);
    assert!(execs[0].stdin);
    assert_eq!(execs[0].cwd, None);

    // Already running: no policy this time.
    let out = s.cave(&s.project(), &["exec", "--", "false"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "");
}

#[test]
fn exec_exit_codes_follow_the_shell() {
    let s = Scratch::new();
    fake_daemon(&s);
    let out = s.cave(&s.project(), &["exec", "--", "crash"]);
    assert_eq!(out.status.code(), Some(137));
    let out = s.cave(&s.project(), &["exec", "--", "nope"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(stderr(&out).contains("command not found in the VM: nope"));
}

#[test]
fn exec_needs_a_command_after_the_separator() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["exec"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("missing command after --"));
    assert!(stderr(&out).contains("fix: cave exec -- uname -a"));
}

#[test]
fn exec_fails_with_125_when_cave_does() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["exec", "--", "uname"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("cannot start the cave daemon"));
}

#[test]
fn shell_without_a_terminal_points_at_exec() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["shell"]);
    assert_eq!(out.status.code(), Some(125), "{}", stderr(&out));
    assert!(stderr(&out).contains("cave shell needs a terminal"));
    assert!(stderr(&out).contains("cave exec -- <cmd>"));
}

#[test]
fn rm_without_a_terminal_needs_yes() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    fs::write(s.project().join("cave.toml"), "").unwrap();
    let out = s.cave(&s.project(), &["rm"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cave rm needs a terminal to ask first"));
    assert!(stderr(&out).contains("--yes"));
    assert!(state.lock().unwrap().requests.is_empty());

    let out = s.cave(&s.project(), &["rm", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Removed the VM for proj; cave.toml is still here"));
    assert!(matches!(
        state.lock().unwrap().requests.as_slice(),
        [Request::Remove { name: None, .. }]
    ));
}

#[test]
fn gc_without_a_terminal_needs_yes() {
    let s = Scratch::new();
    let state = fake_daemon(&s);
    let out = s.cave(&s.project(), &["gc"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("Reclaimable   1.5 GB   unused images 1.5 GB"));
    assert!(stderr(&out).contains("cave gc needs a terminal to ask first"));
    assert!(matches!(
        state.lock().unwrap().requests.as_slice(),
        [Request::Gc { apply: false }]
    ));

    let out = s.cave(&s.project(), &["gc", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).ends_with("Deleted 1.5 GB\n"),
        "{}",
        stdout(&out)
    );
    let out = s.cave(&s.project(), &["gc", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["reclaimable"][0]["what"], "unused images");
}

#[test]
fn cp_needs_exactly_one_side_in_the_vm() {
    let s = Scratch::new();
    let out = s.cave(&s.project(), &["cp", "a", "b"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("one side of cave cp must be in the VM"));
    assert!(stderr(&out).contains("fix: Prefix the VM path with `:`"));

    let out = s.cave(&s.project(), &["cp", ":a", ":b"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not within the VM"));

    let out = s.cave(&s.project(), &["cp", ":a"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("needs a source and a destination"));
}
