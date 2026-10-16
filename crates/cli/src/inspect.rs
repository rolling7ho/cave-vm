//! `cave inspect`: the policy, resources and audit log (docs/cli.md).

use std::process::Command;

use cave_vm_proto::daemon::{AuditEntry, AuditEvent, Reply, Request, VmState};

use crate::client::Connection;
use crate::commands::vm_id;
use crate::error::{CliError, Result};
use crate::policy;
use crate::project;
use crate::style::Paint;

/// Audit entries shown without `--log`.
const RECENT: u32 = 20;

/// Width of the audit log's detail column.
const DETAIL_WIDTH: usize = 34;

/// `cave inspect [--fork <name>] [--log] [--json]`.
pub fn run(fork: Option<String>, log: bool, json: bool) -> Result<()> {
    let project = project::require()?;
    let reply = Connection::call(&Request::Inspect {
        vm: vm_id(&project, fork),
        log_limit: (!log).then_some(RECENT),
    })?;
    if !matches!(reply, Reply::Inspect { .. }) {
        return Err(CliError::unexpected("inspect"));
    }
    if json {
        let mut doc = serde_json::to_value(&reply).map_err(|e| CliError::new(e.to_string()))?;
        if let Some(object) = doc.as_object_mut() {
            object.remove("type");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).map_err(|e| CliError::new(e.to_string()))?
        );
        return Ok(());
    }
    print!("{}", render(&reply, local_offset(), Paint::stdout()));
    Ok(())
}

/// The whole report for an `inspect` reply, with times shifted by
/// `offset` seconds from UTC.
fn render(reply: &Reply, offset: i64, paint: Paint) -> String {
    let Reply::Inspect {
        policy,
        resources,
        audit,
        audit_total,
        state,
        started_unix,
    } = reply
    else {
        return String::new();
    };
    let headline = match (state, started_unix) {
        (VmState::Running, Some(started)) => {
            let time = clock(*started as i64 + offset);
            format!("running since {}", &time[..5])
        }
        (VmState::Running, None) => "running".to_string(),
        (VmState::Suspended, _) => "suspended".to_string(),
        (VmState::Stopped, _) => "stopped".to_string(),
    };
    let mut out = policy::render_with_disk(
        policy,
        &headline,
        resources.disk_used,
        resources.disk_size,
        paint,
    );
    out.push_str("\n\n");
    let shown = audit.len() as u64;
    if *audit_total == 0 {
        out.push_str(&format!(
            "{}: nothing recorded yet\n",
            paint.heading("Audit log")
        ));
        return out;
    }
    if shown < *audit_total {
        out.push_str(&format!(
            "{} (last {shown} of {audit_total}, cave inspect --log for all)\n",
            paint.heading("Audit log")
        ));
    } else {
        out.push_str(&format!("{}\n", paint.heading("Audit log")));
    }
    for entry in audit {
        out.push_str(&audit_line(entry, offset, paint));
        out.push('\n');
    }
    out
}

/// One audit entry as a row: time, event, what, and how it went.
pub fn audit_line(entry: &AuditEntry, offset: i64, paint: Paint) -> String {
    let time = clock((entry.time_unix_ms / 1000) as i64 + offset);
    let (kind, detail, result) = match &entry.event {
        AuditEvent::Exec {
            argv,
            credentials,
            exit_code,
        } => {
            let mut detail = argv.join(" ");
            if !credentials.is_empty() {
                detail.push_str(&format!("   credentials: {}", credentials.join(", ")));
            }
            let result = match exit_code {
                Some(0) => paint.good("exit 0"),
                Some(c) => paint.notice(&format!("exit {c}")),
                None => paint.notice("no exit code"),
            };
            ("exec", detail, result)
        }
        AuditEvent::Write { path, size } => ("write", path.clone(), bytes(*size)),
        AuditEvent::Connect {
            host,
            port,
            allowed,
            reason,
        } => {
            let result = match (allowed, reason) {
                (true, _) => paint.good("allowed"),
                (false, Some(reason)) => paint.bad(&format!("denied ({reason})")),
                (false, None) => paint.bad("denied"),
            };
            ("connect", format!("{host}:{port}"), result)
        }
    };
    let gap = DETAIL_WIDTH.saturating_sub(detail.chars().count()).max(3);
    let kind = paint.bold(&format!("{kind:<9}"));
    format!("  {time}  {kind}{detail}{}{result}", " ".repeat(gap))
}

/// `HH:MM:SS` of `seconds` since the epoch, already shifted to local time.
fn clock(seconds: i64) -> String {
    let day = seconds.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
}

/// `213 B`, `1.2 KB`, `3.4 MB`.
fn bytes(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1 << 20 => format!("{:.1} KB", n as f64 / 1024.0),
        n if n < 1 << 30 => format!("{:.1} MB", n as f64 / f64::from(1 << 20)),
        n => policy::gigabytes(n),
    }
}

/// The local time zone's offset from UTC in seconds, through date(1),
/// since std has no time zones. UTC if it cannot be read.
pub fn local_offset() -> i64 {
    Command::new("/bin/date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or(0)
}

/// `+0200` or `-0530` in seconds.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.split_at_checked(1)? {
        ("+", d) => (1, d),
        ("-", d) => (-1, d),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cave_vm_proto::daemon::{NetMode, Policy, PortMap, Resources, Storage};

    fn entry(h: u64, m: u64, s: u64, event: AuditEvent) -> AuditEntry {
        AuditEntry {
            time_unix_ms: ((h * 60 + m) * 60 + s) * 1000,
            event,
        }
    }

    fn reply(audit: Vec<AuditEntry>, audit_total: u64) -> Reply {
        Reply::Inspect {
            policy: Policy {
                name: "my-project".into(),
                image: "debian@sha256:3f2a9c1e5d7b".into(),
                storage: Storage::Sealed,
                net: NetMode::Nat,
                ports: vec![PortMap {
                    mac: 3000,
                    vm: 3000,
                }],
                cpus: 2,
                memory_bytes: 2 << 30,
                ttl_seconds: None,
                widened: Vec::new(),
                fresh_boot: None,
            },
            resources: Resources {
                cpus: 2,
                memory_bytes: 2 << 30,
                disk_used: 3_328_599_655,
                disk_size: 20 << 30,
            },
            audit,
            audit_total,
            state: VmState::Running,
            started_unix: Some((9 * 60 + 14) * 60),
        }
    }

    #[test]
    fn the_report_matches_the_docs() {
        let audit = vec![
            entry(
                11,
                4,
                12,
                AuditEvent::Exec {
                    argv: vec!["npm".into(), "test".into()],
                    credentials: Vec::new(),
                    exit_code: Some(0),
                },
            ),
            entry(
                11,
                4,
                40,
                AuditEvent::Write {
                    path: "src/app.ts".into(),
                    size: 1229,
                },
            ),
            entry(
                11,
                5,
                1,
                AuditEvent::Connect {
                    host: "registry.npmjs.org".into(),
                    port: 443,
                    allowed: true,
                    reason: None,
                },
            ),
            entry(
                11,
                5,
                3,
                AuditEvent::Connect {
                    host: "192.168.1.10".into(),
                    port: 445,
                    allowed: false,
                    reason: Some("LAN".into()),
                },
            ),
            entry(
                11,
                6,
                20,
                AuditEvent::Exec {
                    argv: vec!["./deploy.sh".into()],
                    credentials: vec!["API_KEY".into()],
                    exit_code: Some(0),
                },
            ),
        ];
        assert_eq!(
            render(&reply(audit, 412), 0, Paint::OFF),
            "my-project: running since 09:14 (debian, sha256:3f2a9c1e)
  files     sealed: nothing on your Mac is reachable from the VM
  network   nat: outbound only; LAN ranges and this Mac are blocked
  ports     localhost:3000 -> 3000
  resources 2 CPUs, 2 GB memory, 3.1 GB of 20 GB disk used
  lifetime  no TTL

Audit log (last 5 of 412, cave inspect --log for all)
  11:04:12  exec     npm test                          exit 0
  11:04:40  write    src/app.ts                        1.2 KB
  11:05:01  connect  registry.npmjs.org:443            allowed
  11:05:03  connect  192.168.1.10:445                  denied (LAN)
  11:06:20  exec     ./deploy.sh   credentials: API_KEY   exit 0
"
        );
    }

    #[test]
    fn a_short_or_empty_log_and_other_states() {
        let one = vec![entry(
            0,
            0,
            1,
            AuditEvent::Exec {
                argv: vec!["yes".into()],
                credentials: Vec::new(),
                exit_code: None,
            },
        )];
        let text = render(&reply(one, 1), 0, Paint::OFF);
        assert!(
            text.contains("\n\nAudit log\n  00:00:01  exec     yes"),
            "{text}"
        );
        assert!(text.ends_with("no exit code\n"), "{text}");

        let Reply::Inspect {
            policy, resources, ..
        } = reply(Vec::new(), 0)
        else {
            unreachable!()
        };
        let stopped = Reply::Inspect {
            policy,
            resources,
            audit: Vec::new(),
            audit_total: 0,
            state: VmState::Suspended,
            started_unix: None,
        };
        let text = render(&stopped, 0, Paint::OFF);
        assert!(text.starts_with("my-project: suspended ("), "{text}");
        assert!(
            text.ends_with("Audit log: nothing recorded yet\n"),
            "{text}"
        );
    }

    #[test]
    fn times_are_shown_in_the_local_zone() {
        assert_eq!(parse_offset("+0200"), Some(7200));
        assert_eq!(parse_offset("-0530"), Some(-19_800));
        assert_eq!(parse_offset("0200"), None);
        assert_eq!(parse_offset("+02"), None);
        assert_eq!(clock(7200 + 30), "02:00:30");
        assert_eq!(clock(-60), "23:59:00");
        assert_eq!(bytes(213), "213 B");
        assert_eq!(bytes(5 << 20), "5.0 MB");
    }
}
