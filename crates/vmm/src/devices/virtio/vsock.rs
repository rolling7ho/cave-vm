//! Virtio socket device (virtio 1.2, section 5.10): stream sockets between
//! the guest and the host. The host side creates no socket files, which
//! macOS would limit to 104-byte paths (decision 0008, D26).
//!
//! - **Host to guest.** [`Connector::connect`] opens a connection to a
//!   guest port, such as the agent's 1024, and returns the host end of a
//!   Unix socket pair. The VMM passes it on to the daemon.
//! - **Guest to host.** A guest connection to the host (CID 2) on port `P`
//!   goes to the handler registered with [`Vsock::serve_host_port`], and is
//!   reset if there is none or it fails.
//!
//! Each connection has a reader thread, which moves bytes from the Unix
//! socket to the guest within the credit the guest grants, and a writer
//! thread, which moves the guest's bytes to the Unix socket, so a slow host
//! peer never blocks a vCPU. Packets for the guest wait in a backlog until
//! it posts receive buffers.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use super::queue::{DescriptorChain, Queue};
use super::{ActivateError, Activation, Interrupt, VirtioDevice, device_type, interrupt};
use crate::memory::GuestMemory;

/// Size of each queue.
pub const QUEUE_SIZE: u16 = 256;
pub const RX_QUEUE: u16 = 0;
pub const TX_QUEUE: u16 = 1;
pub const EVENT_QUEUE: u16 = 2;

/// The host's context ID.
pub const HOST_CID: u64 = 2;

/// Size of `struct virtio_vsock_hdr`.
pub const HEADER_SIZE: usize = 44;

/// Receive buffer the device offers each connection: how many bytes the
/// guest may send before the host peer has read them.
pub const BUF_ALLOC: u32 = 256 * 1024;

/// Largest payload in one packet to the guest.
const MAX_PAYLOAD: usize = 64 * 1024;

/// Socket type: stream.
const TYPE_STREAM: u16 = 1;

/// Packet operations.
pub mod op {
    pub const REQUEST: u16 = 1;
    pub const RESPONSE: u16 = 2;
    pub const RST: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const RW: u16 = 5;
    pub const CREDIT_UPDATE: u16 = 6;
    pub const CREDIT_REQUEST: u16 = 7;
}

/// `SHUTDOWN` flags: what the sender will no longer do.
pub mod shutdown {
    pub const RCV: u32 = 1;
    pub const SEND: u32 = 2;
}

/// First port the device uses for host-initiated connections.
const FIRST_HOST_PORT: u32 = 1 << 30;

/// The fixed part of a packet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Header {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub kind: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl Header {
    pub fn to_bytes(self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.kind.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; HEADER_SIZE]) -> Header {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().expect("4 bytes"));
        let u16_at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().expect("2 bytes"));
        Header {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            kind: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }
}

/// A connection, named from the host's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    host_port: u32,
    guest_port: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    /// The host connected and the device sent `REQUEST`.
    Connecting,
    Connected,
}

struct Conn {
    state: ConnState,
    stream: UnixStream,
    /// Data for the writer thread. Dropped to stop it.
    writer: Option<Sender<Vec<u8>>>,
    /// The guest's receive buffer and how much of what it got it has
    /// consumed, from its latest packet.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes sent to the guest.
    tx_cnt: u32,
    /// Bytes received from the guest.
    rx_cnt: u32,
    /// Bytes the guest sent that reached the host peer.
    fwd_cnt: u32,
    /// `fwd_cnt` when the guest was last told it.
    fwd_cnt_sent: u32,
    /// The guest will send no more.
    guest_done_sending: bool,
    /// The guest will neither send nor receive; the connection is reset
    /// once the writer has delivered what the guest sent.
    guest_closed: bool,
    /// The writer thread has not finished.
    writer_running: bool,
}

impl Conn {
    /// How many more bytes the guest can take.
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc
            .saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }
}

struct ActiveQueue {
    memory: GuestMemory,
    queue: Queue,
    interrupt: Interrupt,
    broken: bool,
}

struct State {
    rx: Option<ActiveQueue>,
    backlog: VecDeque<(Header, Vec<u8>)>,
    conns: HashMap<Key, Conn>,
    next_host_port: u32,
    /// Bumped on every reset, so threads of an older driver session stop.
    generation: u64,
}

/// Opens the host side of a guest connection to a host port.
pub type HostHandler = Arc<dyn Fn() -> io::Result<UnixStream> + Send + Sync>;

struct Shared {
    cid: u64,
    /// Handlers for guest connections to the host, by host port.
    host_ports: Mutex<HashMap<u32, HostHandler>>,
    state: Mutex<State>,
    /// Signalled when credit or connection state changes.
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("vsock lock")
    }

    fn header(&self, key: Key, op: u16, conn: Option<&Conn>) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: self.cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            kind: TYPE_STREAM,
            op,
            buf_alloc: BUF_ALLOC,
            fwd_cnt: conn.map_or(0, |c| c.fwd_cnt),
            ..Header::default()
        }
    }

    /// Queues a packet for the guest and fills receive buffers.
    fn send(&self, state: &mut State, mut header: Header, data: Vec<u8>) {
        header.len = data.len() as u32;
        state.backlog.push_back((header, data));
        fill(state);
    }

    fn reset_conn(&self, state: &mut State, key: Key) {
        if let Some(conn) = state.conns.remove(&key) {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
        let header = self.header(key, op::RST, None);
        self.send(state, header, Vec::new());
        self.changed.notify_all();
    }
}

/// Moves packets from the backlog into free receive buffers.
fn fill(state: &mut State) {
    let Some(active) = &mut state.rx else {
        return;
    };
    let mut used_any = false;
    while !active.broken && !state.backlog.is_empty() {
        let chain = match active.queue.pop(&active.memory) {
            Ok(Some(chain)) => chain,
            Ok(None) => break,
            Err(_) => {
                active.broken = true;
                break;
            }
        };
        let (header, mut data) = state.backlog.pop_front().expect("not empty");
        // The guest's receive buffers are small (4 KiB in Linux), so data
        // that does not fit goes out as more packets, in order.
        let room = usize::try_from(chain.writable_len())
            .unwrap_or(usize::MAX)
            .saturating_sub(HEADER_SIZE);
        let mut header = header;
        if data.len() > room && room > 0 {
            let rest = data.split_off(room);
            state.backlog.push_front((
                Header {
                    len: rest.len() as u32,
                    ..header
                },
                rest,
            ));
            header.len = data.len() as u32;
        }
        let written = write_packet(&active.memory, &chain, header, &data).unwrap_or(0);
        if active
            .queue
            .add_used(&active.memory, chain.head, written)
            .is_err()
        {
            active.broken = true;
        }
        used_any = true;
    }
    if used_any && active.queue.needs_interrupt(&active.memory).unwrap_or(true) {
        active.interrupt.raise(interrupt::USED_BUFFER);
    }
}

fn write_packet(
    mem: &GuestMemory,
    chain: &DescriptorChain,
    header: Header,
    data: &[u8],
) -> Option<u32> {
    let len = HEADER_SIZE + data.len();
    if chain.writable_len() < len as u64 {
        return None;
    }
    chain.write_at(mem, 0, &header.to_bytes()).ok()?;
    chain.write_at(mem, HEADER_SIZE as u64, data).ok()?;
    Some(len as u32)
}

/// A virtio socket device.
pub struct Vsock {
    shared: Arc<Shared>,
    tx: Option<ActiveQueue>,
    /// The event queue, kept for snapshots until transport resets use it
    /// (P4.3).
    event: Option<Queue>,
}

impl Vsock {
    /// A device for a guest with context ID `cid`.
    pub fn new(cid: u64) -> Vsock {
        let shared = Arc::new(Shared {
            cid,
            host_ports: Mutex::new(HashMap::new()),
            state: Mutex::new(State {
                rx: None,
                backlog: VecDeque::new(),
                conns: HashMap::new(),
                next_host_port: FIRST_HOST_PORT,
                generation: 0,
            }),
            changed: Condvar::new(),
        });
        Vsock {
            shared,
            tx: None,
            event: None,
        }
    }

    /// Opens host connections to guest ports, from any thread.
    pub fn connector(&self) -> Connector {
        Connector(Arc::clone(&self.shared))
    }

    /// Sends guest connections to host `port` to `handler`, which returns
    /// the host end. It runs on the vCPU thread, so it must not block for
    /// long.
    pub fn serve_host_port(&self, port: u32, handler: HostHandler) {
        self.shared
            .host_ports
            .lock()
            .expect("host ports lock")
            .insert(port, handler);
    }

    fn config(&self) -> [u8; 8] {
        self.shared.cid.to_le_bytes()
    }

    /// Handles every packet waiting in the transmit queue.
    fn transmit(&mut self) {
        let Some(active) = &mut self.tx else {
            return;
        };
        let mut used_any = false;
        while !active.broken {
            let chain = match active.queue.pop(&active.memory) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(_) => {
                    active.broken = true;
                    break;
                }
            };
            if let Some((header, data)) = read_packet(&active.memory, &chain) {
                handle(&self.shared, header, data);
            }
            if active
                .queue
                .add_used(&active.memory, chain.head, 0)
                .is_err()
            {
                active.broken = true;
            }
            used_any = true;
        }
        if used_any && active.queue.needs_interrupt(&active.memory).unwrap_or(true) {
            active.interrupt.raise(interrupt::USED_BUFFER);
        }
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Opens host connections to guest ports. Cheap to clone.
#[derive(Clone)]
pub struct Connector(Arc<Shared>);

impl Connector {
    /// Connects to guest `port` and returns the host end. If nothing in the
    /// guest listens there, the guest resets the connection and the host
    /// end reads end-of-file. Fails if the guest's driver is not running.
    pub fn connect(&self, port: u32) -> io::Result<UnixStream> {
        let (host, device) = UnixStream::pair()?;
        let shared = &self.0;
        let mut state = shared.lock();
        if state.rx.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the guest's vsock driver is not running",
            ));
        }
        let key = loop {
            let host_port = state.next_host_port;
            state.next_host_port = state
                .next_host_port
                .checked_add(1)
                .unwrap_or(FIRST_HOST_PORT);
            let key = Key {
                host_port,
                guest_port: port,
            };
            if !state.conns.contains_key(&key) {
                break key;
            }
        };
        state
            .conns
            .insert(key, new_conn(device, ConnState::Connecting));
        let header = shared.header(key, op::REQUEST, None);
        shared.send(&mut state, header, Vec::new());
        Ok(host)
    }
}

/// Reads a packet's header and payload. `None` for a malformed packet.
fn read_packet(mem: &GuestMemory, chain: &DescriptorChain) -> Option<(Header, Vec<u8>)> {
    let mut raw = [0u8; HEADER_SIZE];
    if chain.read_at(mem, 0, &mut raw).ok()? != HEADER_SIZE {
        return None;
    }
    let header = Header::from_bytes(&raw);
    let len = header.len as usize;
    if len > MAX_PAYLOAD.max(BUF_ALLOC as usize)
        || chain.readable_len() < (HEADER_SIZE + len) as u64
    {
        return None;
    }
    let mut data = vec![0u8; len];
    if chain.read_at(mem, HEADER_SIZE as u64, &mut data).ok()? != len {
        return None;
    }
    Some((header, data))
}

/// Acts on one packet from the guest.
fn handle(shared: &Arc<Shared>, header: Header, data: Vec<u8>) {
    if header.src_cid != shared.cid || header.dst_cid != HOST_CID {
        return;
    }
    let key = Key {
        host_port: header.dst_port,
        guest_port: header.src_port,
    };
    let mut state = shared.lock();
    if header.kind != TYPE_STREAM {
        if header.op != op::RST {
            shared.reset_conn(&mut state, key);
        }
        return;
    }
    if header.op == op::REQUEST {
        drop(state);
        connect_to_host(shared, key, header);
        return;
    }
    let generation = state.generation;
    let Some(conn) = state.conns.get_mut(&key) else {
        if header.op != op::RST {
            shared.reset_conn(&mut state, key);
        }
        return;
    };
    conn.peer_buf_alloc = header.buf_alloc;
    conn.peer_fwd_cnt = header.fwd_cnt;
    shared.changed.notify_all();
    match header.op {
        op::RESPONSE if conn.state == ConnState::Connecting => {
            conn.state = ConnState::Connected;
            start_threads(shared, key, generation, &mut state);
        }
        op::RW if conn.state == ConnState::Connected && !conn.guest_done_sending => {
            let len = data.len() as u32;
            let in_flight = conn.rx_cnt.wrapping_add(len).wrapping_sub(conn.fwd_cnt);
            let sent = in_flight <= BUF_ALLOC
                && conn.writer.as_ref().is_some_and(|w| w.send(data).is_ok());
            if sent {
                conn.rx_cnt = conn.rx_cnt.wrapping_add(len);
            } else {
                // The guest overran the credit it was given.
                shared.reset_conn(&mut state, key);
            }
        }
        op::SHUTDOWN => {
            if header.flags & shutdown::SEND != 0 {
                conn.guest_done_sending = true;
                // The writer shuts down the write side once it drains.
                conn.writer = None;
            }
            if header.flags & shutdown::RCV != 0 && conn.guest_done_sending {
                if conn.writer_running {
                    conn.guest_closed = true;
                } else {
                    shared.reset_conn(&mut state, key);
                }
            }
        }
        op::RST => {
            if let Some(conn) = state.conns.remove(&key) {
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
            shared.changed.notify_all();
        }
        op::CREDIT_REQUEST => {
            let header = shared.header(key, op::CREDIT_UPDATE, Some(conn));
            conn.fwd_cnt_sent = conn.fwd_cnt;
            shared.send(&mut state, header, Vec::new());
        }
        op::CREDIT_UPDATE | op::RESPONSE | op::RW => {}
        _ => shared.reset_conn(&mut state, key),
    }
}

/// A guest connection to the host: ask the port's handler for the host end.
fn connect_to_host(shared: &Arc<Shared>, key: Key, request: Header) {
    let handler = shared
        .host_ports
        .lock()
        .expect("host ports lock")
        .get(&key.host_port)
        .cloned();
    let stream = handler.map_or_else(|| Err(io::ErrorKind::ConnectionRefused.into()), |h| h());
    let mut state = shared.lock();
    let Ok(stream) = stream else {
        shared.reset_conn(&mut state, key);
        return;
    };
    if state.conns.contains_key(&key) {
        shared.reset_conn(&mut state, key);
        return;
    }
    let mut conn = new_conn(stream, ConnState::Connected);
    conn.peer_buf_alloc = request.buf_alloc;
    conn.peer_fwd_cnt = request.fwd_cnt;
    state.conns.insert(key, conn);
    let generation = state.generation;
    start_threads(shared, key, generation, &mut state);
    let conn = state.conns.get(&key);
    let header = shared.header(key, op::RESPONSE, conn);
    shared.send(&mut state, header, Vec::new());
}

fn new_conn(stream: UnixStream, state: ConnState) -> Conn {
    Conn {
        state,
        stream,
        writer: None,
        peer_buf_alloc: 0,
        peer_fwd_cnt: 0,
        tx_cnt: 0,
        rx_cnt: 0,
        fwd_cnt: 0,
        fwd_cnt_sent: 0,
        guest_done_sending: false,
        guest_closed: false,
        writer_running: false,
    }
}

/// Starts the reader and writer threads of a connected connection.
fn start_threads(shared: &Arc<Shared>, key: Key, generation: u64, state: &mut State) {
    let Some(conn) = state.conns.get_mut(&key) else {
        return;
    };
    let (Ok(read_half), Ok(write_half)) = (conn.stream.try_clone(), conn.stream.try_clone()) else {
        shared.reset_conn(state, key);
        return;
    };
    // Unbounded in messages, but the guest's credit bounds the bytes queued
    // to BUF_ALLOC.
    let (tx, rx) = mpsc::channel();
    conn.writer = Some(tx);
    conn.writer_running = true;
    let reader_shared = Arc::clone(shared);
    thread::spawn(move || reader(&reader_shared, key, generation, read_half));
    let writer_shared = Arc::clone(shared);
    thread::spawn(move || writer(&writer_shared, key, generation, write_half, rx));
}

/// Host peer to guest, within the guest's credit.
fn reader(shared: &Shared, key: Key, generation: u64, mut stream: UnixStream) {
    let mut buf = vec![0u8; MAX_PAYLOAD];
    loop {
        // Wait for credit.
        let want = {
            let mut state = shared.lock();
            loop {
                if state.generation != generation {
                    return;
                }
                let Some(conn) = state.conns.get(&key) else {
                    return;
                };
                let credit = conn.peer_credit() as usize;
                if credit > 0 {
                    break credit.min(MAX_PAYLOAD);
                }
                state = shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(500))
                    .expect("vsock lock")
                    .0;
            }
        };
        let n = match stream.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        let mut state = shared.lock();
        if state.generation != generation {
            return;
        }
        let Some(conn) = state.conns.get_mut(&key) else {
            return;
        };
        if n == 0 {
            // The host peer will send no more.
            let header = shared.header(key, op::SHUTDOWN, Some(conn));
            let header = Header {
                flags: shutdown::SEND,
                ..header
            };
            conn.fwd_cnt_sent = conn.fwd_cnt;
            shared.send(&mut state, header, Vec::new());
            return;
        }
        conn.tx_cnt = conn.tx_cnt.wrapping_add(n as u32);
        conn.fwd_cnt_sent = conn.fwd_cnt;
        let header = shared.header(key, op::RW, Some(conn));
        shared.send(&mut state, header, buf[..n].to_vec());
    }
}

/// Guest to host peer.
fn writer(
    shared: &Shared,
    key: Key,
    generation: u64,
    mut stream: UnixStream,
    data: Receiver<Vec<u8>>,
) {
    for chunk in data {
        if stream.write_all(&chunk).is_err() {
            let mut state = shared.lock();
            if state.generation == generation && state.conns.contains_key(&key) {
                shared.reset_conn(&mut state, key);
            }
            return;
        }
        let mut state = shared.lock();
        if state.generation != generation {
            return;
        }
        let Some(conn) = state.conns.get_mut(&key) else {
            return;
        };
        conn.fwd_cnt = conn.fwd_cnt.wrapping_add(chunk.len() as u32);
        // Tell the guest about freed space once half its credit is back.
        if conn.fwd_cnt.wrapping_sub(conn.fwd_cnt_sent) >= BUF_ALLOC / 2 {
            conn.fwd_cnt_sent = conn.fwd_cnt;
            let header = shared.header(key, op::CREDIT_UPDATE, Some(conn));
            shared.send(&mut state, header, Vec::new());
        }
    }
    // The guest shut down its sending side and everything was written.
    let _ = stream.shutdown(Shutdown::Write);
    let mut state = shared.lock();
    if state.generation != generation {
        return;
    }
    if let Some(conn) = state.conns.get_mut(&key) {
        conn.writer_running = false;
        if conn.guest_closed {
            shared.reset_conn(&mut state, key);
        }
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        device_type::VSOCK
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::read_config_bytes(&self.config(), offset, data);
    }

    fn activate(&mut self, activation: Activation) -> Result<(), ActivateError> {
        let mut queues = activation.queues.into_iter();
        let (Some(rx), Some(tx), Some(event)) = (queues.next(), queues.next(), queues.next())
        else {
            return Err(ActivateError("the vsock device needs three queues".into()));
        };
        if !rx.ready || !tx.ready {
            return Err(ActivateError(
                "the vsock receive and transmit queues must both be ready".into(),
            ));
        }
        let active = |queue| ActiveQueue {
            memory: activation.memory.clone(),
            queue,
            interrupt: activation.interrupt.clone(),
            broken: false,
        };
        self.tx = Some(active(tx));
        self.event = Some(event);
        let mut state = self.shared.lock();
        state.rx = Some(active(rx));
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        match index {
            RX_QUEUE => fill(&mut self.shared.lock()),
            TX_QUEUE => self.transmit(),
            // Events go from the device to the driver only (transport
            // resets after a restore, phase 4).
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.tx = None;
        self.event = None;
        let mut state = self.shared.lock();
        state.rx = None;
        state.backlog.clear();
        state.generation += 1;
        for (_, conn) in state.conns.drain() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
        self.shared.changed.notify_all();
    }

    /// Connections to the host are not saved: they end with the process,
    /// and the guest learns of it from a transport reset (P4.3).
    fn save_queues(&self) -> Vec<Queue> {
        let rx = self.shared.lock().rx.as_ref().map(|a| a.queue.clone());
        [
            rx,
            self.tx.as_ref().map(|a| a.queue.clone()),
            self.event.clone(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

#[cfg(test)]
mod tests;
