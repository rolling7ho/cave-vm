//! The virtio-mmio transport, version 2 (virtio 1.2, section 4.2).
//!
//! Registers are 32 bits wide and little-endian. Device configuration space
//! starts at offset 0x100 and may be accessed in 1, 2 or 4 byte units.

use super::{Activation, Interrupt, Queue, VirtioDevice, features, interrupt as irq, status};
use crate::memory::GuestMemory;

/// "virt" in little-endian ASCII.
pub const MAGIC: u32 = 0x7472_6976;
/// The non-legacy transport.
pub const VERSION: u32 = 2;
/// "cave" in little-endian ASCII. The vendor ID is informational.
pub const VENDOR_ID: u32 = 0x6576_6163;

// Register offsets.
const MAGIC_VALUE: u64 = 0x000;
const VERSION_REG: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID_REG: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const SHM_LEN_LOW: u64 = 0x0b0;
const SHM_LEN_HIGH: u64 = 0x0b4;
const SHM_BASE_LOW: u64 = 0x0b8;
const SHM_BASE_HIGH: u64 = 0x0bc;
const CONFIG_GENERATION: u64 = 0x0fc;
/// Start of device configuration space.
pub const CONFIG: u64 = 0x100;

/// One virtio-mmio slot: the transport registers in front of a device.
pub struct MmioTransport {
    device: Box<dyn VirtioDevice>,
    memory: GuestMemory,
    interrupt: Interrupt,
    queues: Vec<Queue>,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    config_generation: u32,
    activated: bool,
}

impl MmioTransport {
    pub fn new(device: Box<dyn VirtioDevice>, memory: GuestMemory) -> MmioTransport {
        let queues = device
            .queue_max_sizes()
            .iter()
            .map(|&max| Queue::new(max))
            .collect();
        MmioTransport {
            device,
            memory,
            interrupt: Interrupt::new(),
            queues,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            config_generation: 0,
            activated: false,
        }
    }

    /// The device's interrupt, for the run loop to follow and for setting
    /// the vCPU kick.
    pub fn interrupt(&self) -> &Interrupt {
        &self.interrupt
    }

    /// The device status register.
    pub fn status(&self) -> u32 {
        self.status
    }

    fn device_features(&self) -> u64 {
        self.device.features() | features::VERSION_1
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Whether the driver may still change queue configuration.
    fn queue_writable(&mut self) -> Option<&mut Queue> {
        if self.status & status::DRIVER_OK != 0 {
            return None;
        }
        self.selected_queue().filter(|q| !q.ready)
    }

    /// Handles a guest read of `data.len()` bytes at `offset`.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        if offset >= CONFIG {
            self.device.read_config(offset - CONFIG, data);
            return;
        }
        if data.len() != 4 {
            data.fill(0);
            return;
        }
        let value = match offset {
            MAGIC_VALUE => MAGIC,
            VERSION_REG => VERSION,
            DEVICE_ID => self.device.device_type(),
            VENDOR_ID_REG => VENDOR_ID,
            DEVICE_FEATURES => match self.device_features_sel {
                0 => self.device_features() as u32,
                1 => (self.device_features() >> 32) as u32,
                _ => 0,
            },
            QUEUE_NUM_MAX => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.max_size)),
            QUEUE_READY => self
                .queues
                .get(self.queue_sel as usize)
                .map_or(0, |q| u32::from(q.ready)),
            INTERRUPT_STATUS => self.interrupt.status(),
            STATUS => self.status,
            // No shared memory regions: a length of all ones means absent.
            SHM_LEN_LOW | SHM_LEN_HIGH | SHM_BASE_LOW | SHM_BASE_HIGH => u32::MAX,
            CONFIG_GENERATION => self.config_generation,
            _ => 0,
        };
        data.copy_from_slice(&value.to_le_bytes());
    }

    /// Handles a guest write of `data` at `offset`.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= CONFIG {
            self.device.write_config(offset - CONFIG, data);
            self.config_generation = self.config_generation.wrapping_add(1);
            return;
        }
        let Ok(raw) = <[u8; 4]>::try_from(data) else {
            return;
        };
        let value = u32::from_le_bytes(raw);
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = value,
            DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            DRIVER_FEATURES => {
                if self.status & status::FEATURES_OK == 0 {
                    let shift = match self.driver_features_sel {
                        0 => 0,
                        1 => 32,
                        _ => return,
                    };
                    self.driver_features &= !(0xffff_ffffu64 << shift);
                    self.driver_features |= u64::from(value) << shift;
                }
            }
            QUEUE_SEL => self.queue_sel = value,
            QUEUE_NUM => {
                if let Some(q) = self.queue_writable() {
                    q.size = value as u16;
                }
            }
            QUEUE_READY => self.set_queue_ready(value == 1),
            QUEUE_DESC_LOW => self.set_queue_addr(|q| &mut q.desc_table, value, false),
            QUEUE_DESC_HIGH => self.set_queue_addr(|q| &mut q.desc_table, value, true),
            QUEUE_DRIVER_LOW => self.set_queue_addr(|q| &mut q.avail_ring, value, false),
            QUEUE_DRIVER_HIGH => self.set_queue_addr(|q| &mut q.avail_ring, value, true),
            QUEUE_DEVICE_LOW => self.set_queue_addr(|q| &mut q.used_ring, value, false),
            QUEUE_DEVICE_HIGH => self.set_queue_addr(|q| &mut q.used_ring, value, true),
            QUEUE_NOTIFY => {
                if self.activated && (value as usize) < self.queues.len() {
                    self.device.queue_notify(value as u16);
                }
            }
            INTERRUPT_ACK => self.interrupt.ack(value),
            STATUS => self.set_status(value),
            _ => {}
        }
    }

    fn set_queue_addr(&mut self, field: fn(&mut Queue) -> &mut u64, value: u32, high: bool) {
        if let Some(q) = self.queue_writable() {
            let addr = field(q);
            *addr = if high {
                (*addr & 0xffff_ffff) | (u64::from(value) << 32)
            } else {
                (*addr & !0xffff_ffff) | u64::from(value)
            };
        }
    }

    fn set_queue_ready(&mut self, ready: bool) {
        if self.status & status::DRIVER_OK != 0 {
            return;
        }
        if let Some(q) = self.selected_queue() {
            q.ready = ready;
        }
    }

    fn set_status(&mut self, value: u32) {
        if value == 0 {
            self.reset();
            return;
        }
        // The driver only ever adds bits; anything else is a driver bug, and
        // the spec lets the device ignore it.
        if value & self.status != self.status {
            return;
        }
        let added = value & !self.status;
        let mut value = value;
        if added & status::FEATURES_OK != 0 && !self.features_acceptable() {
            value &= !status::FEATURES_OK;
        }
        self.status = value;
        if added & status::DRIVER_OK != 0 && !self.activated {
            self.activate();
        }
    }

    /// The driver accepted only offered features, and the non-legacy
    /// interface.
    fn features_acceptable(&self) -> bool {
        self.driver_features & !self.device_features() == 0
            && self.driver_features & features::VERSION_1 != 0
    }

    fn activate(&mut self) {
        let usable = self.status & status::FEATURES_OK != 0
            && self
                .queues
                .iter()
                .all(|q| !q.ready || q.is_valid(&self.memory));
        if !usable {
            self.needs_reset();
            return;
        }
        let activation = Activation {
            memory: self.memory.clone(),
            queues: self.queues.clone(),
            features: self.driver_features,
            interrupt: self.interrupt.clone(),
        };
        match self.device.activate(activation) {
            Ok(()) => self.activated = true,
            Err(_) => self.needs_reset(),
        }
    }

    fn needs_reset(&mut self) {
        self.status |= status::DEVICE_NEEDS_RESET;
        self.interrupt.raise(irq::CONFIG_CHANGE);
    }

    fn reset(&mut self) {
        if self.activated {
            self.device.reset();
        }
        for q in &mut self.queues {
            q.reset();
        }
        self.interrupt.clear();
        self.status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.activated = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::testing::{DriverQueue, guest_memory};
    use crate::devices::virtio::{ActivateError, read_config_bytes};
    use std::sync::{Arc, Mutex};

    /// A device that copies each request's readable bytes back into its
    /// writable part, uppercased.
    #[derive(Default)]
    struct Echo {
        log: Arc<Mutex<Vec<String>>>,
        active: Option<Activation>,
        fail_activation: bool,
    }

    impl VirtioDevice for Echo {
        fn device_type(&self) -> u32 {
            42
        }
        fn features(&self) -> u64 {
            1 << 3
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &[8, 4]
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            read_config_bytes(&[0xaa, 0xbb, 0xcc, 0xdd], offset, data);
        }
        fn activate(&mut self, activation: Activation) -> Result<(), ActivateError> {
            self.log.lock().unwrap().push("activate".into());
            if self.fail_activation {
                return Err(ActivateError("no".into()));
            }
            self.active = Some(activation);
            Ok(())
        }
        fn queue_notify(&mut self, index: u16) {
            self.log.lock().unwrap().push(format!("notify {index}"));
            let a = self.active.as_mut().unwrap();
            let q = &mut a.queues[index as usize];
            while let Some(chain) = q.pop(&a.memory).unwrap() {
                let mut buf = vec![0u8; chain.readable_len() as usize];
                chain.read_at(&a.memory, 0, &mut buf).unwrap();
                buf.make_ascii_uppercase();
                let n = chain.write_at(&a.memory, 0, &buf).unwrap();
                q.add_used(&a.memory, chain.head, n as u32).unwrap();
            }
            if q.needs_interrupt(&a.memory).unwrap() {
                a.interrupt.raise(irq::USED_BUFFER);
            }
        }
        fn reset(&mut self) {
            self.log.lock().unwrap().push("reset".into());
            self.active = None;
        }
    }

    fn r32(t: &MmioTransport, offset: u64) -> u32 {
        let mut b = [0u8; 4];
        t.read(offset, &mut b);
        u32::from_le_bytes(b)
    }

    fn w32(t: &mut MmioTransport, offset: u64, value: u32) {
        t.write(offset, &value.to_le_bytes());
    }

    fn transport(echo: Echo) -> (MmioTransport, GuestMemory) {
        let mem = guest_memory();
        (MmioTransport::new(Box::new(echo), mem.clone()), mem)
    }

    /// Runs the driver initialization sequence (virtio 1.2, 3.1.1) and sets
    /// up the given queues.
    fn init(t: &mut MmioTransport, queues: &[(u32, &DriverQueue)]) {
        w32(t, STATUS, 0);
        w32(t, STATUS, status::ACKNOWLEDGE);
        w32(t, STATUS, status::ACKNOWLEDGE | status::DRIVER);
        w32(t, DEVICE_FEATURES_SEL, 1);
        let high = r32(t, DEVICE_FEATURES);
        w32(t, DRIVER_FEATURES_SEL, 1);
        w32(t, DRIVER_FEATURES, high);
        w32(t, DRIVER_FEATURES_SEL, 0);
        w32(t, DRIVER_FEATURES, 0);
        let s = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
        w32(t, STATUS, s);
        assert_eq!(r32(t, STATUS) & status::FEATURES_OK, status::FEATURES_OK);
        for &(index, q) in queues {
            w32(t, QUEUE_SEL, index);
            w32(t, QUEUE_NUM, u32::from(q.size));
            w32(t, QUEUE_DESC_LOW, q.desc as u32);
            w32(t, QUEUE_DESC_HIGH, (q.desc >> 32) as u32);
            w32(t, QUEUE_DRIVER_LOW, q.avail as u32);
            w32(t, QUEUE_DRIVER_HIGH, (q.avail >> 32) as u32);
            w32(t, QUEUE_DEVICE_LOW, q.used as u32);
            w32(t, QUEUE_DEVICE_HIGH, (q.used >> 32) as u32);
            w32(t, QUEUE_READY, 1);
        }
        w32(t, STATUS, s | status::DRIVER_OK);
    }

    #[test]
    fn identification_registers() {
        let (t, _) = transport(Echo::default());
        assert_eq!(r32(&t, MAGIC_VALUE), MAGIC);
        assert_eq!(r32(&t, VERSION_REG), 2);
        assert_eq!(r32(&t, DEVICE_ID), 42);
        assert_eq!(r32(&t, VENDOR_ID_REG), VENDOR_ID);
        assert_eq!(r32(&t, SHM_LEN_LOW), u32::MAX, "no shared memory");
    }

    #[test]
    fn features_include_version_1() {
        let (mut t, _) = transport(Echo::default());
        assert_eq!(r32(&t, DEVICE_FEATURES), 1 << 3);
        w32(&mut t, DEVICE_FEATURES_SEL, 1);
        assert_eq!(r32(&t, DEVICE_FEATURES), 1, "VERSION_1 is bit 32");
        w32(&mut t, DEVICE_FEATURES_SEL, 2);
        assert_eq!(r32(&t, DEVICE_FEATURES), 0);
    }

    #[test]
    fn queue_sizes_per_queue() {
        let (mut t, _) = transport(Echo::default());
        assert_eq!(r32(&t, QUEUE_NUM_MAX), 8);
        w32(&mut t, QUEUE_SEL, 1);
        assert_eq!(r32(&t, QUEUE_NUM_MAX), 4);
        w32(&mut t, QUEUE_SEL, 2);
        assert_eq!(r32(&t, QUEUE_NUM_MAX), 0, "no such queue");
    }

    #[test]
    fn config_space_reads_in_small_units() {
        let (t, _) = transport(Echo::default());
        let mut b = [0u8; 1];
        t.read(CONFIG + 1, &mut b);
        assert_eq!(b, [0xbb]);
        let mut h = [0u8; 2];
        t.read(CONFIG + 2, &mut h);
        assert_eq!(h, [0xcc, 0xdd]);
        assert_eq!(r32(&t, CONFIG), 0xddcc_bbaa);
    }

    #[test]
    fn unknown_driver_feature_clears_features_ok() {
        let (mut t, _) = transport(Echo::default());
        w32(&mut t, STATUS, status::ACKNOWLEDGE | status::DRIVER);
        w32(&mut t, DRIVER_FEATURES_SEL, 1);
        w32(&mut t, DRIVER_FEATURES, 1);
        w32(&mut t, DRIVER_FEATURES_SEL, 0);
        w32(&mut t, DRIVER_FEATURES, 1 << 5);
        w32(
            &mut t,
            STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_eq!(r32(&t, STATUS) & status::FEATURES_OK, 0);
    }

    #[test]
    fn legacy_driver_without_version_1_is_refused() {
        let (mut t, _) = transport(Echo::default());
        w32(&mut t, STATUS, status::ACKNOWLEDGE | status::DRIVER);
        w32(
            &mut t,
            STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        assert_eq!(r32(&t, STATUS) & status::FEATURES_OK, 0);
    }

    #[test]
    fn request_flows_through_notify_and_interrupt() {
        let echo = Echo::default();
        let log = Arc::clone(&echo.log);
        let (mut t, mem) = transport(echo);
        let mut q0 = DriverQueue::at(&mem, 8, 0);
        let q1 = DriverQueue::at(&mem, 4, 1);
        init(&mut t, &[(0, &q0), (1, &q1)]);
        assert_eq!(r32(&t, STATUS) & status::DRIVER_OK, status::DRIVER_OK);

        let head = q0.add(&mem, &[b"hi there"], &[8]);
        assert!(!t.interrupt().level());
        w32(&mut t, QUEUE_NOTIFY, 0);
        assert_eq!(q0.used(&mem), vec![(u32::from(head), 8)]);
        assert_eq!(&q0.written(&mem, head, 0, 8), b"HI THERE");

        assert!(t.interrupt().level());
        assert_eq!(r32(&t, INTERRUPT_STATUS), irq::USED_BUFFER);
        w32(&mut t, INTERRUPT_ACK, irq::USED_BUFFER);
        assert!(!t.interrupt().level());
        assert_eq!(*log.lock().unwrap(), ["activate", "notify 0"]);
    }

    #[test]
    fn notify_before_driver_ok_or_out_of_range_is_ignored() {
        let echo = Echo::default();
        let log = Arc::clone(&echo.log);
        let (mut t, mem) = transport(echo);
        w32(&mut t, QUEUE_NOTIFY, 0);
        let q0 = DriverQueue::at(&mem, 8, 0);
        init(&mut t, &[(0, &q0)]);
        w32(&mut t, QUEUE_NOTIFY, 7);
        assert_eq!(*log.lock().unwrap(), ["activate"]);
    }

    #[test]
    fn queue_config_is_frozen_once_ready() {
        let (mut t, mem) = transport(Echo::default());
        let q0 = DriverQueue::at(&mem, 8, 0);
        init(&mut t, &[(0, &q0)]);
        w32(&mut t, QUEUE_SEL, 0);
        w32(&mut t, QUEUE_DESC_LOW, 0x1234_0000);
        w32(&mut t, QUEUE_NUM, 2);
        assert_eq!(t.queues[0].desc_table, q0.desc);
        assert_eq!(t.queues[0].size, 8);
    }

    #[test]
    fn invalid_queue_sets_needs_reset() {
        let echo = Echo::default();
        let log = Arc::clone(&echo.log);
        let (mut t, mem) = transport(echo);
        let mut q0 = DriverQueue::at(&mem, 8, 0);
        q0.desc = 0xdead_0000_0000; // outside guest memory
        init(&mut t, &[(0, &q0)]);
        assert_ne!(r32(&t, STATUS) & status::DEVICE_NEEDS_RESET, 0);
        assert_eq!(r32(&t, INTERRUPT_STATUS), irq::CONFIG_CHANGE);
        assert!(log.lock().unwrap().is_empty(), "device never activated");
    }

    #[test]
    fn failed_activation_sets_needs_reset() {
        let echo = Echo {
            fail_activation: true,
            ..Echo::default()
        };
        let (mut t, mem) = transport(echo);
        let q0 = DriverQueue::at(&mem, 8, 0);
        init(&mut t, &[(0, &q0)]);
        assert_ne!(r32(&t, STATUS) & status::DEVICE_NEEDS_RESET, 0);
    }

    #[test]
    fn reset_returns_to_initial_state() {
        let echo = Echo::default();
        let log = Arc::clone(&echo.log);
        let (mut t, mem) = transport(echo);
        let mut q0 = DriverQueue::at(&mem, 8, 0);
        init(&mut t, &[(0, &q0)]);
        q0.add(&mem, &[b"x"], &[1]);
        w32(&mut t, QUEUE_NOTIFY, 0);
        assert!(t.interrupt().level());

        w32(&mut t, STATUS, 0);
        assert_eq!(r32(&t, STATUS), 0);
        assert!(!t.interrupt().level());
        assert_eq!(r32(&t, QUEUE_READY), 0);
        assert_eq!(t.queues[0], Queue::new(8));
        assert_eq!(log.lock().unwrap().last().unwrap(), "reset");

        // The driver can start over.
        let q0 = DriverQueue::at(&mem, 8, 0);
        init(&mut t, &[(0, &q0)]);
        assert_eq!(r32(&t, STATUS) & status::DRIVER_OK, status::DRIVER_OK);
    }

    #[test]
    fn non_word_register_access_is_ignored() {
        let (mut t, _) = transport(Echo::default());
        let mut b = [0xffu8; 2];
        t.read(MAGIC_VALUE, &mut b);
        assert_eq!(b, [0, 0]);
        t.write(STATUS, &[1]);
        assert_eq!(r32(&t, STATUS), 0);
    }
}
