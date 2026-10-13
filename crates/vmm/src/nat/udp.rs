//! Outbound UDP and DNS through host sockets.
//!
//! The gateway handles UDP itself rather than through smoltcp: each flow,
//! a guest port and a destination, gets a connected host socket, and a
//! reader thread hands back what arrives. Datagrams to the gateway's port
//! 53 go to the Mac's DNS servers, and the replies come back from the
//! gateway, so the guest needs no route to the real servers.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Packet,
    Ipv4Repr, UdpPacket, UdpRepr,
};

use super::{Event, GATEWAY_IP, GATEWAY_MAC, GUEST_IP, GUEST_MAC, MTU};

/// The DNS port on the gateway.
pub const DNS_PORT: u16 = 53;
/// A flow with no datagrams either way for this long is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Most open flows. A datagram that would open another is dropped.
const MAX_FLOWS: usize = 1024;
/// How often a reader thread checks whether its flow was closed.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// The largest UDP payload that fits in one unfragmented frame.
const MAX_PAYLOAD: usize = MTU - 20 - 8;

/// A connected host datagram socket.
pub trait HostDatagram: Send + Sync {
    fn send(&self, data: &[u8]) -> io::Result<usize>;
    /// Waits for one datagram, up to a read timeout set when the socket was
    /// opened. `Ok(None)` means the timeout passed.
    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>>;
}

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl HostDatagram for UdpSocket {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        UdpSocket::send(self, data)
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match UdpSocket::recv(self, buf) {
            Ok(n) => Ok(Some(n)),
            Err(e) if timed_out(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl HostDatagram for UnixDatagram {
    fn send(&self, data: &[u8]) -> io::Result<usize> {
        UnixDatagram::send(self, data)
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match UnixDatagram::recv(self, buf) {
            Ok(n) => Ok(Some(n)),
            Err(e) if timed_out(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Opens host datagram sockets for the guest.
pub trait UdpConnector: Send + Sync {
    /// A socket connected to `dst`, with a read timeout of about a second.
    fn open(&self, dst: SocketAddrV4) -> io::Result<Arc<dyn HostDatagram>>;
}

/// Real UDP sockets.
#[derive(Debug, Clone, Default)]
pub struct HostUdp;

impl UdpConnector for HostUdp {
    fn open(&self, dst: SocketAddrV4) -> io::Result<Arc<dyn HostDatagram>> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect(dst)?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        Ok(Arc::new(socket))
    }
}

/// The IPv4 `nameserver` lines of a resolv.conf, on port 53.
pub fn parse_resolv_conf(text: &str) -> Vec<SocketAddrV4> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("nameserver"))
                .then(|| words.next()?.parse::<Ipv4Addr>().ok())
                .flatten()
        })
        .map(|ip| SocketAddrV4::new(ip, DNS_PORT))
        .collect()
}

/// The Mac's DNS servers, from `/etc/resolv.conf`, which macOS keeps in
/// step with the primary network service. Empty if it cannot be read.
pub fn host_dns_servers() -> Vec<SocketAddrV4> {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|text| parse_resolv_conf(&text))
        .unwrap_or_default()
}

/// A UDP flow as the guest sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub guest_port: u16,
    /// Where the guest sent it: a remote address, or the gateway's DNS port.
    pub dst: SocketAddrV4,
}

/// What reader threads report.
pub enum HostEvent {
    Datagram(FlowKey, Vec<u8>),
    /// The host socket failed; the flow is dropped.
    Closed(FlowKey),
}

struct Flow {
    socket: Arc<dyn HostDatagram>,
    closed: Arc<AtomicBool>,
    last_active: Duration,
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A datagram the guest sent.
pub struct Datagram<'a> {
    pub key: FlowKey,
    pub payload: &'a [u8],
}

/// Picks the UDP datagram out of an IPv4 frame from the guest. Fragments
/// and malformed packets give `None`.
pub fn parse(frame: &[u8]) -> Option<Datagram<'_>> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp || ip.more_frags() || ip.frag_offset() != 0 {
        return None;
    }
    let dst_ip = ip.dst_addr();
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    let payload_len = usize::from(udp.len()).checked_sub(8)?;
    let payload = udp.payload().get(..payload_len)?;
    Some(Datagram {
        key: FlowKey {
            guest_port: udp.src_port(),
            dst: SocketAddrV4::new(dst_ip, udp.dst_port()),
        },
        payload,
    })
}

/// The frame that delivers `payload` to the guest from `from`.
pub fn frame_to_guest(from: SocketAddrV4, guest_port: u16, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let udp = UdpRepr {
        src_port: from.port(),
        dst_port: guest_port,
    };
    let ip = Ipv4Repr {
        src_addr: *from.ip(),
        dst_addr: GUEST_IP,
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: EthernetAddress(GUEST_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let caps = ChecksumCapabilities::default();
    let mut frame = vec![0; eth.buffer_len() + ip.buffer_len() + ip.payload_len];
    let mut eth_frame = EthernetFrame::new_unchecked(&mut frame);
    eth.emit(&mut eth_frame);
    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip.emit(&mut ip_packet, &caps);
    let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
    udp.emit(
        &mut udp_packet,
        &(*from.ip()).into(),
        &GUEST_IP.into(),
        payload.len(),
        |buf| buf.copy_from_slice(payload),
        &caps,
    );
    Some(frame)
}

/// Open UDP flows.
#[derive(Default)]
pub struct Flows {
    flows: HashMap<FlowKey, Flow>,
}

impl Flows {
    pub fn contains(&self, key: &FlowKey) -> bool {
        self.flows.contains_key(key)
    }

    /// Sends a guest datagram on its flow, opening the flow on `upstream`
    /// first if it is new. Returns `false` if it was dropped.
    pub fn send(
        &mut self,
        key: FlowKey,
        upstream: SocketAddrV4,
        payload: &[u8],
        now: Duration,
        connector: &Arc<dyn UdpConnector>,
        events: &Sender<Event>,
    ) -> bool {
        if !self.flows.contains_key(&key) {
            if self.flows.len() >= MAX_FLOWS {
                return false;
            }
            let Ok(socket) = connector.open(upstream) else {
                return false;
            };
            let closed = Arc::new(AtomicBool::new(false));
            spawn_reader(
                key,
                Arc::clone(&socket),
                Arc::clone(&closed),
                events.clone(),
            );
            self.flows.insert(
                key,
                Flow {
                    socket,
                    closed,
                    last_active: now,
                },
            );
        }
        let flow = self.flows.get_mut(&key).expect("inserted above");
        flow.last_active = now;
        // A send the host refuses is dropped, like a lost datagram.
        flow.socket.send(payload).is_ok()
    }

    /// Acts on one host event. Returns the frame to give the guest.
    pub fn host_event(&mut self, event: HostEvent, now: Duration) -> Option<Vec<u8>> {
        match event {
            HostEvent::Datagram(key, data) => {
                let flow = self.flows.get_mut(&key)?;
                flow.last_active = now;
                frame_to_guest(key.dst, key.guest_port, &data)
            }
            HostEvent::Closed(key) => {
                self.flows.remove(&key);
                None
            }
        }
    }

    /// Closes flows idle for [`IDLE_TIMEOUT`].
    pub fn expire(&mut self, now: Duration) {
        self.flows
            .retain(|_, flow| now.saturating_sub(flow.last_active) < IDLE_TIMEOUT);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.flows.len()
    }
}

fn spawn_reader(
    key: FlowKey,
    socket: Arc<dyn HostDatagram>,
    closed: Arc<AtomicBool>,
    events: Sender<Event>,
) {
    thread::spawn(move || {
        let mut buf = vec![0u8; 65_536];
        while !closed.load(Ordering::Acquire) {
            let event = match socket.recv(&mut buf) {
                Ok(None) => continue,
                Ok(Some(n)) => HostEvent::Datagram(key, buf[..n].to_vec()),
                Err(_) => HostEvent::Closed(key),
            };
            let last = matches!(event, HostEvent::Closed(_));
            if events.send(Event::Udp(event)).is_err() || last {
                return;
            }
        }
    });
}

/// Whether `key` is a DNS query to the gateway.
pub fn is_dns(key: &FlowKey) -> bool {
    *key.dst.ip() == GATEWAY_IP && key.dst.port() == DNS_PORT
}

#[cfg(test)]
mod tests;
