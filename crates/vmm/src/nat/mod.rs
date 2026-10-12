//! The userspace network stack behind the virtio-net device (design, "NAT
//! implementation"): the guest sees one Ethernet link with a gateway on it,
//! and the gateway is this module.
//!
//! The guest has a fixed address, [`GUEST_IP`], set on the kernel command
//! line ([`kernel_ip_arg`]), so a boot needs no DHCP round trip and a
//! restored or forked VM keeps its address. The subnet is the one podman's
//! gvproxy uses, picked because it rarely collides with home and office
//! LANs.
//!
//! This part answers ARP for the gateway and pings to it, and drops frames
//! the guest sends with an address that is not its own. Outbound TCP, UDP
//! and DNS through host sockets come next (ROADMAP P3.7c and P3.7d), always
//! together with the network policy (P3.7e).

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpPacket, EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpCidr,
    Ipv4Packet,
};

/// The guest's hardware address, which the virtio-net device reports.
pub const GUEST_MAC: [u8; 6] = [0x02, 0xca, 0x5e, 0x00, 0x00, 0x02];
/// The gateway's hardware address.
pub const GATEWAY_MAC: [u8; 6] = [0x02, 0xca, 0x5e, 0x00, 0x00, 0x01];
/// The guest's IPv4 address.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
/// The gateway's IPv4 address. It is also the guest's DNS server.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
/// Prefix length of the guest's subnet.
pub const PREFIX_LEN: u8 = 24;
/// The link MTU. No offloads are offered, so frames never exceed it.
pub const MTU: usize = 1500;

/// Frames from the guest waiting for the stack. Beyond this they are
/// dropped, as a switch would.
const BACKLOG: usize = 256;

/// The kernel `ip=` argument that configures the guest's address, gateway
/// and DNS server on `eth0` without DHCP (kernel `CONFIG_IP_PNP`).
pub fn kernel_ip_arg() -> String {
    let mask = Ipv4Addr::from(u32::MAX << (32 - PREFIX_LEN));
    format!("ip={GUEST_IP}::{GATEWAY_IP}:{mask}::eth0:off:{GATEWAY_IP}")
}

/// The gateway side of the guest's link.
pub struct Stack {
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
}

impl Stack {
    /// A stack whose clock starts at `now` (time since any fixed point, as
    /// long as later calls use the same one).
    pub fn new(now: Duration) -> Stack {
        let mut link = Link::default();
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut iface = Interface::new(config, &mut link, instant(now));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(GATEWAY_IP.into(), PREFIX_LEN))
                .expect("one address fits");
        });
        Stack {
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
        }
    }

    /// Takes one frame the guest sent. Returns `false` if it was dropped:
    /// malformed, not from the guest's own hardware or IP address, not IPv4
    /// or ARP, or over the backlog.
    pub fn from_guest(&mut self, frame: &[u8]) -> bool {
        if self.link.from_guest.len() >= BACKLOG || !from_guest_address(frame) {
            return false;
        }
        self.link.from_guest.push_back(frame.to_vec());
        true
    }

    /// Processes waiting frames and timers, and returns the frames to give
    /// the guest.
    pub fn poll(&mut self, now: Duration) -> Vec<Vec<u8>> {
        self.iface
            .poll(instant(now), &mut self.link, &mut self.sockets);
        std::mem::take(&mut self.link.to_guest)
    }

    /// How long until [`poll`](Self::poll) has timer work, if any.
    pub fn poll_delay(&mut self, now: Duration) -> Option<Duration> {
        self.iface
            .poll_delay(instant(now), &self.sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
    }
}

fn instant(now: Duration) -> Instant {
    Instant::from_micros(i64::try_from(now.as_micros()).unwrap_or(i64::MAX))
}

/// Whether `frame` is an ARP or IPv4 frame from the guest's own hardware and
/// IP address, addressed to the gateway or broadcast.
fn from_guest_address(frame: &[u8]) -> bool {
    let Ok(eth) = EthernetFrame::new_checked(frame) else {
        return false;
    };
    let dst = eth.dst_addr();
    if eth.src_addr() != EthernetAddress(GUEST_MAC)
        || (dst != EthernetAddress(GATEWAY_MAC) && !dst.is_broadcast())
    {
        return false;
    }
    match eth.ethertype() {
        EthernetProtocol::Arp => ArpPacket::new_checked(eth.payload()).is_ok_and(|arp| {
            arp.source_hardware_addr() == GUEST_MAC
                && arp.source_protocol_addr() == GUEST_IP.octets()
        }),
        EthernetProtocol::Ipv4 => {
            Ipv4Packet::new_checked(eth.payload()).is_ok_and(|ip| ip.src_addr() == GUEST_IP)
        }
        _ => false,
    }
}

/// The link as smoltcp sees it: frames queued from the guest, and frames
/// collected for it.
#[derive(Default)]
struct Link {
    from_guest: VecDeque<Vec<u8>>,
    to_guest: Vec<Vec<u8>>,
}

struct RxToken(Vec<u8>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct TxToken<'a>(&'a mut Vec<Vec<u8>>);

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let r = f(&mut frame);
        self.0.push(frame);
        r
    }
}

impl Device for Link {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.from_guest.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.to_guest)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.to_guest))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU + 14;
        caps
    }
}

#[cfg(test)]
mod tests;
