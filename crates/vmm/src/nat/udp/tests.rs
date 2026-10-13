use std::os::unix::net::UnixDatagram;
use std::sync::Mutex;
use std::time::Instant as StdInstant;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetFrame, EthernetRepr, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};

use super::super::*;
use super::*;

const REMOTE: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
const DNS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 53);

/// Host sockets that are socket pairs; the test keeps the other end.
#[derive(Default)]
struct FakeUdp {
    opened: Mutex<Vec<(SocketAddrV4, UnixDatagram)>>,
}

impl UdpConnector for FakeUdp {
    fn open(&self, dst: SocketAddrV4) -> io::Result<Arc<dyn HostDatagram>> {
        let (ours, theirs) = UnixDatagram::pair()?;
        ours.set_read_timeout(Some(Duration::from_millis(20)))?;
        theirs.set_read_timeout(Some(Duration::from_secs(2)))?;
        self.opened.lock().unwrap().push((dst, theirs));
        Ok(Arc::new(ours))
    }
}

impl FakeUdp {
    fn opened(&self) -> Vec<SocketAddrV4> {
        self.opened
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| *a)
            .collect()
    }

    /// The host end of the socket opened to `dst`.
    fn peer(&self, dst: SocketAddrV4) -> UnixDatagram {
        let opened = self.opened.lock().unwrap();
        let (_, peer) = opened
            .iter()
            .rev()
            .find(|(a, _)| *a == dst)
            .expect("opened");
        peer.try_clone().unwrap()
    }
}

fn stack(mode: Mode, dns: Vec<SocketAddrV4>) -> (Stack, Arc<FakeUdp>) {
    let udp = Arc::new(FakeUdp::default());
    let stack = Stack::new(
        Duration::ZERO,
        Policy::new(mode, Vec::new()),
        Arc::new(TcpConnector::default()),
    )
    .with_udp(udp.clone(), dns);
    (stack, udp)
}

/// A UDP frame from the guest.
fn datagram(guest_port: u16, dst: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
    let udp = UdpRepr {
        src_port: guest_port,
        dst_port: dst.port(),
    };
    let ip = Ipv4Repr {
        src_addr: GUEST_IP,
        dst_addr: *dst.ip(),
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(GATEWAY_MAC),
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
        &GUEST_IP.into(),
        &(*dst.ip()).into(),
        payload.len(),
        |buf| buf.copy_from_slice(payload),
        &caps,
    );
    frame
}

/// The source, destination port and payload of a UDP frame for the guest,
/// checking its checksums.
fn reply(frame: &[u8]) -> (SocketAddrV4, u16, Vec<u8>) {
    let eth = EthernetFrame::new_checked(frame).unwrap();
    assert_eq!(eth.dst_addr(), EthernetAddress(GUEST_MAC));
    assert_eq!(eth.src_addr(), EthernetAddress(GATEWAY_MAC));
    let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
    assert!(ip.verify_checksum());
    assert_eq!(ip.dst_addr(), GUEST_IP);
    let udp = UdpPacket::new_checked(ip.payload()).unwrap();
    assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
    (
        SocketAddrV4::new(ip.src_addr(), udp.src_port()),
        udp.dst_port(),
        udp.payload().to_vec(),
    )
}

/// The next datagram on a host socket.
fn recv(peer: &UnixDatagram) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    let n = peer.recv(&mut buf).unwrap();
    buf[..n].to_vec()
}

/// Polls until the stack has frames for the guest, or two seconds pass.
fn frames(stack: &mut Stack) -> Vec<Vec<u8>> {
    let started = StdInstant::now();
    loop {
        stack.wait(Duration::from_millis(50));
        let out = stack.poll(started.elapsed());
        if !out.is_empty() || started.elapsed() > Duration::from_secs(2) {
            return out;
        }
    }
}

#[test]
fn resolv_conf_gives_the_ipv4_name_servers() {
    let text = "# comment\nnameserver 192.168.1.1\nnameserver fe80::1%en0\nsearch lan\n  nameserver   1.1.1.1  \n";
    assert_eq!(
        parse_resolv_conf(text),
        [
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 53),
            SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53),
        ]
    );
}

#[test]
fn a_datagram_goes_out_and_the_reply_comes_back() {
    let (mut stack, udp) = stack(Mode::Nat, Vec::new());
    let dst = SocketAddrV4::new(REMOTE, 123);
    assert!(stack.from_guest(&datagram(40000, dst, b"ping")));
    assert!(stack.from_guest(&datagram(40000, dst, b"again")));
    assert_eq!(udp.opened(), [dst], "one host socket per flow");

    let peer = udp.peer(dst);
    assert_eq!(recv(&peer), b"ping");
    assert_eq!(recv(&peer), b"again");
    peer.send(b"pong").unwrap();

    let out = frames(&mut stack);
    assert_eq!(out.len(), 1);
    assert_eq!(reply(&out[0]), (dst, 40000, b"pong".to_vec()));
    assert_eq!(
        stack.take_connects(),
        [ConnectEvent {
            protocol: Protocol::Udp,
            dst,
            allowed: true,
            reason: None
        }],
        "reported once per flow"
    );
}

#[test]
fn dns_to_the_gateway_goes_to_the_macs_server() {
    let (mut stack, udp) = stack(Mode::Nat, vec![DNS]);
    let gateway_dns = SocketAddrV4::new(GATEWAY_IP, DNS_PORT);
    assert!(stack.from_guest(&datagram(5353, gateway_dns, b"query")));
    assert_eq!(udp.opened(), [DNS]);

    let peer = udp.peer(DNS);
    assert_eq!(recv(&peer), b"query");
    peer.send(b"answer").unwrap();

    let out = frames(&mut stack);
    assert_eq!(reply(&out[0]), (gateway_dns, 5353, b"answer".to_vec()));
    assert!(stack.take_connects().is_empty(), "DNS is not a connection");
}

#[test]
fn dns_without_a_server_and_other_gateway_ports_are_dropped() {
    let (mut stack, udp) = stack(Mode::Nat, Vec::new());
    assert!(!stack.from_guest(&datagram(1, SocketAddrV4::new(GATEWAY_IP, DNS_PORT), b"q")));
    assert!(!stack.from_guest(&datagram(1, SocketAddrV4::new(GATEWAY_IP, 9999), b"q")));
    assert!(udp.opened().is_empty());
    assert!(
        stack.poll(Duration::from_millis(1)).is_empty(),
        "no ICMP either"
    );
}

#[test]
fn denied_datagrams_are_dropped_and_reported_once() {
    let (mut stack, udp) = stack(Mode::Nat, vec![DNS]);
    let lan = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 53);
    assert!(!stack.from_guest(&datagram(7, lan, b"q")));
    assert!(!stack.from_guest(&datagram(7, lan, b"q")));
    assert!(udp.opened().is_empty());
    assert_eq!(
        stack.take_connects(),
        [ConnectEvent {
            protocol: Protocol::Udp,
            dst: lan,
            allowed: false,
            reason: Some("LAN")
        }]
    );
    assert!(stack.poll(Duration::from_millis(1)).is_empty());

    // lan mode lets it through.
    let (mut stack, udp) = self::stack(Mode::Lan, Vec::new());
    assert!(stack.from_guest(&datagram(7, lan, b"q")));
    assert_eq!(udp.opened(), [lan]);
}

#[test]
fn without_udp_set_up_datagrams_are_dropped_silently() {
    let mut stack = Stack::new(
        Duration::ZERO,
        Policy::new(Mode::Nat, Vec::new()),
        Arc::new(TcpConnector::default()),
    );
    assert!(!stack.from_guest(&datagram(1, SocketAddrV4::new(REMOTE, 53), b"q")));
    assert!(stack.poll(Duration::from_millis(1)).is_empty());
}

#[test]
fn idle_flows_are_closed() {
    let (mut stack, udp) = stack(Mode::Nat, Vec::new());
    let dst = SocketAddrV4::new(REMOTE, 123);
    assert!(stack.from_guest(&datagram(1, dst, b"a")));
    stack.poll(IDLE_TIMEOUT - Duration::from_secs(1));
    assert_eq!(stack.udp.len(), 1);
    stack.poll(IDLE_TIMEOUT + Duration::from_secs(1));
    assert_eq!(stack.udp.len(), 0);
    // The next datagram opens a new socket.
    assert!(stack.from_guest(&datagram(1, dst, b"b")));
    assert_eq!(udp.opened(), [dst, dst]);
}

#[test]
fn a_reply_too_big_for_one_frame_is_dropped() {
    let src = SocketAddrV4::new(REMOTE, 1);
    assert!(frame_to_guest(src, 2, &[0; MAX_PAYLOAD]).is_some());
    assert!(frame_to_guest(src, 2, &[0; MAX_PAYLOAD + 1]).is_none());
}

#[test]
fn fragments_are_not_parsed() {
    let mut frame = datagram(1, SocketAddrV4::new(REMOTE, 53), b"q");
    {
        let mut eth = EthernetFrame::new_unchecked(&mut frame);
        let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
        ip.set_more_frags(true);
        ip.fill_checksum();
    }
    assert!(parse(&frame).is_none());
    let (mut stack, udp) = stack(Mode::Nat, Vec::new());
    assert!(!stack.from_guest(&frame));
    assert!(udp.opened().is_empty());
}
