use super::*;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpRepr, EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpProtocol, Ipv4Repr,
};

const OTHER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x99];
const OTHER_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 50);
const OUTSIDE_IP: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// A stack whose host connections all fail; TCP tests use [`FakeHost`].
fn stack(mode: Mode) -> Stack {
    Stack::new(
        t(0),
        Policy::new(mode, Vec::new()),
        Arc::new(FakeHost::default()),
    )
}

/// An ARP request asking who has `target`.
fn arp_request(src_mac: [u8; 6], src_ip: Ipv4Addr, target: Ipv4Addr) -> Vec<u8> {
    let arp = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(src_mac),
        source_protocol_addr: src_ip,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: target,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(src_mac),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    };
    let mut frame = vec![0; eth.buffer_len() + arp.buffer_len()];
    let mut packet = EthernetFrame::new_unchecked(&mut frame);
    eth.emit(&mut packet);
    arp.emit(&mut ArpPacket::new_unchecked(packet.payload_mut()));
    frame
}

/// An ICMP echo request from `src` to `dst`, sent to the gateway's MAC.
fn ping(src_mac: [u8; 6], src: Ipv4Addr, dst: Ipv4Addr) -> Vec<u8> {
    let icmp = Icmpv4Repr::EchoRequest {
        ident: 0x1234,
        seq_no: 1,
        data: b"cave ping",
    };
    let ip = Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Icmp,
        payload_len: icmp.buffer_len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(src_mac),
        dst_addr: EthernetAddress(GATEWAY_MAC),
        ethertype: EthernetProtocol::Ipv4,
    };
    let caps = ChecksumCapabilities::default();
    let mut frame = vec![0; eth.buffer_len() + ip.buffer_len() + icmp.buffer_len()];
    let mut packet = EthernetFrame::new_unchecked(&mut frame);
    eth.emit(&mut packet);
    let mut ipv4 = Ipv4Packet::new_unchecked(packet.payload_mut());
    ip.emit(&mut ipv4, &caps);
    icmp.emit(&mut Icmpv4Packet::new_unchecked(ipv4.payload_mut()), &caps);
    frame
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// An ARP request the guest kernel sent for the gateway, recorded from a
/// real boot with this subnet configured.
const RECORDED_ARP: &str =
    "ffffffffffff02ca5e0000020806000108000604000102ca5e000002c0a87f02000000000000c0a87f01";

fn assert_arp_reply(frame: &[u8]) {
    let eth = EthernetFrame::new_checked(frame).unwrap();
    assert_eq!(eth.dst_addr(), EthernetAddress(GUEST_MAC));
    assert_eq!(eth.src_addr(), EthernetAddress(GATEWAY_MAC));
    let arp = ArpRepr::parse(&ArpPacket::new_checked(eth.payload()).unwrap()).unwrap();
    assert_eq!(
        arp,
        ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: EthernetAddress(GATEWAY_MAC),
            source_protocol_addr: GATEWAY_IP,
            target_hardware_addr: EthernetAddress(GUEST_MAC),
            target_protocol_addr: GUEST_IP,
        }
    );
}

#[test]
fn kernel_argument_sets_address_gateway_and_dns() {
    assert_eq!(
        kernel_ip_arg(),
        "ip=192.168.127.2::192.168.127.1:255.255.255.0::eth0:off:192.168.127.1"
    );
}

#[test]
fn recorded_arp_request_for_the_gateway_is_answered() {
    let mut stack = stack(Mode::Nat);
    assert!(stack.from_guest(&hex(RECORDED_ARP)));
    let out = stack.poll(t(1));
    assert_eq!(out.len(), 1);
    assert_arp_reply(&out[0]);
}

#[test]
fn built_arp_request_matches_the_recording() {
    assert_eq!(
        arp_request(GUEST_MAC, GUEST_IP, GATEWAY_IP),
        hex(RECORDED_ARP)
    );
}

#[test]
fn arp_for_other_addresses_is_dropped_unanswered() {
    let mut stack = stack(Mode::Nat);
    assert!(!stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, OTHER_IP)));
    assert!(!stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, OUTSIDE_IP)));
    assert!(stack.poll(t(1)).is_empty());
}

#[test]
fn pings_to_the_gateway_are_answered() {
    let mut stack = stack(Mode::Nat);
    stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, GATEWAY_IP));
    stack.poll(t(1));
    assert!(stack.from_guest(&ping(GUEST_MAC, GUEST_IP, GATEWAY_IP)));
    let out = stack.poll(t(2));
    assert_eq!(out.len(), 1);
    let eth = EthernetFrame::new_checked(&out[0][..]).unwrap();
    assert_eq!(eth.dst_addr(), EthernetAddress(GUEST_MAC));
    let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
    assert_eq!(ip.src_addr(), GATEWAY_IP);
    assert_eq!(ip.dst_addr(), GUEST_IP);
    let icmp = Icmpv4Repr::parse(
        &Icmpv4Packet::new_checked(ip.payload()).unwrap(),
        &ChecksumCapabilities::default(),
    )
    .unwrap();
    assert_eq!(
        icmp,
        Icmpv4Repr::EchoReply {
            ident: 0x1234,
            seq_no: 1,
            data: b"cave ping",
        }
    );
}

#[test]
fn pings_to_the_outside_are_dropped_not_answered_by_the_gateway() {
    let mut stack = stack(Mode::Nat);
    stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, GATEWAY_IP));
    stack.poll(t(1));
    assert!(!stack.from_guest(&ping(GUEST_MAC, GUEST_IP, OUTSIDE_IP)));
    assert!(stack.poll(t(2)).is_empty());
}

#[test]
fn frames_with_someone_elses_address_are_dropped() {
    let mut stack = stack(Mode::Nat);
    // Another hardware address, or the guest's MAC with another IP.
    assert!(!stack.from_guest(&arp_request(OTHER_MAC, OTHER_IP, GATEWAY_IP)));
    assert!(!stack.from_guest(&arp_request(GUEST_MAC, OTHER_IP, GATEWAY_IP)));
    assert!(!stack.from_guest(&ping(OTHER_MAC, GUEST_IP, GATEWAY_IP)));
    assert!(!stack.from_guest(&ping(GUEST_MAC, OTHER_IP, GATEWAY_IP)));
    // Not addressed to the gateway or broadcast.
    let mut unicast_elsewhere = ping(GUEST_MAC, GUEST_IP, GATEWAY_IP);
    unicast_elsewhere[..6].copy_from_slice(&OTHER_MAC);
    assert!(!stack.from_guest(&unicast_elsewhere));
    assert!(stack.poll(t(1)).is_empty());
}

#[test]
fn malformed_and_non_ipv4_frames_are_dropped() {
    let mut stack = stack(Mode::Nat);
    assert!(!stack.from_guest(&[]));
    assert!(!stack.from_guest(&[0xff; 13]));
    let mut ipv6 = ping(GUEST_MAC, GUEST_IP, GATEWAY_IP);
    ipv6[12..14].copy_from_slice(&[0x86, 0xdd]);
    assert!(!stack.from_guest(&ipv6));
    let truncated = &ping(GUEST_MAC, GUEST_IP, GATEWAY_IP)[..20];
    assert!(!stack.from_guest(truncated));
}

#[test]
fn backlog_is_bounded() {
    let mut stack = stack(Mode::Nat);
    let frame = arp_request(GUEST_MAC, GUEST_IP, GATEWAY_IP);
    for _ in 0..BACKLOG {
        assert!(stack.from_guest(&frame));
    }
    assert!(!stack.from_guest(&frame));
    stack.poll(t(1));
    assert!(stack.from_guest(&frame), "room again after a poll");
}

// Outbound TCP, with a smoltcp interface standing in for the guest kernel
// and in-process socket pairs standing in for the internet.

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::IpAddress;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Instant as StdInstant;

/// Host connections as socket pairs: the test gets the far end of each.
#[derive(Default)]
struct FakeHost {
    refuse: HashSet<SocketAddrV4>,
    accepted: Mutex<Vec<(SocketAddrV4, UnixStream)>>,
    attempts: Mutex<Vec<SocketAddrV4>>,
}

impl Connector for FakeHost {
    fn connect(&self, addr: SocketAddrV4) -> std::io::Result<Box<dyn HostStream>> {
        self.attempts.lock().unwrap().push(addr);
        if self.refuse.contains(&addr) || self.accepted.lock().is_err() {
            return Err(std::io::ErrorKind::ConnectionRefused.into());
        }
        let (ours, theirs) = UnixStream::pair()?;
        self.accepted.lock().unwrap().push((addr, theirs));
        Ok(Box::new(ours))
    }
}

impl FakeHost {
    fn take_peer(&self) -> Option<UnixStream> {
        self.accepted.lock().unwrap().pop().map(|(_, s)| s)
    }
}

/// The guest's side of the link: a second smoltcp interface.
struct Rig {
    started: StdInstant,
    stack: Stack,
    host: Arc<FakeHost>,
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
}

fn rig(mode: Mode, host: FakeHost) -> Rig {
    let host = Arc::new(host);
    let stack = Stack::new(t(0), Policy::new(mode, Vec::new()), host.clone());
    let mut link = Link::default();
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
    let mut iface = Interface::new(config, &mut link, instant(t(0)));
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(GUEST_IP.into(), PREFIX_LEN))
            .unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(GATEWAY_IP)
        .unwrap();
    Rig {
        started: StdInstant::now(),
        stack,
        host,
        iface,
        sockets: SocketSet::new(Vec::new()),
        link,
    }
}

const REMOTE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);

impl Rig {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    /// Moves frames both ways until the link is quiet.
    fn pump(&mut self) {
        for _ in 0..100 {
            let now = self.now();
            self.iface
                .poll(instant(now), &mut self.link, &mut self.sockets);
            let out = std::mem::take(&mut self.link.to_guest);
            for frame in &out {
                self.stack.from_guest(frame);
            }
            let back = self.stack.poll(now);
            let quiet = out.is_empty() && back.is_empty();
            self.link.from_guest.extend(back);
            if quiet {
                return;
            }
        }
    }

    /// Pumps, waiting for host events, until `done` or a timeout.
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        let deadline = StdInstant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(StdInstant::now() < deadline, "timed out waiting for {what}");
            self.stack.wait(Duration::from_millis(5));
        }
    }

    fn connect(&mut self, dst: Ipv4Addr, port: u16) -> SocketHandle {
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 64 * 1024]),
            tcp::SocketBuffer::new(vec![0; 64 * 1024]),
        );
        socket
            .connect(self.iface.context(), (IpAddress::from(dst), port), 40000)
            .unwrap();
        self.sockets.add(socket)
    }

    fn socket(&mut self, handle: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(handle)
    }

    fn state(&mut self, handle: SocketHandle) -> tcp::State {
        self.socket(handle).state()
    }

    /// Accepts the host connection the gateway made.
    fn peer(&mut self) -> UnixStream {
        let mut peer = None;
        self.until("the host connection", |r| {
            peer = r.host.take_peer();
            peer.is_some()
        });
        let peer = peer.unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        peer
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn outbound_connection_relays_both_ways_and_closes() {
    let mut r = rig(Mode::Nat, FakeHost::default());
    let s = r.connect(REMOTE, 80);
    let mut peer = r.peer();
    r.until("the guest to connect", |r| {
        r.state(s) == tcp::State::Established
    });
    assert_eq!(
        *r.host.attempts.lock().unwrap(),
        [SocketAddrV4::new(REMOTE, 80)]
    );
    assert_eq!(
        r.stack.take_connects(),
        [ConnectEvent {
            protocol: Protocol::Tcp,
            dst: SocketAddrV4::new(REMOTE, 80),
            allowed: true,
            reason: None
        }]
    );

    r.socket(s).send_slice(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    r.pump();
    let mut request = [0u8; 18];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"GET / HTTP/1.0\r\n\r\n");

    peer.write_all(b"HTTP/1.0 200 OK\r\n").unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    r.until("the response and FIN", |r| {
        let socket = r.socket(s);
        while socket.can_recv() {
            socket
                .recv(|b| (b.len(), response.extend_from_slice(b)))
                .unwrap();
        }
        !socket.may_recv()
    });
    assert_eq!(response, b"HTTP/1.0 200 OK\r\n");

    r.socket(s).close();
    r.until("the guest's FIN to reach the host", |_| true);
    let mut rest = Vec::new();
    peer.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    r.until("the connection to close", |r| {
        matches!(r.state(s), tcp::State::Closed | tcp::State::TimeWait)
    });
}

#[test]
fn a_refused_host_connection_resets_the_guest() {
    let mut host = FakeHost::default();
    host.refuse.insert(SocketAddrV4::new(REMOTE, 81));
    let mut r = rig(Mode::Nat, host);
    let s = r.connect(REMOTE, 81);
    r.until("the reset", |r| r.state(s) == tcp::State::Closed);
    assert_eq!(
        *r.host.attempts.lock().unwrap(),
        [SocketAddrV4::new(REMOTE, 81)]
    );
}

#[test]
fn denied_destinations_are_reset_without_a_host_connection() {
    let mut r = rig(Mode::Nat, FakeHost::default());
    let s = r.connect(Ipv4Addr::new(10, 0, 0, 5), 22);
    r.until("the reset", |r| r.state(s) == tcp::State::Closed);
    assert!(r.host.attempts.lock().unwrap().is_empty());
    assert_eq!(
        r.stack.take_connects(),
        [ConnectEvent {
            protocol: Protocol::Tcp,
            dst: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 22),
            allowed: false,
            reason: Some("LAN")
        }]
    );

    // The gateway has no TCP services yet.
    let s = r.connect(GATEWAY_IP, 80);
    r.until("the gateway's reset", |r| r.state(s) == tcp::State::Closed);
    assert!(r.host.attempts.lock().unwrap().is_empty());
}

#[test]
fn lan_mode_forwards_private_destinations() {
    let mut r = rig(Mode::Lan, FakeHost::default());
    let s = r.connect(Ipv4Addr::new(10, 0, 0, 5), 22);
    let _peer = r.peer();
    r.until("the guest to connect", |r| {
        r.state(s) == tcp::State::Established
    });
}

#[test]
fn large_transfers_arrive_intact_in_both_directions() {
    let mut r = rig(Mode::Nat, FakeHost::default());
    let s = r.connect(REMOTE, 443);
    let mut peer = r.peer();
    r.until("the guest to connect", |r| {
        r.state(s) == tcp::State::Established
    });
    let data = pattern(1_000_000);

    // Host to guest: the host writes on a thread, since its socket buffer
    // fills long before the guest has read everything.
    let mut writer = peer.try_clone().unwrap();
    let sent = data.clone();
    let host_writer = std::thread::spawn(move || writer.write_all(&sent).unwrap());
    let mut got = Vec::new();
    r.until("a megabyte from the host", |r| {
        let socket = r.socket(s);
        while socket.can_recv() {
            socket
                .recv(|b| (b.len(), got.extend_from_slice(b)))
                .unwrap();
        }
        got.len() >= data.len()
    });
    host_writer.join().unwrap();
    assert!(got == data);

    // Guest to host.
    let reader = std::thread::spawn(move || {
        let mut got = vec![0u8; 1_000_000];
        peer.read_exact(&mut got).unwrap();
        got
    });
    let mut offset = 0;
    r.until("a megabyte from the guest", |r| {
        let socket = r.socket(s);
        while offset < data.len() && socket.can_send() {
            offset += socket.send_slice(&data[offset..]).unwrap();
        }
        offset == data.len() && r.socket(s).send_queue() == 0
    });
    assert!(reader.join().unwrap() == data);
}
