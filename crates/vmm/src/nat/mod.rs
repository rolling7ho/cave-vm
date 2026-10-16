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
//! guest sends with an address that is not its own. Outbound TCP ([`tcp`])
//! and UDP ([`udp`]) go through host sockets for destinations the
//! [`Policy`] allows; a denied TCP connection gets a reset and a denied
//! datagram is dropped. DNS queries to the gateway go to the Mac's DNS
//! servers.
//!
//! Host connections end with the VMM process. After a restore the guest
//! still has its TCP connections, so the new stack is told of the ones
//! that were open ([`Stack::open_flows`] at the snapshot) and makes the
//! guest reset them ([`Stack::reset_guest_flows`]), so programs see an
//! error instead of waiting for data that will never come. A guest that
//! sends on a connection the stack does not know gets a reset anyway.

mod connects;
mod host;
mod policy;
mod tcp;
mod udp;

pub use connects::Connects;
pub use host::{Connector, HostStream, TcpConnector};
pub use policy::{Mode, Policy, Verdict};
pub use tcp::FlowKey;
pub use udp::{
    DNS_PORT, HostDatagram, HostUdp, IDLE_TIMEOUT, UdpConnector, host_dns_servers,
    parse_resolv_conf,
};

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, HardwareAddress, IpCidr, IpProtocol, Ipv4Packet, TcpPacket,
};

use tcp::{Flows, HostEvent};

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

/// Most denied UDP flows remembered as reported. Past it, a denied flow
/// not yet reported is denied without an audit entry, so a guest sending
/// to ever new ports cannot grow the map without end.
pub const MAX_UDP_DENIED: usize = 4096;

/// The longest [`Stack::run`] sleeps without news, so idle flows still
/// expire.
const IDLE_WAIT: Duration = Duration::from_secs(1);

/// What host threads report to the gateway.
pub(crate) enum Event {
    Tcp(HostEvent),
    Udp(udp::HostEvent),
    /// A frame the guest sent, through [`GuestFrames`].
    Guest(Vec<u8>),
    /// [`GuestFrames::close`]: [`Stack::run`] returns.
    Close,
}

/// Hands frames the guest sends to a [`Stack`] running on its own thread
/// ([`Stack::run`]), waking it. Never blocks, so a vCPU thread can call it.
#[derive(Clone)]
pub struct GuestFrames {
    events: Sender<Event>,
    pending: Arc<AtomicUsize>,
}

impl GuestFrames {
    /// Queues one frame. Returns `false` if it was dropped because the
    /// stack is [`BACKLOG`] frames behind, or has stopped.
    pub fn send(&self, frame: &[u8]) -> bool {
        if self.pending.fetch_add(1, Ordering::AcqRel) >= BACKLOG {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        self.events.send(Event::Guest(frame.to_vec())).is_ok()
    }

    /// Makes [`Stack::run`] return.
    pub fn close(&self) {
        let _ = self.events.send(Event::Close);
    }
}

/// The guest's open TCP connections, kept up to date by the stack, for a
/// snapshot taken from another thread. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct OpenFlows(Arc<Mutex<Vec<FlowKey>>>);

impl OpenFlows {
    pub fn get(&self) -> Vec<FlowKey> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, flows: Vec<FlowKey>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = flows;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// A connection attempt by the guest, for the audit log. For UDP, the
/// first datagram of a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectEvent {
    pub protocol: Protocol,
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
    udp_connector: Option<Arc<dyn UdpConnector>>,
    dns: Vec<SocketAddrV4>,
    udp: udp::Flows,
    /// Denied UDP flows already reported, and when, so a stream of
    /// datagrams is reported once per [`IDLE_TIMEOUT`].
    udp_denied: HashMap<udp::FlowKey, Duration>,
    events: Sender<Event>,
    host_events: Receiver<Event>,
    /// Guest frames queued through [`GuestFrames`] and not yet taken.
    pending: Arc<AtomicUsize>,
    /// Set by [`Event::Close`].
    closed: bool,
    /// An event [`wait`](Self::wait) took off the channel.
    waiting: Option<Event>,
    connects: Vec<ConnectEvent>,
    /// The time of the latest [`poll`](Self::poll).
    now: Duration,
    open_flows: OpenFlows,
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
            udp_connector: None,
            dns: Vec::new(),
            udp: udp::Flows::default(),
            udp_denied: HashMap::new(),
            events,
            host_events,
            pending: Arc::default(),
            closed: false,
            waiting: None,
            connects: Vec::new(),
            now,
            open_flows: OpenFlows::default(),
        }
    }

    /// The guest's open TCP connections, updated on every poll.
    pub fn open_flows(&self) -> OpenFlows {
        self.open_flows.clone()
    }

    /// Makes the guest reset each of `flows`, its connections from before
    /// a restore. Each gets segments the guest cannot accept, so it
    /// answers with an acknowledgment carrying its sequence numbers; no
    /// socket here knows the connection, so that is answered with a reset
    /// the guest accepts.
    ///
    /// The guest, which already knows the gateway, sends no ARP request
    /// first, and a reset to an address the stack has not resolved would
    /// be dropped. So the stack takes the request the guest would have
    /// sent, which teaches it the guest's hardware address.
    pub fn reset_guest_flows(&mut self, flows: &[FlowKey]) {
        self.link.from_guest.push_back(guest_arp_request());
        self.link
            .to_guest
            .extend(flows.iter().flat_map(|&key| tcp::probe_frames(key)));
    }

    /// Lets the guest send UDP through `connector`, and DNS queries to the
    /// gateway on to `dns` (the first server; usually
    /// [`host_dns_servers`]). Without it, UDP is dropped.
    pub fn with_udp(mut self, connector: Arc<dyn UdpConnector>, dns: Vec<SocketAddrV4>) -> Stack {
        self.udp_connector = Some(connector);
        self.dns = dns;
        self
    }

    /// Takes one frame the guest sent. Returns `false` if it was dropped:
    /// malformed, not from the guest's own hardware or IP address, not IPv4
    /// or ARP, or over the backlog.
    ///
    /// A TCP SYN for a new connection is checked against the policy. If it
    /// is allowed, it is held while the host connects; if not, the guest
    /// gets a reset. A UDP datagram is sent on at once, or dropped.
    pub fn from_guest(&mut self, frame: &[u8]) -> bool {
        if self.link.from_guest.len() >= BACKLOG || !from_guest_address(frame) {
            return false;
        }
        if is_udp(frame) {
            return match udp::parse(frame) {
                Some(datagram) => self.udp_from_guest(datagram.key, datagram.payload),
                None => false,
            };
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
                        // smoltcp resets it, as for a denied one.
                        Verdict::Forward if self.flows.is_full() => {}
                        Verdict::Forward => {
                            self.connects.push(ConnectEvent {
                                protocol: Protocol::Tcp,
                                dst: key.dst,
                                allowed: true,
                                reason: None,
                            });
                            self.flows
                                .connect(key, frame.to_vec(), &self.connector, &self.events);
                            return true;
                        }
                        Verdict::Deny(reason) => self.connects.push(ConnectEvent {
                            protocol: Protocol::Tcp,
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

    fn udp_from_guest(&mut self, key: udp::FlowKey, payload: &[u8]) -> bool {
        let Some(connector) = &self.udp_connector else {
            return false;
        };
        let upstream = if self.udp.contains(&key) {
            key.dst
        } else if udp::is_dns(&key) {
            match self.dns.first() {
                Some(server) => *server,
                None => return false,
            }
        } else {
            match self.policy.check(*key.dst.ip()) {
                Verdict::Forward => {
                    self.connects.push(ConnectEvent {
                        protocol: Protocol::Udp,
                        dst: key.dst,
                        allowed: true,
                        reason: None,
                    });
                    key.dst
                }
                Verdict::Deny(reason) => {
                    let now = self.now;
                    let reported = self
                        .udp_denied
                        .get(&key)
                        .is_some_and(|at| now.saturating_sub(*at) < IDLE_TIMEOUT);
                    if !reported && self.udp_denied.len() < MAX_UDP_DENIED {
                        self.udp_denied.insert(key, now);
                        self.connects.push(ConnectEvent {
                            protocol: Protocol::Udp,
                            dst: key.dst,
                            allowed: false,
                            reason: Some(reason),
                        });
                    }
                    return false;
                }
                // Only DNS listens on the gateway.
                Verdict::Gateway => return false,
            }
        };
        self.udp
            .send(key, upstream, payload, self.now, connector, &self.events)
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
        self.now = now;
        let waiting = self.waiting.take();
        let events: Vec<Event> = waiting
            .into_iter()
            .chain(self.host_events.try_iter())
            .collect();
        for event in events {
            match event {
                Event::Guest(frame) => {
                    self.pending.fetch_sub(1, Ordering::AcqRel);
                    self.from_guest(&frame);
                }
                Event::Close => self.closed = true,
                Event::Tcp(event) => {
                    if let Some(syn) = self
                        .flows
                        .host_event(event, &mut self.sockets, &self.events)
                    {
                        self.link.from_guest.push_back(syn);
                    }
                }
                Event::Udp(event) => {
                    if let Some(frame) = self.udp.host_event(event, now) {
                        self.link.to_guest.push(frame);
                    }
                }
            }
        }
        self.udp.expire(now);
        self.udp_denied
            .retain(|_, at| now.saturating_sub(*at) < IDLE_TIMEOUT);
        for _ in 0..RELAY_ROUNDS {
            self.iface
                .poll(instant(now), &mut self.link, &mut self.sockets);
            if !self.flows.relay(&mut self.sockets) {
                break;
            }
        }
        self.open_flows.set(self.flows.open_keys());
        std::mem::take(&mut self.link.to_guest)
    }

    /// A handle for queueing guest frames from other threads while
    /// [`run`](Self::run) drives the stack.
    pub fn guest_frames(&self) -> GuestFrames {
        GuestFrames {
            events: self.events.clone(),
            pending: Arc::clone(&self.pending),
        }
    }

    /// Drives the stack until [`GuestFrames::close`]: takes guest frames,
    /// host news and timers as they come, gives each frame for the guest to
    /// `deliver`, and each connection attempt to `record`. `clock` is the
    /// same clock the stack was created with. Sleeps while nothing happens.
    pub fn run(
        &mut self,
        clock: &dyn Fn() -> Duration,
        deliver: &mut dyn FnMut(&[u8]),
        record: &mut dyn FnMut(ConnectEvent),
    ) {
        while !self.closed {
            for frame in self.poll(clock()) {
                deliver(&frame);
            }
            for event in self.take_connects() {
                record(event);
            }
            if self.closed {
                break;
            }
            let timeout = self.poll_delay(clock()).unwrap_or(IDLE_WAIT);
            self.wait(timeout.min(IDLE_WAIT));
        }
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

/// Whether an IPv4 frame carries UDP. `frame` has passed
/// [`from_guest_address`].
fn is_udp(frame: &[u8]) -> bool {
    let eth = EthernetFrame::new_unchecked(frame);
    eth.ethertype() == EthernetProtocol::Ipv4
        && Ipv4Packet::new_unchecked(eth.payload()).next_header() == IpProtocol::Udp
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

/// The ARP request the guest sends for the gateway.
fn guest_arp_request() -> Vec<u8> {
    let arp = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: GUEST_IP,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: GATEWAY_IP,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    };
    let mut frame = vec![0; eth.buffer_len() + arp.buffer_len()];
    let mut eth_frame = EthernetFrame::new_unchecked(&mut frame);
    eth.emit(&mut eth_frame);
    arp.emit(&mut ArpPacket::new_unchecked(eth_frame.payload_mut()));
    frame
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
