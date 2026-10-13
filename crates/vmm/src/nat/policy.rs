//! Which destinations the guest may reach (design, "Network and the LAN").

use std::net::Ipv4Addr;

use super::{GATEWAY_IP, GUEST_IP, PREFIX_LEN};

/// The network mode a VM runs with. `--net off` has no device at all, so it
/// never reaches the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Outbound only, with private, link-local and shared ranges denied.
    Nat,
    /// As `Nat`, but private, link-local and shared ranges are allowed
    /// (`--net lan`).
    Lan,
}

/// Where a guest connection goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Out through a host socket.
    Forward,
    /// To the gateway itself, which serves DNS and the daemon.
    Gateway,
    /// Refused, with the reason the audit log records, such as `LAN`.
    Deny(&'static str),
}

/// The network policy for one VM.
#[derive(Debug, Clone)]
pub struct Policy {
    mode: Mode,
    /// The Mac's own addresses, on every interface.
    host_addrs: Vec<Ipv4Addr>,
}

/// Ranges denied in [`Mode::Nat`] only, with their audit reasons.
const PRIVATE: [(Ipv4Addr, u8, &str); 5] = [
    (Ipv4Addr::new(10, 0, 0, 0), 8, "LAN"),
    (Ipv4Addr::new(172, 16, 0, 0), 12, "LAN"),
    (Ipv4Addr::new(192, 168, 0, 0), 16, "LAN"),
    (Ipv4Addr::new(169, 254, 0, 0), 16, "link-local"),
    (Ipv4Addr::new(100, 64, 0, 0), 10, "shared address space"),
];

/// Ranges denied in every mode. Loopback would reach the Mac's own
/// services, which only `-p` forwards may expose.
const ALWAYS: [(Ipv4Addr, u8, &str); 4] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8, "reserved"),
    (Ipv4Addr::new(127, 0, 0, 0), 8, "the Mac"),
    (Ipv4Addr::new(224, 0, 0, 0), 4, "multicast"),
    (Ipv4Addr::new(240, 0, 0, 0), 4, "reserved"),
];

fn within(addr: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(addr) & mask == u32::from(net) & mask
}

impl Policy {
    /// A policy for `mode`. `host_addrs` are the Mac's own addresses, which
    /// are denied in every mode.
    pub fn new(mode: Mode, host_addrs: Vec<Ipv4Addr>) -> Policy {
        Policy { mode, host_addrs }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Where a guest connection to `addr` goes.
    pub fn check(&self, addr: Ipv4Addr) -> Verdict {
        if addr == GATEWAY_IP {
            return Verdict::Gateway;
        }
        if within(addr, GUEST_IP, PREFIX_LEN) {
            // Nothing else lives on the guest's link.
            return Verdict::Deny("LAN");
        }
        if self.host_addrs.contains(&addr) {
            return Verdict::Deny("the Mac");
        }
        if let Some(&(_, _, reason)) = ALWAYS.iter().find(|(n, p, _)| within(addr, *n, *p)) {
            return Verdict::Deny(reason);
        }
        if self.mode == Mode::Nat
            && let Some(&(_, _, reason)) = PRIVATE.iter().find(|(n, p, _)| within(addr, *n, *p))
        {
            return Verdict::Deny(reason);
        }
        Verdict::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn nat_denies_each_private_range_at_both_ends() {
        let nat = Policy::new(Mode::Nat, Vec::new());
        for (first, last, reason) in [
            ("10.0.0.0", "10.255.255.255", "LAN"),
            ("172.16.0.0", "172.31.255.255", "LAN"),
            ("192.168.0.0", "192.168.255.255", "LAN"),
            ("169.254.0.0", "169.254.255.255", "link-local"),
            ("100.64.0.0", "100.127.255.255", "shared address space"),
        ] {
            assert_eq!(nat.check(ip(first)), Verdict::Deny(reason), "{first}");
            assert_eq!(nat.check(ip(last)), Verdict::Deny(reason), "{last}");
        }
        // Just outside each range.
        for outside in [
            "9.255.255.255",
            "11.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "169.253.255.255",
            "100.63.255.255",
            "100.128.0.0",
        ] {
            assert_eq!(nat.check(ip(outside)), Verdict::Forward, "{outside}");
        }
    }

    #[test]
    fn public_addresses_are_forwarded() {
        let nat = Policy::new(Mode::Nat, Vec::new());
        assert_eq!(nat.check(ip("1.1.1.1")), Verdict::Forward);
        assert_eq!(nat.check(ip("140.82.112.3")), Verdict::Forward);
    }

    #[test]
    fn lan_mode_allows_private_ranges() {
        let lan = Policy::new(Mode::Lan, Vec::new());
        for addr in [
            "10.1.2.3",
            "172.20.0.1",
            "192.168.1.10",
            "169.254.1.1",
            "100.100.1.1",
        ] {
            assert_eq!(lan.check(ip(addr)), Verdict::Forward, "{addr}");
        }
    }

    #[test]
    fn the_mac_itself_is_denied_in_every_mode() {
        let addrs = vec![ip("192.168.1.20"), ip("203.0.113.7")];
        for mode in [Mode::Nat, Mode::Lan] {
            let policy = Policy::new(mode, addrs.clone());
            assert_eq!(policy.check(ip("203.0.113.7")), Verdict::Deny("the Mac"));
            assert_eq!(policy.check(ip("192.168.1.20")), Verdict::Deny("the Mac"));
            assert_eq!(policy.check(ip("127.0.0.1")), Verdict::Deny("the Mac"));
            assert_eq!(policy.check(ip("127.8.9.10")), Verdict::Deny("the Mac"));
        }
    }

    #[test]
    fn the_gateway_is_an_exception_and_the_rest_of_its_subnet_is_not() {
        for mode in [Mode::Nat, Mode::Lan] {
            let policy = Policy::new(mode, Vec::new());
            assert_eq!(policy.check(GATEWAY_IP), Verdict::Gateway);
            assert_eq!(policy.check(ip("192.168.127.50")), Verdict::Deny("LAN"));
        }
    }

    #[test]
    fn reserved_and_multicast_are_denied_in_every_mode() {
        for mode in [Mode::Nat, Mode::Lan] {
            let policy = Policy::new(mode, Vec::new());
            assert_eq!(policy.check(ip("0.0.0.0")), Verdict::Deny("reserved"));
            assert_eq!(policy.check(ip("224.0.0.251")), Verdict::Deny("multicast"));
            assert_eq!(
                policy.check(ip("255.255.255.255")),
                Verdict::Deny("reserved")
            );
        }
    }
}
