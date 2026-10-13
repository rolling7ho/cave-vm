//! Virtio network device (virtio 1.2, section 5.1).
//!
//! Frames the guest sends go to a [`NetBackend`] on the vCPU thread that
//! notified the transmit queue. Frames for the guest arrive on whatever
//! thread runs the network stack, through a [`NetReceiver`]; it fills the
//! receive queue directly, so it never waits for the device lock a vCPU
//! holds.
//!
//! No offloads are offered (checksum, segmentation, mergeable buffers), so
//! every frame fits one receive buffer of the size Linux allocates for a
//! 1500-byte MTU. The link is always up.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::queue::{DescriptorChain, Queue};
use super::{ActivateError, Activation, Interrupt, VirtioDevice, device_type, interrupt};
use crate::memory::GuestMemory;

/// Size of each queue.
pub const QUEUE_SIZE: u16 = 256;
/// The receive queue's index.
pub const RX_QUEUE: u16 = 0;
/// The transmit queue's index.
pub const TX_QUEUE: u16 = 1;

/// Feature bits (virtio 1.2, 5.1.3).
pub mod features {
    pub const MAC: u64 = 1 << 5;
    pub const STATUS: u64 = 1 << 16;
}

/// `status` config field: the link is up.
const LINK_UP: u16 = 1;

/// Size of `struct virtio_net_hdr` with `num_buffers`, which virtio 1.0 and
/// later always include.
pub const HEADER_SIZE: usize = 12;

/// Largest frame accepted from the guest. Without segmentation offloads a
/// frame is at most the MTU plus the Ethernet header; this leaves room for
/// a larger MTU without letting a bad driver make the device buffer
/// arbitrary amounts.
pub const MAX_FRAME: usize = 65535;

/// Frames kept for the guest while it has no free receive buffer. Beyond
/// this, frames are dropped, as a real NIC would.
pub const RX_BACKLOG: usize = 256;

/// Where frames the guest sends go: the userspace network stack, or a test.
/// Called with the device lock held on a vCPU thread, so it must not block.
pub trait NetBackend: Send {
    /// One Ethernet frame, without the virtio header.
    fn send(&mut self, frame: &[u8]);
}

struct ActiveQueue {
    memory: GuestMemory,
    queue: Queue,
    interrupt: Interrupt,
    /// Set after a malformed queue; the device stops serving it until the
    /// driver resets it.
    broken: bool,
}

impl ActiveQueue {
    fn finish(&mut self, used_any: bool) {
        if used_any && self.queue.needs_interrupt(&self.memory).unwrap_or(true) {
            self.interrupt.raise(interrupt::USED_BUFFER);
        }
    }
}

#[derive(Default)]
struct Rx {
    active: Option<ActiveQueue>,
    backlog: VecDeque<Vec<u8>>,
    /// Frames wait in the backlog while a snapshot is taken.
    paused: bool,
}

impl Rx {
    /// Moves frames from the backlog into free receive buffers.
    fn fill(&mut self) {
        if self.paused {
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        let mut used_any = false;
        while !active.broken && !self.backlog.is_empty() {
            let chain = match active.queue.pop(&active.memory) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(_) => {
                    active.broken = true;
                    break;
                }
            };
            let frame = self.backlog.pop_front().expect("not empty");
            // A buffer too small for the frame, or outside guest memory, is
            // returned empty and the frame is dropped.
            let written = write_frame(&active.memory, &chain, &frame).unwrap_or(0);
            if active
                .queue
                .add_used(&active.memory, chain.head, written)
                .is_err()
            {
                active.broken = true;
            }
            used_any = true;
        }
        active.finish(used_any);
    }
}

/// Writes the header and `frame` into a receive buffer. Returns the bytes
/// written, or `None` if it does not fit.
fn write_frame(mem: &GuestMemory, chain: &DescriptorChain, frame: &[u8]) -> Option<u32> {
    let len = HEADER_SIZE + frame.len();
    if chain.writable_len() < len as u64 {
        return None;
    }
    let mut header = [0u8; HEADER_SIZE];
    // num_buffers: the frame is in this one buffer.
    header[10..12].copy_from_slice(&1u16.to_le_bytes());
    chain.write_at(mem, 0, &header).ok()?;
    chain.write_at(mem, HEADER_SIZE as u64, frame).ok()?;
    Some(len as u32)
}

/// Delivers frames to the guest from any thread. Cheap to clone.
#[derive(Clone)]
pub struct NetReceiver {
    rx: Arc<Mutex<Rx>>,
}

impl NetReceiver {
    /// Hands one Ethernet frame to the guest. Returns `false` if it was
    /// dropped: the driver is not running, the backlog is full, or the frame
    /// is too large.
    pub fn deliver(&self, frame: &[u8]) -> bool {
        let mut rx = self.rx.lock().expect("net rx lock");
        if rx.active.is_none() || rx.backlog.len() >= RX_BACKLOG || frame.len() > MAX_FRAME {
            return false;
        }
        rx.backlog.push_back(frame.to_vec());
        rx.fill();
        true
    }
}

/// A virtio network device.
pub struct Net {
    mac: [u8; 6],
    backend: Box<dyn NetBackend>,
    rx: Arc<Mutex<Rx>>,
    tx: Option<ActiveQueue>,
}

impl Net {
    /// A device with hardware address `mac`, sending the guest's frames to
    /// `backend`.
    pub fn new(mac: [u8; 6], backend: Box<dyn NetBackend>) -> Net {
        Net {
            mac,
            backend,
            rx: Arc::default(),
            tx: None,
        }
    }

    /// The handle the network stack uses to deliver frames to the guest.
    pub fn receiver(&self) -> NetReceiver {
        NetReceiver {
            rx: Arc::clone(&self.rx),
        }
    }

    fn config(&self) -> [u8; 8] {
        let mut config = [0u8; 8];
        config[..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&LINK_UP.to_le_bytes());
        config
    }

    /// Sends every frame waiting in the transmit queue.
    fn transmit(&mut self) {
        let Some(active) = &mut self.tx else {
            return;
        };
        let mut used_any = false;
        let mut frame = Vec::new();
        while !active.broken {
            let chain = match active.queue.pop(&active.memory) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(_) => {
                    active.broken = true;
                    break;
                }
            };
            let len = chain.readable_len().saturating_sub(HEADER_SIZE as u64);
            // A chain with no frame, a frame over the limit, or one outside
            // guest memory is dropped.
            if len > 0 && len <= MAX_FRAME as u64 {
                frame.resize(len as usize, 0);
                if chain
                    .read_at(&active.memory, HEADER_SIZE as u64, &mut frame)
                    .is_ok_and(|n| n == frame.len())
                {
                    self.backend.send(&frame);
                }
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
        active.finish(used_any);
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        device_type::NET
    }

    fn features(&self) -> u64 {
        features::MAC | features::STATUS
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::read_config_bytes(&self.config(), offset, data);
    }

    fn activate(&mut self, activation: Activation) -> Result<(), ActivateError> {
        let mut queues = activation.queues.into_iter();
        let (Some(rx), Some(tx)) = (queues.next(), queues.next()) else {
            return Err(ActivateError("the net device needs two queues".into()));
        };
        if !rx.ready || !tx.ready {
            return Err(ActivateError(
                "the net receive and transmit queues must both be ready".into(),
            ));
        }
        let active = |queue| ActiveQueue {
            memory: activation.memory.clone(),
            queue,
            interrupt: activation.interrupt.clone(),
            broken: false,
        };
        self.tx = Some(active(tx));
        let mut shared = self.rx.lock().expect("net rx lock");
        shared.backlog.clear();
        shared.active = Some(active(rx));
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        match index {
            RX_QUEUE => self.rx.lock().expect("net rx lock").fill(),
            TX_QUEUE => self.transmit(),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.tx = None;
        let mut rx = self.rx.lock().expect("net rx lock");
        rx.active = None;
        rx.backlog.clear();
    }

    fn pause(&mut self) {
        self.rx.lock().expect("net rx lock").paused = true;
    }

    fn resume(&mut self) {
        let mut rx = self.rx.lock().expect("net rx lock");
        rx.paused = false;
        rx.fill();
    }

    /// Frames waiting in the backlog are not saved: they are dropped, as
    /// a network may drop any frame.
    fn save_queues(&self) -> Vec<Queue> {
        let rx = self.rx.lock().expect("net rx lock");
        [rx.active.as_ref(), self.tx.as_ref()]
            .into_iter()
            .flatten()
            .map(|a| a.queue.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::testing::{DriverQueue, guest_memory};

    const MAC: [u8; 6] = [0x02, 0xca, 0x5e, 0x00, 0x00, 0x01];

    /// A backend that keeps every frame the guest sent.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);

    impl NetBackend for Recorder {
        fn send(&mut self, frame: &[u8]) {
            self.0.lock().unwrap().push(frame.to_vec());
        }
    }

    struct Rig {
        mem: GuestMemory,
        rxq: DriverQueue,
        txq: DriverQueue,
        net: Net,
        sent: Recorder,
        receiver: NetReceiver,
        interrupt: Interrupt,
    }

    fn rig() -> Rig {
        let mem = guest_memory();
        let rxq = DriverQueue::at(&mem, 8, 0);
        let txq = DriverQueue::at(&mem, 8, 1);
        let sent = Recorder::default();
        let mut net = Net::new(MAC, Box::new(sent.clone()));
        let interrupt = Interrupt::new();
        net.activate(Activation {
            memory: mem.clone(),
            queues: vec![rxq.device_queue(), txq.device_queue()],
            features: features::MAC | features::STATUS,
            interrupt: interrupt.clone(),
        })
        .unwrap();
        let receiver = net.receiver();
        Rig {
            mem,
            rxq,
            txq,
            net,
            sent,
            receiver,
            interrupt,
        }
    }

    fn frame(n: usize) -> Vec<u8> {
        (0..n).map(|i| i as u8).collect()
    }

    #[test]
    fn config_has_the_mac_and_link_up() {
        let net = Net::new(MAC, Box::new(Recorder::default()));
        let mut config = [0u8; 8];
        net.read_config(0, &mut config);
        assert_eq!(&config[..6], &MAC);
        assert_eq!(u16::from_le_bytes([config[6], config[7]]), LINK_UP);
        assert_eq!(net.features(), features::MAC | features::STATUS);
        assert_eq!(net.queue_max_sizes().len(), 2);
    }

    #[test]
    fn guest_frames_reach_the_backend_without_the_header() {
        let mut r = rig();
        let f = frame(60);
        let head = r.txq.add(&r.mem, &[&[0u8; HEADER_SIZE], &f], &[]);
        // A frame split across descriptors, header and data together.
        let mut joined = vec![0u8; HEADER_SIZE];
        joined.extend_from_slice(&frame(42));
        let second = r.txq.add(&r.mem, &[&joined[..20], &joined[20..]], &[]);
        r.net.queue_notify(TX_QUEUE);
        assert_eq!(*r.sent.0.lock().unwrap(), vec![f, frame(42)]);
        assert_eq!(
            r.txq.used(&r.mem),
            vec![(u32::from(head), 0), (u32::from(second), 0)]
        );
        assert!(r.interrupt.level());
    }

    #[test]
    fn empty_and_oversized_guest_frames_are_dropped_and_returned() {
        let mut r = rig();
        let short = r.txq.add(&r.mem, &[&[0u8; 8]], &[]);
        let big = vec![0u8; HEADER_SIZE + MAX_FRAME + 1];
        let over = r.txq.add(&r.mem, &[&big], &[]);
        r.net.queue_notify(TX_QUEUE);
        assert!(r.sent.0.lock().unwrap().is_empty());
        assert_eq!(
            r.txq.used(&r.mem),
            vec![(u32::from(short), 0), (u32::from(over), 0)]
        );
    }

    #[test]
    fn delivered_frames_land_in_receive_buffers_with_a_header() {
        let mut r = rig();
        let head = r.rxq.add(&r.mem, &[], &[1526]);
        let f = frame(100);
        assert!(r.receiver.deliver(&f));
        let len = (HEADER_SIZE + f.len()) as u32;
        assert_eq!(r.rxq.used(&r.mem), vec![(u32::from(head), len)]);
        let written = r.rxq.written(&r.mem, head, 0, len as usize);
        let mut expect = vec![0u8; HEADER_SIZE];
        expect[10] = 1;
        expect.extend_from_slice(&f);
        assert_eq!(written, expect);
        assert!(r.interrupt.level());
    }

    #[test]
    fn frames_wait_for_buffers_and_overflow_is_dropped() {
        let mut r = rig();
        for i in 0..RX_BACKLOG {
            assert!(r.receiver.deliver(&[i as u8; 60]), "frame {i}");
        }
        assert!(!r.receiver.deliver(&[0xff; 60]), "backlog full");
        assert!(!r.interrupt.level());

        let first = r.rxq.add(&r.mem, &[], &[1526]);
        let second = r.rxq.add(&r.mem, &[], &[1526]);
        r.net.queue_notify(RX_QUEUE);
        let used = r.rxq.used(&r.mem);
        assert_eq!(used.len(), 2);
        assert_eq!(r.rxq.written(&r.mem, first, HEADER_SIZE as u64, 1), [0]);
        assert_eq!(r.rxq.written(&r.mem, second, HEADER_SIZE as u64, 1), [1]);
        assert!(r.receiver.deliver(&[0xfe; 60]), "room again");
    }

    #[test]
    fn a_buffer_too_small_is_returned_empty() {
        let mut r = rig();
        let small = r.rxq.add(&r.mem, &[], &[HEADER_SIZE as u32 + 10]);
        let next = r.rxq.add(&r.mem, &[], &[1526]);
        assert!(r.receiver.deliver(&frame(60)));
        assert!(r.receiver.deliver(&frame(5)));
        assert_eq!(
            r.rxq.used(&r.mem),
            vec![
                (u32::from(small), 0),
                (u32::from(next), HEADER_SIZE as u32 + 5)
            ]
        );
    }

    #[test]
    fn frames_before_activation_or_after_reset_are_dropped() {
        let net = Net::new(MAC, Box::new(Recorder::default()));
        assert!(!net.receiver().deliver(&frame(60)));

        let mut r = rig();
        assert!(r.receiver.deliver(&frame(60)), "kept for a buffer");
        r.net.reset();
        assert!(!r.receiver.deliver(&frame(60)));
        r.rxq.add(&r.mem, &[], &[1526]);
        r.net.queue_notify(RX_QUEUE);
        assert!(r.rxq.used(&r.mem).is_empty(), "backlog cleared on reset");
    }

    #[test]
    fn activation_needs_both_queues_ready() {
        let mem = guest_memory();
        let rxq = DriverQueue::at(&mem, 8, 0);
        let mut not_ready = DriverQueue::at(&mem, 8, 1).device_queue();
        not_ready.ready = false;
        let mut net = Net::new(MAC, Box::new(Recorder::default()));
        let activation = |queues| Activation {
            memory: mem.clone(),
            queues,
            features: 0,
            interrupt: Interrupt::new(),
        };
        assert!(net.activate(activation(vec![rxq.device_queue()])).is_err());
        assert!(
            net.activate(activation(vec![rxq.device_queue(), not_ready]))
                .is_err()
        );
    }

    #[test]
    fn frames_can_be_delivered_from_another_thread() {
        let mut r = rig();
        for _ in 0..4 {
            r.rxq.add(&r.mem, &[], &[1526]);
        }
        let receiver = r.receiver.clone();
        std::thread::spawn(move || {
            for i in 0..4u8 {
                assert!(receiver.deliver(&[i; 60]));
            }
        })
        .join()
        .unwrap();
        assert_eq!(r.rxq.used(&r.mem).len(), 4);
    }

    #[test]
    fn a_restored_device_continues_from_the_saved_queues() {
        let mut r = rig();
        let tx1 = r.txq.add(&r.mem, &[&[0u8; HEADER_SIZE], &frame(60)], &[]);
        r.net.queue_notify(TX_QUEUE);
        let rx1 = r.rxq.add(&r.mem, &[], &[2048]);
        let rx2 = r.rxq.add(&r.mem, &[], &[2048]);
        r.net.queue_notify(RX_QUEUE);
        assert!(r.receiver.deliver(&frame(70)));
        let tx2 = r.txq.add(&r.mem, &[&[0u8; HEADER_SIZE], &frame(61)], &[]);
        let saved = r.net.save_queues();
        assert_eq!(saved.len(), 2);
        assert_eq!((saved[0].next_avail, saved[1].next_avail), (1, 1));

        let sent = Recorder::default();
        let mut net = Net::new(MAC, Box::new(sent.clone()));
        net.activate(Activation {
            memory: r.mem.clone(),
            queues: saved.clone(),
            features: features::MAC | features::STATUS,
            interrupt: Interrupt::new(),
        })
        .unwrap();
        assert_eq!(net.save_queues(), saved);
        net.queue_notify(TX_QUEUE);
        assert_eq!(*sent.0.lock().unwrap(), vec![frame(61)]);
        assert_eq!(
            r.txq.used(&r.mem),
            vec![(u32::from(tx1), 0), (u32::from(tx2), 0)]
        );
        assert!(net.receiver().deliver(&frame(71)));
        let len = (HEADER_SIZE + 70) as u32;
        assert_eq!(
            r.rxq.used(&r.mem),
            vec![(u32::from(rx1), len), (u32::from(rx2), len + 1)]
        );
    }

    #[test]
    fn frames_wait_while_paused() {
        let mut r = rig();
        let head = r.rxq.add(&r.mem, &[], &[2048]);
        r.net.queue_notify(RX_QUEUE);
        r.net.pause();
        assert!(r.receiver.deliver(&frame(60)));
        assert!(r.rxq.used(&r.mem).is_empty(), "guest memory untouched");
        r.net.resume();
        assert_eq!(
            r.rxq.used(&r.mem),
            vec![(u32::from(head), (HEADER_SIZE + 60) as u32)]
        );
    }
}
