use super::*;
use crate::devices::virtio::testing::{DriverQueue, guest_memory};
use std::time::Instant;

const GUEST_CID: u64 = 3;
const AGENT_PORT: u32 = 1024;
const RX_BUF: u32 = (HEADER_SIZE + 4096) as u32;

struct Rig {
    mem: GuestMemory,
    rxq: DriverQueue,
    txq: DriverQueue,
    dev: Vsock,
    /// Receive buffers posted, by head.
    posted: Vec<u16>,
    /// Used receive buffers not yet looked at.
    used: VecDeque<(u32, u32)>,
}

fn rig() -> Rig {
    let mem = guest_memory();
    let rxq = DriverQueue::at(&mem, 64, 0);
    let txq = DriverQueue::at(&mem, 64, 1);
    let evq = DriverQueue::at(&mem, 8, 2);
    let mut dev = Vsock::new(GUEST_CID);
    dev.activate(Activation {
        memory: mem.clone(),
        queues: vec![rxq.device_queue(), txq.device_queue(), evq.device_queue()],
        features: 0,
        interrupt: Interrupt::new(),
    })
    .unwrap();
    let mut r = Rig {
        mem,
        rxq,
        txq,
        dev,
        posted: Vec::new(),
        used: VecDeque::new(),
    };
    for _ in 0..8 {
        r.post();
    }
    r
}

impl Rig {
    fn post(&mut self) {
        let head = self.rxq.add(&self.mem, &[], &[RX_BUF]);
        self.posted.push(head);
        self.dev.queue_notify(RX_QUEUE);
    }

    /// The next packet the device gave the guest.
    fn next_packet(&mut self) -> (Header, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fresh = self.rxq.used(&self.mem);
            self.used.extend(fresh);
            if let Some((head, len)) = self.used.pop_front() {
                let head = head as u16;
                let bytes = self.rxq.written(&self.mem, head, 0, len as usize);
                self.rxq.recycle(head);
                self.post();
                let header = Header::from_bytes(bytes[..HEADER_SIZE].try_into().unwrap());
                assert_eq!(header.len as usize, bytes.len() - HEADER_SIZE);
                return (header, bytes[HEADER_SIZE..].to_vec());
            }
            assert!(Instant::now() < deadline, "no packet for the guest");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Asserts no packet arrives within a short time.
    fn no_packet(&mut self) {
        thread::sleep(Duration::from_millis(100));
        assert!(self.used.is_empty() && self.rxq.used(&self.mem).is_empty());
    }

    fn guest_send(&mut self, header: Header, data: &[u8]) {
        let header = Header {
            len: data.len() as u32,
            ..header
        };
        let bytes = header.to_bytes();
        self.txq.add(&self.mem, &[&bytes, data], &[]);
        self.dev.queue_notify(TX_QUEUE);
    }

    /// A header from the guest's `guest_port` to the host's `host_port`.
    fn guest_header(&self, guest_port: u32, host_port: u32, op: u16) -> Header {
        Header {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port: guest_port,
            dst_port: host_port,
            kind: TYPE_STREAM,
            op,
            buf_alloc: 64 * 1024,
            ..Header::default()
        }
    }

    /// Connects a host process to the agent port and accepts in the guest.
    /// Returns the host stream and the host port the device picked.
    fn host_connect(&mut self) -> (UnixStream, u32) {
        let stream = with_timeout(self.dev.connector().connect(AGENT_PORT).unwrap());
        let (req, _) = self.next_packet();
        assert_eq!(req.op, op::REQUEST);
        let response = self.guest_header(AGENT_PORT, req.src_port, op::RESPONSE);
        self.guest_send(response, &[]);
        (stream, req.src_port)
    }
}

/// Sets a read timeout, which macOS refuses once the peer has closed, so
/// tests set it as soon as they have the stream.
fn with_timeout(stream: UnixStream) -> UnixStream {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}

fn read_exactly(stream: &mut UnixStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).unwrap();
    buf
}

#[test]
fn header_round_trips() {
    let h = Header {
        src_cid: 3,
        dst_cid: 2,
        src_port: 1,
        dst_port: 2,
        len: 3,
        kind: 1,
        op: 5,
        flags: 6,
        buf_alloc: 7,
        fwd_cnt: 8,
    };
    assert_eq!(Header::from_bytes(&h.to_bytes()), h);
}

#[test]
fn config_has_the_guest_cid() {
    let r = rig();
    let mut config = [0u8; 8];
    r.dev.read_config(0, &mut config);
    assert_eq!(u64::from_le_bytes(config), GUEST_CID);
    assert_eq!(r.dev.queue_max_sizes().len(), 3);
}

#[test]
fn host_connection_to_a_guest_port_carries_data_both_ways() {
    let mut r = rig();
    let mut host = with_timeout(r.dev.connector().connect(AGENT_PORT).unwrap());
    let (req, _) = r.next_packet();
    assert_eq!(
        (req.op, req.src_cid, req.dst_cid, req.dst_port, req.kind),
        (op::REQUEST, HOST_CID, GUEST_CID, AGENT_PORT, TYPE_STREAM)
    );
    assert!(req.src_port >= FIRST_HOST_PORT);
    assert_eq!(req.buf_alloc, BUF_ALLOC);
    let port = req.src_port;
    r.guest_send(r.guest_header(AGENT_PORT, port, op::RESPONSE), &[]);

    host.write_all(b"hello").unwrap();
    let (rw, data) = r.next_packet();
    assert_eq!(
        (rw.op, rw.dst_port, data.as_slice()),
        (op::RW, AGENT_PORT, &b"hello"[..])
    );

    r.guest_send(r.guest_header(AGENT_PORT, port, op::RW), b"world");
    assert_eq!(read_exactly(&mut host, 5), b"world");

    // The host closes its sending side: the guest is told.
    host.shutdown(Shutdown::Write).unwrap();
    let (shut, _) = r.next_packet();
    assert_eq!((shut.op, shut.flags), (op::SHUTDOWN, shutdown::SEND));

    // The guest resets: the host sees the end.
    r.guest_send(r.guest_header(AGENT_PORT, port, op::RST), &[]);
    let mut rest = Vec::new();
    assert_eq!(host.read_to_end(&mut rest).unwrap(), 0);
}

#[test]
fn guest_connection_to_a_host_port_handler() {
    let mut r = rig();
    let (handed_out, picked_up) = mpsc::channel();
    r.dev.serve_host_port(
        5000,
        Arc::new(move || {
            let (host, device) = UnixStream::pair()?;
            handed_out.send(host).unwrap();
            Ok(device)
        }),
    );
    r.guest_send(r.guest_header(777, 5000, op::REQUEST), &[]);
    let (resp, _) = r.next_packet();
    assert_eq!(
        (resp.op, resp.src_port, resp.dst_port),
        (op::RESPONSE, 5000, 777)
    );
    let mut host = with_timeout(picked_up.recv().unwrap());

    r.guest_send(r.guest_header(777, 5000, op::RW), b"ping");
    assert_eq!(read_exactly(&mut host, 4), b"ping");
    host.write_all(b"pong").unwrap();
    let (rw, data) = r.next_packet();
    assert_eq!((rw.op, data.as_slice()), (op::RW, &b"pong"[..]));

    // The guest shuts down both ways: the device resets and the host sees
    // the end.
    let both = Header {
        flags: shutdown::SEND | shutdown::RCV,
        ..r.guest_header(777, 5000, op::SHUTDOWN)
    };
    r.guest_send(both, &[]);
    let (rst, _) = r.next_packet();
    assert_eq!(rst.op, op::RST);
    let mut rest = Vec::new();
    assert_eq!(host.read_to_end(&mut rest).unwrap(), 0);
}

#[test]
fn guest_connection_to_a_port_with_no_handler_or_a_failing_one_is_reset() {
    let mut r = rig();
    r.guest_send(r.guest_header(777, 6000, op::REQUEST), &[]);
    let (rst, _) = r.next_packet();
    assert_eq!((rst.op, rst.src_port, rst.dst_port), (op::RST, 6000, 777));

    r.dev
        .serve_host_port(6001, Arc::new(|| Err(io::ErrorKind::Other.into())));
    r.guest_send(r.guest_header(778, 6001, op::REQUEST), &[]);
    let (rst, _) = r.next_packet();
    assert_eq!((rst.op, rst.src_port, rst.dst_port), (op::RST, 6001, 778));
}

#[test]
fn packets_for_unknown_connections_are_reset_and_strangers_ignored() {
    let mut r = rig();
    r.guest_send(r.guest_header(1, 2, op::RW), b"x");
    let (rst, _) = r.next_packet();
    assert_eq!(rst.op, op::RST);
    // An RST for nothing gets no answer, and neither does a packet with
    // someone else's source CID.
    r.guest_send(r.guest_header(1, 2, op::RST), &[]);
    let spoofed = Header {
        src_cid: 99,
        ..r.guest_header(1, 2, op::REQUEST)
    };
    r.guest_send(spoofed, &[]);
    r.no_packet();
}

#[test]
fn host_data_stays_within_the_guest_credit() {
    let mut r = rig();
    let mut host = with_timeout(r.dev.connector().connect(AGENT_PORT).unwrap());
    let (req, _) = r.next_packet();
    let port = req.src_port;
    let small = Header {
        buf_alloc: 10,
        ..r.guest_header(AGENT_PORT, port, op::RESPONSE)
    };
    r.guest_send(small, &[]);
    host.write_all(&[7u8; 25]).unwrap();
    let mut got = 0;
    while got < 10 {
        got += r.next_packet().1.len();
    }
    assert_eq!(got, 10);
    r.no_packet();

    // The guest consumed what it got: ten more may come.
    let update = Header {
        buf_alloc: 10,
        fwd_cnt: 10,
        ..r.guest_header(AGENT_PORT, port, op::CREDIT_UPDATE)
    };
    r.guest_send(update, &[]);
    while got < 20 {
        got += r.next_packet().1.len();
    }
    assert_eq!(got, 20);
    r.no_packet();
}

#[test]
fn host_data_larger_than_a_receive_buffer_is_split_in_order() {
    let mut r = rig();
    let (mut host, _) = r.host_connect();
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    host.write_all(&data).unwrap();
    let mut got = Vec::new();
    while got.len() < data.len() {
        let (header, payload) = r.next_packet();
        assert_eq!(header.op, op::RW);
        assert!(payload.len() <= RX_BUF as usize - HEADER_SIZE);
        got.extend(payload);
    }
    assert_eq!(got, data);
}

#[test]
fn guest_close_after_sending_delivers_everything_first() {
    let mut r = rig();
    let (mut host, port) = r.host_connect();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    for chunk in data.chunks(50_000) {
        r.guest_send(r.guest_header(AGENT_PORT, port, op::RW), chunk);
    }
    // close() in the guest: no more sending or receiving.
    let close = Header {
        flags: shutdown::SEND | shutdown::RCV,
        ..r.guest_header(AGENT_PORT, port, op::SHUTDOWN)
    };
    r.guest_send(close, &[]);
    let mut got = Vec::new();
    host.read_to_end(&mut got).unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data);
    // Then the device resets the connection for the guest.
    loop {
        let (header, _) = r.next_packet();
        if header.op == op::RST {
            break;
        }
        assert_eq!(header.op, op::CREDIT_UPDATE);
    }
}

#[test]
fn credit_request_is_answered_with_an_update() {
    let mut r = rig();
    let (_host, port) = r.host_connect();
    r.guest_send(r.guest_header(AGENT_PORT, port, op::CREDIT_REQUEST), &[]);
    let (update, _) = r.next_packet();
    assert_eq!(
        (update.op, update.buf_alloc),
        (op::CREDIT_UPDATE, BUF_ALLOC)
    );
}

#[test]
fn guest_overrunning_the_credit_is_reset() {
    let mut r = rig();
    let (host, port) = r.host_connect();
    // Nobody reads on the host side, so the bytes stay in flight.
    let chunk = vec![1u8; 32 * 1024];
    for _ in 0..(BUF_ALLOC as usize / chunk.len()) {
        r.guest_send(r.guest_header(AGENT_PORT, port, op::RW), &chunk);
    }
    // Some of it may already have been written and credited; send until
    // the device refuses.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        r.guest_send(
            r.guest_header(AGENT_PORT, port, op::RW),
            &vec![2u8; BUF_ALLOC as usize],
        );
        if let Some(&(head, len)) = r.rxq.used(&r.mem).first() {
            let bytes = r.rxq.written(&r.mem, head as u16, 0, len as usize);
            let header = Header::from_bytes(bytes[..HEADER_SIZE].try_into().unwrap());
            if header.op == op::RST {
                break;
            }
        }
        assert!(Instant::now() < deadline, "never reset");
    }
    let _ = host.shutdown(Shutdown::Both);
}

#[test]
fn device_reset_closes_host_connections() {
    let mut r = rig();
    let (mut host, _) = r.host_connect();
    r.dev.reset();
    let mut rest = Vec::new();
    assert_eq!(host.read_to_end(&mut rest).unwrap(), 0);
}

#[test]
fn host_connection_before_the_driver_starts_fails() {
    let dev = Vsock::new(GUEST_CID);
    let err = dev.connector().connect(AGENT_PORT).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotConnected);
}

#[test]
fn a_restored_device_continues_from_the_saved_queues() {
    let mut r = rig();
    let _host = r.dev.connector().connect(AGENT_PORT).unwrap();
    let (request, _) = r.next_packet();
    assert_eq!(request.op, op::REQUEST);
    let saved = r.dev.save_queues();
    assert_eq!(saved.len(), 3, "the event queue is kept too");
    assert_eq!(saved[RX_QUEUE as usize].next_used, 1);
    assert_eq!(saved[EVENT_QUEUE as usize].size, 8);

    // A new process: connections to the host are gone, the rings carry on.
    let mut dev = Vsock::new(GUEST_CID);
    dev.activate(Activation {
        memory: r.mem.clone(),
        queues: saved.clone(),
        features: 0,
        interrupt: Interrupt::new(),
    })
    .unwrap();
    assert_eq!(dev.save_queues(), saved);
    r.dev = dev;
    r.dev.queue_notify(RX_QUEUE);
    let _host = r.dev.connector().connect(AGENT_PORT).unwrap();
    let (request, _) = r.next_packet();
    assert_eq!(request.op, op::REQUEST);
    assert_eq!(r.dev.save_queues()[RX_QUEUE as usize].next_used, 2);
}

#[test]
fn a_restore_sends_a_transport_reset_when_an_event_buffer_is_posted() {
    let mem = guest_memory();
    let rxq = DriverQueue::at(&mem, 64, 0);
    let txq = DriverQueue::at(&mem, 64, 1);
    let mut evq = DriverQueue::at(&mem, 8, 2);
    let interrupt = Interrupt::new();
    let mut dev = Vsock::new(GUEST_CID);
    dev.activate(Activation {
        memory: mem.clone(),
        queues: vec![rxq.device_queue(), txq.device_queue(), evq.device_queue()],
        features: 0,
        interrupt: interrupt.clone(),
    })
    .unwrap();
    // No event buffer yet: the reset waits for one.
    dev.restored();
    assert!(evq.used(&mem).is_empty());
    assert!(!interrupt.level());

    let head = evq.add(&mem, &[], &[4]);
    dev.queue_notify(EVENT_QUEUE);
    assert_eq!(evq.used(&mem), vec![(u32::from(head), 4)]);
    assert_eq!(
        evq.written(&mem, head, 0, 4),
        EVENT_TRANSPORT_RESET.to_le_bytes()
    );
    assert!(interrupt.level());

    // Sent once.
    evq.add(&mem, &[], &[4]);
    dev.queue_notify(EVENT_QUEUE);
    assert!(evq.used(&mem).is_empty());
    assert_eq!(dev.save_queues()[EVENT_QUEUE as usize].next_used, 1);
}
