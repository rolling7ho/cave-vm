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
    let mut stack = Stack::new(t(0));
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
fn arp_for_other_addresses_is_not_answered() {
    let mut stack = Stack::new(t(0));
    assert!(stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, OTHER_IP)));
    assert!(stack.poll(t(1)).is_empty());
}

#[test]
fn pings_to_the_gateway_are_answered() {
    let mut stack = Stack::new(t(0));
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
fn pings_to_the_outside_get_no_answer_yet() {
    let mut stack = Stack::new(t(0));
    stack.from_guest(&arp_request(GUEST_MAC, GUEST_IP, GATEWAY_IP));
    stack.poll(t(1));
    assert!(stack.from_guest(&ping(GUEST_MAC, GUEST_IP, OUTSIDE_IP)));
    assert!(stack.poll(t(2)).is_empty());
}

#[test]
fn frames_with_someone_elses_address_are_dropped() {
    let mut stack = Stack::new(t(0));
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
    let mut stack = Stack::new(t(0));
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
    let mut stack = Stack::new(t(0));
    let frame = arp_request(GUEST_MAC, GUEST_IP, OTHER_IP);
    for _ in 0..BACKLOG {
        assert!(stack.from_guest(&frame));
    }
    assert!(!stack.from_guest(&frame));
    stack.poll(t(1));
    assert!(stack.from_guest(&frame), "room again after a poll");
}
