//! Messages on the VMM control link: the daemon's socket pair to each VMM
//! process, which is the VMM's standard input (docs/protocol.md, "VMM
//! control").
//!
//! The daemon sends [`ToVmm`] requests one at a time and reads a [`FromVmm`]
//! reply to each. The reply to `agent_connect` carries a file descriptor
//! alongside the frame.

use serde::{Deserialize, Serialize};

use crate::daemon::{AuditEntry, NetMode};
use crate::error::ErrorMessage;
use crate::frame;
use crate::handshake::Hello;

/// How to boot a VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmmConfig {
    /// The kernel image, an absolute path.
    pub kernel: String,
    /// The project disk, an absolute path.
    pub disk: String,
    pub memory_bytes: u64,
    pub cpus: u32,
    pub net: NetMode,
    /// A snapshot folder to restore instead of booting, an absolute path.
    /// The disk must already be the snapshot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
}

/// What the daemon sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToVmm {
    Hello(Hello),
    Boot {
        config: VmmConfig,
    },
    AgentConnect,
    Ping,
    /// Pauses the VM, saves it into `dir` (an absolute path to a folder),
    /// and lets it run on. Replied to with `ok`.
    Snapshot {
        dir: String,
    },
    Stop,
    /// The guest's connection attempts since the last `take_connects`, for
    /// the audit log. Replied to with `connects`.
    TakeConnects,
    /// A `type` this build does not know; never returned by
    /// [`frame::decode`].
    #[serde(other, skip_serializing)]
    Unknown,
}

/// What the VMM sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromVmm {
    Hello(Hello),
    Error(ErrorMessage),
    /// The guest agent answered.
    Booted,
    /// A connection to the guest agent is attached to this frame.
    AgentConnection,
    Pong,
    Ok,
    /// The reply to `take_connects`: `connect` audit entries, oldest first.
    Connects {
        entries: Vec<AuditEntry>,
        /// Attempts not kept because too many were waiting.
        #[serde(default, skip_serializing_if = "is_zero")]
        dropped: u64,
    },
    /// A `type` this build does not know; never returned by
    /// [`frame::decode`].
    #[serde(other, skip_serializing)]
    Unknown,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl frame::Message for ToVmm {
    fn is_unknown(&self) -> bool {
        matches!(self, ToVmm::Unknown)
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            ToVmm::Boot { config }
                if !config.kernel.starts_with('/') || !config.disk.starts_with('/') =>
            {
                Err("\"kernel\" and \"disk\" must be absolute paths".into())
            }
            ToVmm::Boot { config } if config.cpus == 0 || config.memory_bytes == 0 => {
                Err("\"cpus\" and \"memory_bytes\" must be more than zero".into())
            }
            ToVmm::Boot { config }
                if config.restore.as_ref().is_some_and(|d| !d.starts_with('/')) =>
            {
                Err("\"restore\" must be an absolute path".into())
            }
            ToVmm::Snapshot { dir } if !dir.starts_with('/') => {
                Err("\"dir\" must be an absolute path".into())
            }
            _ => Ok(()),
        }
    }
}

impl frame::Message for FromVmm {
    fn is_unknown(&self) -> bool {
        matches!(self, FromVmm::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameError, decode, encode};
    use serde_json::json;

    fn config() -> VmmConfig {
        VmmConfig {
            kernel: "/k/Image".into(),
            disk: "/p/.cave/disk.img".into(),
            memory_bytes: 2 << 30,
            cpus: 2,
            net: NetMode::Nat,
            restore: None,
        }
    }

    #[test]
    fn requests_and_replies_round_trip_as_documented() {
        let boot = ToVmm::Boot { config: config() };
        let value = serde_json::to_value(&boot).unwrap();
        assert_eq!(
            value,
            json!({"type": "boot", "config": {"kernel": "/k/Image", "disk": "/p/.cave/disk.img", "memory_bytes": 2147483648u64, "cpus": 2, "net": "nat"}})
        );
        let restore = ToVmm::Boot {
            config: VmmConfig {
                restore: Some("/p/.cave/snapshots/s".into()),
                ..config()
            },
        };
        assert_eq!(
            serde_json::to_value(&restore).unwrap()["config"]["restore"],
            "/p/.cave/snapshots/s"
        );
        let snapshot = ToVmm::Snapshot {
            dir: "/p/.cave/snapshots/s".into(),
        };
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({"type": "snapshot", "dir": "/p/.cave/snapshots/s"})
        );
        for message in [
            boot,
            restore,
            snapshot,
            ToVmm::AgentConnect,
            ToVmm::Ping,
            ToVmm::Stop,
            ToVmm::TakeConnects,
        ] {
            let frame = encode(&message).unwrap();
            assert_eq!(decode::<ToVmm>(&frame[4..]).unwrap(), message);
        }
        for message in [
            FromVmm::Booted,
            FromVmm::AgentConnection,
            FromVmm::Pong,
            FromVmm::Ok,
        ] {
            let frame = encode(&message).unwrap();
            assert_eq!(decode::<FromVmm>(&frame[4..]).unwrap(), message);
        }
        assert_eq!(
            serde_json::to_value(FromVmm::AgentConnection).unwrap(),
            json!({"type": "agent_connection"})
        );
        assert_eq!(
            serde_json::to_value(ToVmm::TakeConnects).unwrap(),
            json!({"type": "take_connects"})
        );
        let connects = FromVmm::Connects {
            entries: vec![AuditEntry {
                time_unix_ms: 5,
                event: crate::daemon::AuditEvent::Connect {
                    host: "192.168.1.10".into(),
                    port: 445,
                    allowed: false,
                    reason: Some("LAN".into()),
                },
            }],
            dropped: 0,
        };
        assert_eq!(
            serde_json::to_value(&connects).unwrap(),
            json!({"type": "connects", "entries": [{"time_unix_ms": 5, "event": "connect", "host": "192.168.1.10", "port": 445, "allowed": false, "reason": "LAN"}]})
        );
        let frame = encode(&connects).unwrap();
        assert_eq!(decode::<FromVmm>(&frame[4..]).unwrap(), connects);
    }

    #[test]
    fn boot_needs_absolute_paths_and_resources() {
        let mut bad = config();
        bad.disk = "disk.img".into();
        let frame = encode(&ToVmm::Boot { config: bad }).unwrap();
        assert!(matches!(
            decode::<ToVmm>(&frame[4..]),
            Err(FrameError::BadMessage { .. })
        ));
        let mut bad = config();
        bad.cpus = 0;
        let frame = encode(&ToVmm::Boot { config: bad }).unwrap();
        assert!(matches!(
            decode::<ToVmm>(&frame[4..]),
            Err(FrameError::BadMessage { .. })
        ));
        let mut bad = config();
        bad.restore = Some("snap".into());
        let frame = encode(&ToVmm::Boot { config: bad }).unwrap();
        assert!(matches!(
            decode::<ToVmm>(&frame[4..]),
            Err(FrameError::BadMessage { .. })
        ));
        let frame = encode(&ToVmm::Snapshot { dir: "snap".into() }).unwrap();
        assert!(matches!(
            decode::<ToVmm>(&frame[4..]),
            Err(FrameError::BadMessage { .. })
        ));
    }
}
