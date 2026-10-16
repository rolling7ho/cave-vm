//! The startup policy: what a VM can reach (docs/cli.md, "The startup
//! policy").

use std::time::Duration;

use cave_vm_proto::daemon::{NetMode, Policy, Storage};

use crate::style::Paint;

/// Column where the widening flags are echoed.
const MARK_COLUMN: usize = 58;

/// `base@sha256:3f2a9c1e...` as `base, sha256:3f2a9c1e`.
fn image(image: &str) -> String {
    match image.split_once("@sha256:") {
        Some((name, digest)) => format!("{name}, sha256:{}", &digest[..digest.len().min(8)]),
        None => image.to_string(),
    }
}

/// One row: the label in bold, and the flags that widened it in yellow.
fn line(paint: Paint, label: &str, text: &str, marks: &[&str]) -> String {
    let plain = format!("  {label:<9} {text}");
    let mut line = format!("  {} {text}", paint.bold(&format!("{label:<9}")));
    if !marks.is_empty() {
        let gap = MARK_COLUMN.max(plain.len() + 2) - plain.len();
        let marks = format!("[{}]", marks.join(" "));
        line = format!("{line}{}{}", " ".repeat(gap), paint.notice(&marks));
    }
    line
}

pub fn gigabytes(bytes: u64) -> String {
    let gb = bytes as f64 / f64::from(1 << 30);
    if gb.fract() == 0.0 {
        format!("{gb:.0} GB")
    } else {
        format!("{gb:.1} GB")
    }
}

fn duration(seconds: u64) -> String {
    match seconds {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// The policy lines. `headline` is what follows the VM's name, such as
/// `started in 0.4s`.
pub fn render(policy: &Policy, headline: &str, paint: Paint) -> String {
    lines(policy, headline, None, paint)
}

/// The policy lines with the disk in use and its size on the resources
/// line, as `cave inspect` shows them.
pub fn render_with_disk(
    policy: &Policy,
    headline: &str,
    used: u64,
    size: u64,
    paint: Paint,
) -> String {
    lines(policy, headline, Some((used, size)), paint)
}

fn lines(policy: &Policy, headline: &str, disk: Option<(u64, u64)>, paint: Paint) -> String {
    let marks = |prefix: &str| -> Vec<&str> {
        policy
            .widened
            .iter()
            .filter(|w| w.starts_with(prefix))
            .map(String::as_str)
            .collect()
    };
    let files = match policy.storage {
        Storage::Sealed => "sealed: nothing on your Mac is reachable from the VM",
        Storage::Synced => {
            "synced: cave keeps a copy of the code on your Mac; the VM cannot reach it"
        }
    };
    let network = match policy.net {
        NetMode::Off => "off: no network",
        NetMode::Nat => "nat: outbound only; LAN ranges and this Mac are blocked",
        NetMode::Lan => "lan: outbound, and LAN ranges are reachable",
    };
    let ports = if policy.ports.is_empty() {
        "none".to_string()
    } else {
        policy
            .ports
            .iter()
            .map(|p| format!("localhost:{} -> {}", p.mac, p.vm))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cpus = if policy.cpus == 1 {
        "1 CPU".to_string()
    } else {
        format!("{} CPUs", policy.cpus)
    };
    let lifetime = policy.ttl_seconds.map_or_else(
        || "no TTL".to_string(),
        |s| format!("removed {} after boot", duration(s)),
    );
    [
        format!(
            "{}: {headline} ({})",
            paint.bold(&policy.name),
            image(&policy.image)
        ),
        line(paint, "files", files, &[]),
        line(paint, "network", network, &marks("--net")),
        line(paint, "ports", &ports, &marks("-p")),
        line(
            paint,
            "resources",
            &match disk {
                None => format!("{cpus}, {} memory", gigabytes(policy.memory_bytes)),
                Some((used, size)) => format!(
                    "{cpus}, {} memory, {} of {} disk used",
                    gigabytes(policy.memory_bytes),
                    gigabytes(used),
                    gigabytes(size)
                ),
            },
            &[],
        ),
        line(paint, "lifetime", &lifetime, &[]),
    ]
    .join("\n")
}

/// `0.4s`, for "started in 0.4s".
pub fn seconds(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cave_vm_proto::daemon::PortMap;

    fn policy() -> Policy {
        Policy {
            name: "my-project".into(),
            image: "base@sha256:3f2a9c1e5d7b".into(),
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

    #[test]
    fn default_policy_matches_the_spec() {
        assert_eq!(
            render(&policy(), "started in 0.4s", Paint::OFF),
            "my-project: started in 0.4s (base, sha256:3f2a9c1e)
  files     sealed: nothing on your Mac is reachable from the VM
  network   nat: outbound only; LAN ranges and this Mac are blocked
  ports     none
  resources 2 CPUs, 2 GB memory
  lifetime  no TTL"
        );
    }

    #[test]
    fn widened_items_are_marked_with_their_flags() {
        let mut p = policy();
        p.net = NetMode::Lan;
        p.ports = vec![PortMap {
            mac: 3000,
            vm: 3000,
        }];
        p.widened = vec!["--net lan".into(), "-p 3000".into()];
        let text = render(&p, "started in 0.4s", Paint::OFF);
        assert!(
            text.contains("  network   lan: outbound, and LAN ranges are reachable   [--net lan]"),
            "{text}"
        );
        assert!(
            text.contains("  ports     localhost:3000 -> 3000                        [-p 3000]"),
            "{text}"
        );
    }

    #[test]
    fn ttl_and_sizes() {
        let mut p = policy();
        p.ttl_seconds = Some(7200);
        p.memory_bytes = 3 << 29;
        p.cpus = 1;
        let text = render(&p, "running", Paint::OFF);
        assert!(text.contains("1 CPU, 1.5 GB memory"), "{text}");
        assert!(text.contains("removed 2h after boot"), "{text}");
    }
}
