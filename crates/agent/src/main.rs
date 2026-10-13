//! The guest agent, built for Linux (musl) and reached over vsock.
//!
//! Started by the guest's init. Listens on vsock port
//! [`AGENT_PORT`](cave_vm_proto::AGENT_PORT) and serves each connection on
//! its own thread.

#![deny(unsafe_code)]

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use cave_vm_agent::sys::VsockListener;
    use cave_vm_agent::{Config, serve};
    use cave_vm_proto::AGENT_PORT;

    let listener = match VsockListener::bind(AGENT_PORT) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cave-agent: cannot listen on vsock port {AGENT_PORT}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = Config::default();
    loop {
        match listener.accept() {
            Ok(conn) => {
                let config = config.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(Box::new(conn), &config) {
                        eprintln!("cave-agent: {e}");
                    }
                });
            }
            Err(e) => eprintln!("cave-agent: accept: {e}"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("cave-agent runs inside a Linux guest; on the Mac, use cave.");
    ExitCode::FAILURE
}
