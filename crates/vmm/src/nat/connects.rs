//! Connection attempts kept for the daemon's audit log until it takes them
//! with `take_connects` (docs/protocol.md, "VMM control").

use std::sync::{Arc, Mutex};

use cave_vm_proto::daemon::{AuditEntry, AuditEvent};

use super::ConnectEvent;

/// Most attempts kept between two takes. Past it, attempts are counted
/// and dropped, so a guest that opens connections in a loop cannot grow
/// the VMM's memory while the daemon is away.
pub const MAX_KEPT: usize = 10_000;

/// The attempts since the daemon last took them. Clones share them.
#[derive(Debug, Clone, Default)]
pub struct Connects(Arc<Mutex<Kept>>);

#[derive(Debug, Default)]
struct Kept {
    entries: Vec<AuditEntry>,
    dropped: u64,
}

impl Connects {
    /// Keeps `event`, seen at `time_unix_ms`, or counts it as dropped when
    /// [`MAX_KEPT`] attempts are waiting.
    pub fn push(&self, time_unix_ms: u64, event: &ConnectEvent) {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if kept.entries.len() >= MAX_KEPT {
            kept.dropped += 1;
            return;
        }
        kept.entries.push(AuditEntry {
            time_unix_ms,
            event: AuditEvent::Connect {
                host: event.dst.ip().to_string(),
                port: event.dst.port(),
                allowed: event.allowed,
                reason: event.reason.map(String::from),
            },
        });
    }

    /// The attempts kept since the last take, oldest first, and how many
    /// were dropped meanwhile.
    pub fn take(&self) -> (Vec<AuditEntry>, u64) {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = std::mem::take(&mut kept.dropped);
        (std::mem::take(&mut kept.entries), dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nat::Protocol;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn event(port: u16, reason: Option<&'static str>) -> ConnectEvent {
        ConnectEvent {
            protocol: Protocol::Tcp,
            dst: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 10), port),
            allowed: reason.is_none(),
            reason,
        }
    }

    #[test]
    fn attempts_become_audit_entries_and_are_taken_once() {
        let connects = Connects::default();
        connects.clone().push(7, &event(445, Some("LAN")));
        connects.push(8, &event(443, None));
        let (entries, dropped) = connects.take();
        assert_eq!(dropped, 0);
        assert_eq!(
            entries,
            [
                AuditEntry {
                    time_unix_ms: 7,
                    event: AuditEvent::Connect {
                        host: "192.168.1.10".into(),
                        port: 445,
                        allowed: false,
                        reason: Some("LAN".into()),
                    },
                },
                AuditEntry {
                    time_unix_ms: 8,
                    event: AuditEvent::Connect {
                        host: "192.168.1.10".into(),
                        port: 443,
                        allowed: true,
                        reason: None,
                    },
                },
            ]
        );
        assert_eq!(connects.take(), (Vec::new(), 0));
    }

    #[test]
    fn past_the_limit_attempts_are_counted_and_dropped() {
        let connects = Connects::default();
        for i in 0..MAX_KEPT + 3 {
            connects.push(i as u64, &event(80, None));
        }
        let (entries, dropped) = connects.take();
        assert_eq!((entries.len(), dropped), (MAX_KEPT, 3));
        assert_eq!(connects.take().1, 0);
    }
}
