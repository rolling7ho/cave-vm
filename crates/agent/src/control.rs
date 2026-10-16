//! The control connection: liveness, snapshot preparation, and the clock
//! and timezone.

use std::io;
use std::path::Path;

use cave_vm_proto::agent::Message;
use cave_vm_proto::{ErrorCode, read_frame};

use crate::session::kind;
use crate::stream::{Out, Stream};
use crate::sys;
use crate::timezone;

pub fn run(mut reader: Box<dyn Stream>, out: &Out) -> io::Result<()> {
    loop {
        let message = match read_frame::<_, Message>(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(()),
            Err(e) => return out.send(&Message::Error(e.to_error_message())),
        };
        match message {
            Message::Ping => out.send(&Message::Pong)?,
            Message::Pong | Message::Ok => {}
            Message::SnapshotPrepare => {
                sys::sync();
                out.send(&Message::SnapshotReady)?;
            }
            Message::Restored {
                time_unix_ns,
                timezone,
            } => match sys::set_clock(time_unix_ns) {
                Ok(()) => {
                    // A timezone that cannot be set leaves the old one; the
                    // clock is what matters.
                    if let Some(zone) = timezone
                        && let Err(e) = timezone::set(Path::new("/"), &zone)
                    {
                        eprintln!("cave-agent: could not set the timezone: {e}");
                    }
                    out.send(&Message::Ok)?
                }
                Err(e) => out.error(
                    ErrorCode::PermissionDenied,
                    format!("could not set the guest clock: {e}"),
                )?,
            },
            other => {
                return out.error(
                    ErrorCode::BadRequest,
                    format!("unexpected {} on the control connection", kind(&other)),
                );
            }
        }
    }
}
