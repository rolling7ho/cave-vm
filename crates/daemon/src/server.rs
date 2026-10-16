//! The daemon API socket: one connection per operation (docs/protocol.md).

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cave_vm_proto::agent::Message;
use cave_vm_proto::daemon::{AuditEvent, Reply, Request, VmId};
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};

use crate::audit;
use crate::credentials::{self, Redactor};
use crate::forward;
use crate::home::Home;
use crate::link::daemon_software;
use crate::progress;
use crate::relay;
use crate::vms::Vms;

/// The port sshd listens on in the VM, for remote editing.
const SSH_PORT: u16 = 22;

/// How long `stop_all` gives each VMM after SIGTERM before SIGKILL (D43).
const FORCE_STOP_GRACE: Duration = Duration::from_secs(5);

/// How often a `sync` waiting for its VM checks that the client is still
/// there.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Binds the daemon socket, readable and writable by this user only. A
/// socket left by a daemon that is gone is replaced; a live one is an error.
pub fn bind(home: &Home) -> io::Result<UnixListener> {
    let path = home.socket();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a cave daemon is already running on {}", path.display()),
            ));
        }
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves connections until the listener fails.
pub fn serve(listener: &UnixListener, vms: &Arc<Vms>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let vms = Arc::clone(vms);
        thread::spawn(move || {
            let _ = handle(stream, &vms);
        });
    }
    Ok(())
}

fn send_error(stream: &mut UnixStream, e: ErrorMessage) -> io::Result<()> {
    write_frame(stream, &Reply::Error(e)).map_err(io::Error::other)
}

/// One connection: the handshake, then one request.
pub fn handle(mut stream: UnixStream, vms: &Vms) -> io::Result<()> {
    let next = |stream: &mut UnixStream| -> io::Result<Option<Request>> {
        match read_frame::<_, Request>(stream) {
            Ok(request) => Ok(request),
            Err(e) => {
                let _ = send_error(stream, e.to_error_message());
                Err(io::Error::other(e))
            }
        }
    };
    let Some(first) = next(&mut stream)? else {
        return Ok(());
    };
    let Request::Hello(hello) = first else {
        return send_error(
            &mut stream,
            ErrorMessage::new(ErrorCode::BadRequest, "the first message must be hello"),
        );
    };
    match hello.accept(VERSIONS, daemon_software()) {
        Ok(reply) => write_frame(&mut stream, &Reply::Hello(reply)).map_err(io::Error::other)?,
        Err(e) => return send_error(&mut stream, e),
    }
    let Some(request) = next(&mut stream)? else {
        return Ok(());
    };
    let reply = match request {
        Request::Exec {
            vm,
            mut exec,
            credentials,
        } => {
            // Values go into the command's environment on the agent link
            // only, and are redacted from what comes back.
            let values = match credentials::resolve(Path::new(&vm.project), &credentials) {
                Ok(values) => values,
                Err(e) => return send_error(&mut stream, e),
            };
            let started = audit::now_ms();
            let argv = exec.argv.clone();
            let connection = vms.agent_connection_with(&vm, &credentials);
            // Recorded once it reached the VM, even if the client went away
            // before it ended.
            let last = match connection {
                Ok((agent, _in_use, held)) => {
                    // Credentials from earlier calls may be in the VM too.
                    let mut known = credentials::values_of(Path::new(&vm.project), &held);
                    known.extend(values.iter().cloned());
                    let redactor = Redactor::new(&known);
                    exec.env.extend(values);
                    relay::run_redacted(&mut stream, agent, &Message::Exec(exec), &redactor).last
                }
                Err(e) => return send_error(&mut stream, e),
            };
            let exit_code = match last {
                Some(Reply::Exit { code, .. }) => code,
                _ => None,
            };
            let event = AuditEvent::Exec {
                argv,
                credentials,
                exit_code,
            };
            audit::record(&vm, started, event);
            return Ok(());
        }
        Request::FileRead { vm, path } => {
            let (agent, _in_use, held) = match vms.agent_connection_with(&vm, &[]) {
                Ok(connection) => connection,
                Err(e) => return send_error(&mut stream, e),
            };
            let redactor = Redactor::new(&credentials::values_of(Path::new(&vm.project), &held));
            let request = Message::FileRead { path };
            relay::run_redacted(&mut stream, agent, &request, &redactor);
            return Ok(());
        }
        Request::FileWrite {
            vm,
            path,
            mode,
            create_dirs,
        } => {
            let started = audit::now_ms();
            let request = Message::FileWrite {
                path: path.clone(),
                mode,
                create_dirs,
            };
            if let Some(Some(Reply::FileWritten { size })) = relay(stream, vms, &vm, &request)? {
                audit::record(&vm, started, AuditEvent::Write { path, size });
            }
            return Ok(());
        }
        Request::Shell { vm, pty } => {
            return relay(stream, vms, &vm, &Message::PtyOpen(pty)).map(drop);
        }
        Request::Ssh { vm } => {
            let opened = vms.agent_connection(&vm).and_then(|(mut agent, in_use)| {
                forward::open_port(&mut agent, SSH_PORT)?;
                Ok((agent, in_use))
            });
            let (agent, _in_use) = match opened {
                Ok(opened) => opened,
                Err(e) => return send_error(&mut stream, e),
            };
            write_frame(&mut stream, &Reply::Ok).map_err(io::Error::other)?;
            return forward::splice(stream, agent);
        }
        Request::Start { vm, net, ports } => {
            // A first start downloads the image; the client shows progress.
            let mut sink = stream.try_clone()?;
            progress::with_sink(
                move |reply| {
                    let _ = write_frame(&mut sink, &reply);
                },
                || vms.start(&vm, net, &ports),
            )
            .map(Reply::Policy)
        }
        Request::Remove { vm, name: None } => vms.remove(&vm).map(|()| Reply::Ok),
        Request::Remove {
            vm,
            name: Some(name),
        } => vms
            .remove(&VmId {
                fork: Some(name),
                ..vm
            })
            .map(|()| Reply::Ok),
        Request::Fork { vm, name, from } => {
            vms.fork(&vm, name.as_deref(), from.as_deref())
                .map(|forked| Reply::Forked {
                    name: forked.name,
                    from: forked.from,
                    reason: forked.fallback,
                    credentials: forked.credentials,
                })
        }
        Request::Doctor => Ok(Reply::Checks {
            checks: vms.doctor(),
        }),
        Request::Gc { apply } => vms.gc(apply).map(Reply::Usage),
        Request::Images => vms.images().map(|images| Reply::Images { images }),
        Request::Inspect { vm, log_limit } => vms.inspect(&vm, log_limit),
        Request::Pull { vm, force } => vms.pull(&vm, force).map(|pulled| Reply::Pulled {
            snapshot: pulled.snapshot,
            applied: pulled.applied,
            deleted: pulled.deleted,
        }),
        Request::Restore { vm, name } => {
            vms.restore(&vm, name.as_deref())
                .map(|restored| Reply::Restored {
                    name: restored.name,
                    memory_restored: restored.fallback.is_none(),
                    reason: restored.fallback,
                    saved: restored.saved,
                    credentials: restored.credentials,
                })
        }
        Request::Snapshot { vm, name, before } => {
            let taken = if before {
                vms.snapshot_before(&vm)
            } else {
                vms.snapshot(&vm, name.as_deref())
            };
            let reply = taken.map(|(name, size)| Reply::Snapshot { name, size });
            let taken = reply.is_ok();
            write_frame(&mut stream, &reply.unwrap_or_else(Reply::Error))
                .map_err(io::Error::other)?;
            // The design syncs the mirror on snapshot, after the client
            // has its answer.
            if taken
                && vms.check_mirror(&vm).is_ok()
                && let Err(e) = vms.sync(&vm)
            {
                eprintln!("cave-daemon: cannot sync the Mac mirror: {}", e.message);
            }
            return Ok(());
        }
        Request::Stop { vm } => vms.suspend(&vm).map(|_| Reply::Ok),
        Request::StopAll => Ok(Reply::StoppedAll {
            vms: vms.force_stop_all(FORCE_STOP_GRACE),
        }),
        Request::Sync { vm } => {
            // A sealed project would otherwise wait for nothing.
            let ready = vms
                .check_mirror(&vm)
                .and_then(|()| wait_running(&mut stream, vms, &vm));
            if let Err(e) = ready {
                return send_error(&mut stream, e);
            }
            vms.sync(&vm).map(|synced| Reply::Synced {
                written: synced.written,
                deleted: synced.deleted,
                conflicts: synced.conflicts,
                pending: synced.pending,
                held: synced.held,
                redacted: synced.redacted,
            })
        }
        Request::List { project } => Ok(Reply::List {
            entries: vms.list(project.as_deref()),
        }),
        Request::Hello(_) => Err(ErrorMessage::new(
            ErrorCode::BadRequest,
            "hello was already sent",
        )),
        other => Err(ErrorMessage::new(
            ErrorCode::BadRequest,
            format!("this daemon does not support {} yet", request_name(&other)),
        )
        .with_fix("Update cave, or use a command this version supports.")),
    };
    let reply = reply.unwrap_or_else(Reply::Error);
    write_frame(&mut stream, &reply).map_err(io::Error::other)
}

/// Relays an agent operation for a running VM. Returns `None` if it never
/// reached the VM, and otherwise the reply that ended it, if it ended
/// before the client went away ([`relay::Ended::last`]).
fn relay(
    mut stream: UnixStream,
    vms: &Vms,
    vm: &VmId,
    request: &Message,
) -> io::Result<Option<Option<Reply>>> {
    match vms.agent_connection(vm) {
        Ok((agent, _in_use)) => Ok(Some(relay::run(&mut stream, agent, request).last)),
        Err(e) => send_error(&mut stream, e).map(|()| None),
    }
}

/// Waits until `vm` is running, saying so to the client first if it is
/// not (docs/cli.md, `cave sync`). Gives up if the client goes away.
fn wait_running(stream: &mut UnixStream, vms: &Vms, vm: &VmId) -> Result<(), ErrorMessage> {
    let mut said = false;
    while !vms.wait_running(vm, WAIT_POLL)? {
        if !said {
            let progress = Reply::Progress {
                message: "waiting for the VM".into(),
                done: None,
                total: None,
            };
            write_frame(stream, &progress)
                .map_err(|e| ErrorMessage::new(ErrorCode::Io, e.to_string()))?;
            said = true;
        }
        if client_gone(stream) {
            return Err(ErrorMessage::new(ErrorCode::Io, "the client went away"));
        }
    }
    Ok(())
}

/// Whether the client closed its side of `stream`. The client sends
/// nothing more after its request, so any byte read is dropped.
fn client_gone(mut stream: &UnixStream) -> bool {
    let mut byte = [0u8];
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let gone = matches!(stream.read(&mut byte), Ok(0));
    stream.set_nonblocking(false).is_err() || gone
}

/// The `type` of a request, such as `exec`.
fn request_name(request: &Request) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_else(|| "this request".into())
}
