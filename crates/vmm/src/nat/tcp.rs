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
use smoltcp::socket::tcp;
use smoltcp::wire::IpListenEndpoint;

use super::Event;
use super::host::{Connector, HostStream};

/// Bytes each direction may have in flight inside the gateway.
pub const WINDOW: usize = 256 * 1024;

/// Bytes read from a host socket at a time.
const READ_CHUNK: usize = 16 * 1024;

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
#[derive(Default)]
pub struct Flows {
    flows: HashMap<FlowKey, Phase>,
}

impl Flows {
    pub fn contains(&self, key: &FlowKey) -> bool {
        self.flows.contains_key(key)
    }

    pub fn is_connecting(&self, key: &FlowKey) -> bool {
        matches!(self.flows.get(key), Some(Phase::Connecting { .. }))
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
