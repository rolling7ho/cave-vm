//! Keeps the guest's clock and timezone on the Mac's. The VM has no clock
//! of its own: the guest agent sets the time it is sent at boot and after
//! a restore, and [`keep`] sends it again whenever the Mac's clock or
//! timezone moves, such as after the Mac slept with the VM running.

use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cave_vm_proto::Data;
use cave_vm_proto::agent::{Message, Timezone};

/// The Mac's timezone setting, a link into its zone files.
const LOCALTIME: &str = "/etc/localtime";

/// The largest zone file sent; real ones are a few kilobytes.
const MAX_TZIF: u64 = 64 * 1024;

/// How often [`keep`] compares the clocks.
const CHECK: Duration = Duration::from_secs(5);

/// A move of the Mac's clock against its monotonic one that is sent at
/// once.
const JUMP: Duration = Duration::from_secs(1);

/// How often the time is sent anyway, to correct the guest's drift.
const REFRESH: Duration = Duration::from_secs(10 * 60);

/// The Mac's timezone, or None when it cannot be read.
pub fn mac_timezone() -> Option<Timezone> {
    timezone_at(Path::new(LOCALTIME))
}

/// The timezone that `link` names: the part of its target after
/// `zoneinfo/`, and the zone file it points to.
fn timezone_at(link: &Path) -> Option<Timezone> {
    let target = fs::read_link(link).ok()?;
    let target = target.to_str()?;
    let name = &target[target.rfind("zoneinfo/")? + "zoneinfo/".len()..];
    if !valid_name(name) || fs::metadata(link).ok()?.len() > MAX_TZIF {
        return None;
    }
    let tzif = fs::read(link).ok()?;
    tzif.starts_with(b"TZif").then(|| Timezone {
        name: name.to_string(),
        tzif: Data(tzif),
    })
}

/// A zone name such as `America/Argentina/Buenos_Aires` or `Etc/GMT+8`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
        })
}

/// The message that sets the guest's clock and timezone to the Mac's now.
pub fn message() -> io::Result<Message> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(Message::Restored {
        time_unix_ns: u64::try_from(now.as_nanos()).map_err(io::Error::other)?,
        timezone: mac_timezone(),
    })
}

/// How far the Mac's clock is from its monotonic clock, which stops while
/// the Mac sleeps, in nanoseconds since `start`.
fn offset(start: (SystemTime, Instant)) -> i128 {
    let wall = match SystemTime::now().duration_since(start.0) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    wall - start.1.elapsed().as_nanos() as i128
}

/// Whether to send the time again: the Mac's clock jumped, its timezone
/// changed, or [`REFRESH`] passed.
fn due(jumped: i128, zone_changed: bool, since_sent: Duration) -> bool {
    jumped.unsigned_abs() >= JUMP.as_nanos() || zone_changed || since_sent >= REFRESH
}

/// Sends the time to the guest again whenever [`due`], until `done`.
/// `send` opens a control connection and delivers a message; a failure is
/// tried again at the next check.
pub fn keep(send: impl Fn(&Message) -> io::Result<()>, done: &AtomicBool) {
    let start = (SystemTime::now(), Instant::now());
    let mut sent_offset = offset(start);
    let mut sent_zone = mac_timezone().map(|z| z.name);
    let mut sent_at = Instant::now();
    while !done.load(Ordering::Acquire) {
        thread::sleep(CHECK);
        let now_offset = offset(start);
        let zone = mac_timezone();
        let zone_changed = zone.as_ref().map(|z| &z.name) != sent_zone.as_ref();
        if !due(now_offset - sent_offset, zone_changed, sent_at.elapsed()) {
            continue;
        }
        if done.load(Ordering::Acquire) {
            return;
        }
        let sent = message().and_then(|m| send(&m));
        if sent.is_ok() {
            sent_offset = now_offset;
            sent_zone = zone.map(|z| z.name);
            sent_at = Instant::now();
        }
    }
}

/// The send for [`keep`]: one control connection from `connect` per
/// message, which the agent answers with `ok`.
pub fn send_over(
    connect: impl Fn() -> io::Result<UnixStream>,
) -> impl Fn(&Message) -> io::Result<()> {
    move |message| crate::control::send_clock(connect()?, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_names_are_checked() {
        for good in [
            "Asia/Manila",
            "UTC",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+8",
            "America/Port-au-Prince",
        ] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "../etc/passwd",
            "Asia//Manila",
            "/UTC",
            "A b",
            "Asia/.x",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_timezone_comes_from_the_link_target() {
        let dir = std::env::temp_dir().join(format!("cave-clock-{}", std::process::id()));
        let zones = dir.join("zoneinfo/Asia");
        fs::create_dir_all(&zones).unwrap();
        fs::write(zones.join("Manila"), b"TZif2 fake zone").unwrap();
        fs::write(zones.join("Bad"), b"not a zone").unwrap();
        let link = dir.join("localtime");
        std::os::unix::fs::symlink(zones.join("Manila"), &link).unwrap();
        let zone = timezone_at(&link).unwrap();
        assert_eq!(zone.name, "Asia/Manila");
        assert_eq!(zone.tzif, Data(b"TZif2 fake zone".to_vec()));

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(zones.join("Bad"), &link).unwrap();
        assert!(timezone_at(&link).is_none());
        assert!(timezone_at(&dir.join("missing")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_time_is_sent_after_a_jump_a_zone_change_or_a_while() {
        let second = 1_000_000_000;
        assert!(!due(0, false, Duration::from_secs(60)));
        assert!(!due(second / 2, false, Duration::ZERO));
        assert!(due(2 * second, false, Duration::ZERO));
        assert!(due(-2 * second, false, Duration::ZERO));
        assert!(due(0, true, Duration::ZERO));
        assert!(due(0, false, REFRESH));
    }

    #[test]
    fn the_mac_timezone_is_read_here() {
        // On a Mac /etc/localtime is a link into the zone files.
        if let Some(zone) = mac_timezone() {
            assert!(valid_name(&zone.name), "{}", zone.name);
        }
    }
}
