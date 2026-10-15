//! Outbound TCP: each guest connection the policy allows becomes a host
//! socket, and the gateway relays bytes between the two.
//!
//! The guest's SYN is held until the host connection is up, so a refused or
//! unreachable destination reaches the guest as a reset, as it would on a
//! real network. Only then does a smoltcp socket listen on the destination
//! address and take the SYN. Each host socket has a reader and a writer
//! thread; both are bounded by [`WINDOW`] bytes, so neither side can make
//! the gateway buffer without limit.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::tcp;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpListenEndpoint, IpProtocol,
    Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber,
};

use super::host::{Connector, HostStream};
use super::{Event, GATEWAY_MAC, GUEST_IP, GUEST_MAC};

/// Bytes each direction may have in flight inside the gateway.
pub const WINDOW: usize = 256 * 1024;

/// Bytes read from a host socket at a time.
const READ_CHUNK: usize = 16 * 1024;

/// Most connections open or connecting at once, as for UDP. Each has two
/// threads and up to two [`WINDOW`]s of buffers, so a guest opening
/// connections without end cannot exhaust the Mac. A SYN that would open
/// another is reset.
const MAX_FLOWS: usize = 1024;

/// A guest connection: its port, and where it is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub guest_port: u16,
    pub dst: SocketAddrV4,
}

/// What host threads report to the gateway.
pub enum HostEvent {
    Connected(FlowKey, Box<dyn HostStream>),
    Failed(FlowKey),
    Data(FlowKey, Vec<u8>),
    /// The host peer will send no more.
    Eof(FlowKey),
    /// The host socket failed; the guest connection is reset.
    Reset(FlowKey),
}

/// Bytes a host reader has handed over that are not yet in smoltcp.
#[derive(Default)]
struct Credit {
    outstanding: Mutex<usize>,
    freed: Condvar,
    closed: AtomicBool,
}

impl Credit {
    /// Waits until there is room. Returns `false` once the flow is gone.
    fn wait_for_room(&self) -> bool {
        let mut outstanding = self.outstanding.lock().expect("credit lock");
        while *outstanding >= WINDOW && !self.closed.load(Ordering::Acquire) {
            outstanding = self.freed.wait(outstanding).expect("credit lock");
        }
        !self.closed.load(Ordering::Acquire)
    }

    fn take(&self, n: usize) {
        *self.outstanding.lock().expect("credit lock") += n;
    }

    fn give(&self, n: usize) {
        let mut outstanding = self.outstanding.lock().expect("credit lock");
        *outstanding = outstanding.saturating_sub(n);
        self.freed.notify_all();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _guard = self.outstanding.lock().expect("credit lock");
        self.freed.notify_all();
    }
}

enum Phase {
    /// Waiting for the host connection; the guest's SYN is held.
    Connecting {
        syn: Vec<u8>,
    },
    Open(Box<Open>),
}

struct Open {
    socket: SocketHandle,
    stream: Box<dyn HostStream>,
    /// Data for the writer thread. Dropped once the guest sends FIN.
    to_host: Option<Sender<Vec<u8>>>,
    /// Bytes queued for the writer thread and not yet written.
    to_host_queued: Arc<Mutex<usize>>,
    /// Host data not yet in the smoltcp socket.
    from_host: VecDeque<u8>,
    credit: Arc<Credit>,
    host_eof: bool,
    closing: bool,
}

/// Every outbound TCP connection.
pub struct Flows {
    flows: HashMap<FlowKey, Phase>,
    /// [`MAX_FLOWS`], lowered by tests.
    pub(super) max: usize,
}

impl Default for Flows {
    fn default() -> Flows {
        Flows {
            flows: HashMap::new(),
            max: MAX_FLOWS,
        }
    }
}

impl Flows {
    /// Whether another connection would pass [`MAX_FLOWS`].
    pub fn is_full(&self) -> bool {
        self.flows.len() >= self.max
    }

    pub fn contains(&self, key: &FlowKey) -> bool {
        self.flows.contains_key(key)
    }

    pub fn is_connecting(&self, key: &FlowKey) -> bool {
        matches!(self.flows.get(key), Some(Phase::Connecting { .. }))
    }

    /// The connections the guest has open, sorted.
    pub fn open_keys(&self) -> Vec<FlowKey> {
        let mut keys: Vec<FlowKey> = self
            .flows
            .iter()
            .filter(|(_, phase)| matches!(phase, Phase::Open(_)))
            .map(|(key, _)| *key)
            .collect();
        keys.sort_by_key(|k| (k.guest_port, *k.dst.ip(), k.dst.port()));
        keys
    }

    /// Starts connecting for a new guest connection whose SYN is `syn`.
    pub fn connect(
        &mut self,
        key: FlowKey,
        syn: Vec<u8>,
        connector: &Arc<dyn Connector>,
        events: &Sender<Event>,
    ) {
        self.flows.insert(key, Phase::Connecting { syn });
        let connector = Arc::clone(connector);
        let events = events.clone();
        thread::spawn(move || {
            let event = match connector.connect(key.dst) {
                Ok(stream) => HostEvent::Connected(key, stream),
                Err(_) => HostEvent::Failed(key),
            };
            let _ = events.send(Event::Tcp(event));
        });
    }

    /// Acts on one host event. Returns frames to hand smoltcp: the held SYN
    /// once connected, or once refused (so smoltcp answers with a reset).
    pub fn host_event(
        &mut self,
        event: HostEvent,
        sockets: &mut SocketSet<'static>,
        events: &Sender<Event>,
    ) -> Option<Vec<u8>> {
        match event {
            HostEvent::Connected(key, stream) => {
                let Some(Phase::Connecting { .. }) = self.flows.get(&key) else {
                    let _ = stream.shutdown_stream(Shutdown::Both);
                    return None;
                };
                match open(key, stream, sockets, events) {
                    Some(open) => {
                        let Some(Phase::Connecting { syn }) =
                            self.flows.insert(key, Phase::Open(Box::new(open)))
                        else {
                            unreachable!("checked above");
                        };
                        Some(syn)
                    }
                    None => self.refuse(key),
                }
            }
            HostEvent::Failed(key) => self.refuse(key),
            HostEvent::Data(key, data) => {
                if let Some(Phase::Open(open)) = self.flows.get_mut(&key) {
                    open.from_host.extend(data);
                }
                None
            }
            HostEvent::Eof(key) => {
                if let Some(Phase::Open(open)) = self.flows.get_mut(&key) {
                    open.host_eof = true;
                }
                None
            }
            HostEvent::Reset(key) => {
                if let Some(Phase::Open(open)) = self.flows.get_mut(&key) {
                    sockets.get_mut::<tcp::Socket>(open.socket).abort();
                }
                None
            }
        }
    }

    /// Drops a connecting flow and returns its SYN, which smoltcp answers
    /// with a reset since no socket listens for it.
    fn refuse(&mut self, key: FlowKey) -> Option<Vec<u8>> {
        match self.flows.remove(&key) {
            Some(Phase::Connecting { syn }) => Some(syn),
            Some(phase) => {
                self.flows.insert(key, phase);
                None
            }
            None => None,
        }
    }

    /// Moves bytes between smoltcp sockets and host sockets, and removes
    /// finished flows. Returns whether anything moved.
    pub fn relay(&mut self, sockets: &mut SocketSet<'static>) -> bool {
        let mut moved = false;
        let mut finished = Vec::new();
        for (key, phase) in &mut self.flows {
            let Phase::Open(open) = phase else { continue };
            let socket = sockets.get_mut::<tcp::Socket>(open.socket);
            moved |= open.relay(socket);
            if socket.state() == tcp::State::Closed || socket.state() == tcp::State::TimeWait {
                finished.push(*key);
            }
        }
        for key in finished {
            if let Some(Phase::Open(open)) = self.flows.remove(&key) {
                let _ = open.stream.shutdown_stream(Shutdown::Both);
                open.credit.close();
                sockets.remove(open.socket);
            }
        }
        moved
    }
}

impl Open {
    fn relay(&mut self, socket: &mut tcp::Socket<'static>) -> bool {
        let mut moved = false;
        // Guest to host, within the writer's window.
        if let Some(to_host) = &self.to_host {
            while socket.can_recv() {
                let room = WINDOW.saturating_sub(*self.to_host_queued.lock().expect("queued lock"));
                if room == 0 {
                    break;
                }
                let chunk = socket
                    .recv(|buf| {
                        let n = buf.len().min(room);
                        (n, buf[..n].to_vec())
                    })
                    .unwrap_or_default();
                if chunk.is_empty() {
                    break;
                }
                *self.to_host_queued.lock().expect("queued lock") += chunk.len();
                let _ = to_host.send(chunk);
                moved = true;
            }
            let guest_sent_fin = matches!(
                socket.state(),
                tcp::State::CloseWait
                    | tcp::State::LastAck
                    | tcp::State::Closing
                    | tcp::State::TimeWait
            );
            if guest_sent_fin && !socket.can_recv() {
                // The guest sent FIN: the writer shuts the host side once it
                // has written everything.
                self.to_host = None;
                moved = true;
            }
        }
        // Host to guest, as fast as the guest's window allows.
        while !self.from_host.is_empty() && socket.can_send() {
            let (front, _) = self.from_host.as_slices();
            let n = socket.send_slice(front).unwrap_or(0);
            if n == 0 {
                break;
            }
            self.from_host.drain(..n);
            self.credit.give(n);
            moved = true;
        }
        if self.host_eof && self.from_host.is_empty() && !self.closing {
            socket.close();
            self.closing = true;
            moved = true;
        }
        moved
    }
}

/// Bare acknowledgments to the guest on `key`'s connection that it cannot
/// accept, so it answers with an acknowledgment carrying its own sequence
/// numbers, which are not known here. Linux answers any segment outside
/// its receive window; some stacks check the acknowledgment number first
/// and answer only one for data not yet sent, so the probes cover the
/// sequence space in quarters and one of them is always ahead.
pub fn probe_frames(key: FlowKey) -> impl Iterator<Item = Vec<u8>> {
    (0..4u32).map(move |quarter| probe_frame(key, quarter << 30))
}

fn probe_frame(key: FlowKey, ack: u32) -> Vec<u8> {
    let tcp = TcpRepr {
        src_port: key.dst.port(),
        dst_port: key.guest_port,
        control: TcpControl::None,
        seq_number: TcpSeqNumber(0),
        ack_number: Some(TcpSeqNumber(ack as i32)),
        window_len: 0,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    };
    let ip = Ipv4Repr {
        src_addr: *key.dst.ip(),
        dst_addr: GUEST_IP,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.header_len(),
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
    let mut tcp_packet = TcpPacket::new_unchecked(ip_packet.payload_mut());
    tcp.emit(
        &mut tcp_packet,
        &(*key.dst.ip()).into(),
        &GUEST_IP.into(),
        &caps,
    );
    frame
}

/// A listening socket for the guest's SYN, and the host threads.
fn open(
    key: FlowKey,
    stream: Box<dyn HostStream>,
    sockets: &mut SocketSet<'static>,
    events: &Sender<Event>,
) -> Option<Open> {
    let reader = stream.try_clone_stream().ok()?;
    let writer = stream.try_clone_stream().ok()?;
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; WINDOW]),
        tcp::SocketBuffer::new(vec![0; WINDOW]),
    );
    socket.set_nagle_enabled(false);
    let addr = *key.dst.ip();
    socket
        .listen(IpListenEndpoint {
            addr: Some(addr.into()),
            port: key.dst.port(),
        })
        .ok()?;
    let handle = sockets.add(socket);

    let credit = Arc::new(Credit::default());
    {
        let credit = Arc::clone(&credit);
        let events = events.clone();
        thread::spawn(move || read_host(key, reader, &credit, &events));
    }
    let queued = Arc::new(Mutex::new(0usize));
    let (to_host, data) = mpsc::channel::<Vec<u8>>();
    {
        let queued = Arc::clone(&queued);
        let events = events.clone();
        thread::spawn(move || {
            let mut writer = writer;
            for chunk in data {
                if writer.write_all(&chunk).is_err() {
                    let _ = events.send(Event::Tcp(HostEvent::Reset(key)));
                    return;
                }
                let mut q = queued.lock().expect("queued lock");
                *q = (*q).saturating_sub(chunk.len());
            }
            let _ = writer.shutdown_stream(Shutdown::Write);
        });
    }
    Some(Open {
        socket: handle,
        stream,
        to_host: Some(to_host),
        to_host_queued: queued,
        from_host: VecDeque::new(),
        credit,
        host_eof: false,
        closing: false,
    })
}

fn read_host(
    key: FlowKey,
    mut reader: Box<dyn HostStream>,
    credit: &Credit,
    events: &Sender<Event>,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    while credit.wait_for_room() {
        let event = match reader.read(&mut buf) {
            Ok(0) => HostEvent::Eof(key),
            Ok(n) => {
                credit.take(n);
                HostEvent::Data(key, buf[..n].to_vec())
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => HostEvent::Reset(key),
        };
        let last = !matches!(event, HostEvent::Data(..));
        if events.send(Event::Tcp(event)).is_err() || last {
            return;
        }
    }
}
