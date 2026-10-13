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
//! The gateway answers ARP for itself and pings to it, and drops frames the
//! guest sends with an address that is not its own. Outbound TCP goes
//! through host sockets ([`tcp`]) for destinations the [`Policy`] allows;
//! anything else gets a reset. UDP and DNS come next (ROADMAP P3.7d).

mod host;
mod policy;
mod tcp;

pub use host::{Connector, HostStream, TcpConnector};
pub use policy::{Mode, Policy, Verdict};

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpPacket, EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpCidr,
    IpProtocol, Ipv4Packet, TcpPacket,
};

use tcp::{FlowKey, Flows, HostEvent};

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

/// Rounds of polling and relaying per [`Stack::poll`], so a busy flow cannot
/// keep it from returning.
const RELAY_ROUNDS: usize = 8;

/// A connection attempt by the guest, for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectEvent {
    pub dst: SocketAddrV4,
    pub allowed: bool,
    /// Why it was denied, such as `LAN`.
    pub reason: Option<&'static str>,
}

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
    policy: Policy,
    connector: Arc<dyn Connector>,
    flows: Flows,
    events: Sender<HostEvent>,
    host_events: Receiver<HostEvent>,
    /// An event [`wait`](Self::wait) took off the channel.
    waiting: Option<HostEvent>,
    connects: Vec<ConnectEvent>,
}

impl Stack {
    /// A stack whose clock starts at `now` (time since any fixed point, as
    /// long as later calls use the same one). Guest connections go out
    /// through `connector` where `policy` allows.
    pub fn new(now: Duration, policy: Policy, connector: Arc<dyn Connector>) -> Stack {
        let mut link = Link::default();
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut iface = Interface::new(config, &mut link, instant(now));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(GATEWAY_IP.into(), PREFIX_LEN))
                .expect("one address fits");
        });
        // Take packets for any destination routed through the gateway, so
        // a socket can stand in for each remote end.
        iface.set_any_ip(true);
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP)
            .expect("one route fits");
        let (events, host_events) = mpsc::channel();
        Stack {
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
            policy,
            connector,
            flows: Flows::default(),
            events,
            host_events,
            waiting: None,
            connects: Vec::new(),
        }
    }

    /// Takes one frame the guest sent. Returns `false` if it was dropped:
    /// malformed, not from the guest's own hardware or IP address, not IPv4
    /// or ARP, or over the backlog.
    ///
    /// A TCP SYN for a new connection is checked against the policy. If it
    /// is allowed, it is held while the host connects; if not, the guest
    /// gets a reset.
    pub fn from_guest(&mut self, frame: &[u8]) -> bool {
        if self.link.from_guest.len() >= BACKLOG || !from_guest_address(frame) {
            return false;
        }
        match classify(frame) {
            Packet::Other => {}
            Packet::NotForGateway => return false,
            Packet::Syn(key) => {
                if self.flows.contains(&key) {
                    // A retransmitted SYN while connecting, or one smoltcp
                    // handles for an open flow.
                    if self.flows.is_connecting(&key) {
                        return true;
                    }
                } else {
                    match self.policy.check(*key.dst.ip()) {
                        Verdict::Forward => {
                            self.connects.push(ConnectEvent {
                                dst: key.dst,
                                allowed: true,
                                reason: None,
                            });
                            self.flows
                                .connect(key, frame.to_vec(), &self.connector, &self.events);
                            return true;
                        }
                        Verdict::Deny(reason) => self.connects.push(ConnectEvent {
                            dst: key.dst,
                            allowed: false,
                            reason: Some(reason),
                        }),
                        // Nothing listens on the gateway yet.
                        Verdict::Gateway => {}
                    }
                }
            }
        }
        self.link.from_guest.push_back(frame.to_vec());
        true
    }

    /// Waits up to `timeout` for a host connection to have news, so a
    /// caller's loop can sleep between polls.
    pub fn wait(&mut self, timeout: Duration) {
        if self.waiting.is_some() {
            return;
        }
        match self.host_events.recv_timeout(timeout) {
            Ok(event) => self.waiting = Some(event),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    /// Processes waiting frames, host events and timers, and returns the
    /// frames to give the guest.
    pub fn poll(&mut self, now: Duration) -> Vec<Vec<u8>> {
        let waiting = self.waiting.take();
        for event in waiting.into_iter().chain(self.host_events.try_iter()) {
            if let Some(syn) = self
                .flows
                .host_event(event, &mut self.sockets, &self.events)
            {
                self.link.from_guest.push_back(syn);
            }
        }
        for _ in 0..RELAY_ROUNDS {
            self.iface
                .poll(instant(now), &mut self.link, &mut self.sockets);
            if !self.flows.relay(&mut self.sockets) {
                break;
            }
        }
        std::mem::take(&mut self.link.to_guest)
    }

    /// Connection attempts since the last call, for the audit log.
    pub fn take_connects(&mut self) -> Vec<ConnectEvent> {
        std::mem::take(&mut self.connects)
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

enum Packet {
    /// The opening SYN of a TCP connection.
    Syn(FlowKey),
    /// ICMP for somewhere other than the gateway, which smoltcp would
    /// answer as if it were the destination.
    NotForGateway,
    Other,
}

/// Picks out what the gateway treats specially. `frame` has passed
/// [`from_guest_address`].
fn classify(frame: &[u8]) -> Packet {
    let eth = EthernetFrame::new_unchecked(frame);
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return Packet::Other;
    }
    let ip = Ipv4Packet::new_unchecked(eth.payload());
    let dst = ip.dst_addr();
    match ip.next_header() {
        IpProtocol::Icmp if dst != GATEWAY_IP => Packet::NotForGateway,
        IpProtocol::Tcp => match TcpPacket::new_checked(ip.payload()) {
            Ok(tcp) if tcp.syn() && !tcp.ack() => Packet::Syn(FlowKey {
                guest_port: tcp.src_port(),
                dst: SocketAddrV4::new(dst, tcp.dst_port()),
            }),
            _ => Packet::Other,
        },
        _ => Packet::Other,
    }
}

/// Whether `frame` is an IPv4 frame, or an ARP request for the gateway,
/// from the guest's own hardware and IP address, addressed to the gateway
/// or broadcast.
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
        // Only ARP for the gateway: smoltcp, taking any address, would
        // otherwise claim every address on the link.
        EthernetProtocol::Arp => ArpPacket::new_checked(eth.payload()).is_ok_and(|arp| {
            arp.source_hardware_addr() == GUEST_MAC
                && arp.source_protocol_addr() == GUEST_IP.octets()
                && arp.target_protocol_addr() == GATEWAY_IP.octets()
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
