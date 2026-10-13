//! The daemon API socket: one connection per operation (docs/protocol.md).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;

use cave_vm_proto::daemon::{Reply, Request};
use cave_vm_proto::{ErrorCode, ErrorMessage, VERSIONS, read_frame, write_frame};

use crate::home::Home;
use crate::link::daemon_software;
use crate::vms::Vms;

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
        Request::Start { vm, net, ports } => vms.start(&vm, net, &ports).map(Reply::Policy),
        Request::Stop { vm } => vms.stop(&vm).map(|()| Reply::Ok),
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

/// The `type` of a request, such as `exec`.
fn request_name(request: &Request) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_else(|| "this request".into())
}
