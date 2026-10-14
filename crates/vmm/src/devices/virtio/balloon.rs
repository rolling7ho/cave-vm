//! virtio-balloon (virtio 1.2, section 5.5), used only for free page
//! reporting.
//!
//! A guest that boots touches far more memory than it keeps using, and
//! freed guest pages still count toward the VMM's memory on the Mac. With
//! `VIRTIO_BALLOON_F_PAGE_REPORTING`, the guest reports blocks of free
//! memory on the reporting queue, and the device hands their host pages
//! back to macOS (through the [`Discard`] function it was made with) before
//! returning the buffers. The guest does not use a reported block until the buffer comes
//! back, and expects nothing of its contents afterwards.
//!
//! The device never asks the guest to inflate (`num_pages` stays zero).
//! Buffers on the inflate and deflate queues are returned untouched.

use super::{
    ActivateError, Activation, DescriptorChain, Queue, VirtioDevice, device_type, interrupt,
};
use crate::memory::GuestMemory;

/// Feature bits (virtio 1.2, section 5.5.3).
pub mod features {
    /// The device has a reporting queue for free pages.
    pub const PAGE_REPORTING: u64 = 1 << 5;
}

/// Queue indexes. With only page reporting offered, the reporting queue
/// follows the inflate and deflate queues: drivers number only the queues
/// whose features were negotiated.
const INFLATE: u16 = 0;
const DEFLATE: u16 = 1;
const REPORTING: u16 = 2;

const QUEUE_SIZE: u16 = 64;

/// Size of the configuration space: `num_pages`, `actual`,
/// `free_page_hint_cmd_id` and `poison_val`, each a little-endian u32.
const CONFIG_SIZE: usize = 16;

/// Hands a reported guest range, as an address and a length, back to the
/// host.
pub type Discard = Box<dyn FnMut(&GuestMemory, u64, u64) + Send>;

/// The balloon device.
pub struct Balloon {
    /// `actual`, which the driver writes with the pages it holds.
    actual: u32,
    discard: Discard,
    active: Option<Active>,
}

struct Active {
    memory: GuestMemory,
    queues: Vec<Queue>,
    interrupt: super::Interrupt,
    /// A queue error leaves the device doing nothing until reset, as for
    /// the other devices.
    broken: bool,
}

impl Balloon {
    /// A balloon that hands each reported range to `discard`, which gives
    /// its host pages back ([`GuestMemory::discard`], or
    /// `GuestMemory::discard_mapped` for memory the hypervisor maps).
    pub fn new(discard: Discard) -> Balloon {
        Balloon {
            actual: 0,
            discard,
            active: None,
        }
    }

    fn config(&self) -> [u8; CONFIG_SIZE] {
        let mut config = [0u8; CONFIG_SIZE];
        config[4..8].copy_from_slice(&self.actual.to_le_bytes());
        config
    }

    /// Returns every buffer on queue `index`, handing reported pages back
    /// first if it is the reporting queue.
    fn process(&mut self, index: u16) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.broken {
            return;
        }
        let Some(queue) = active.queues.get_mut(usize::from(index)) else {
            return;
        };
        if !queue.ready {
            return;
        }
        let mut used_any = false;
        loop {
            let chain: DescriptorChain = match queue.pop(&active.memory) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(_) => {
                    active.broken = true;
                    break;
                }
            };
            if index == REPORTING {
                for d in chain.writable() {
                    (self.discard)(&active.memory, d.addr, u64::from(d.len));
                }
            }
            if queue.add_used(&active.memory, chain.head, 0).is_err() {
                active.broken = true;
                break;
            }
            used_any = true;
        }
        if used_any && queue.needs_interrupt(&active.memory).unwrap_or(true) {
            active.interrupt.raise(interrupt::USED_BUFFER);
        }
    }
}

impl VirtioDevice for Balloon {
    fn device_type(&self) -> u32 {
        device_type::BALLOON
    }

    fn features(&self) -> u64 {
        features::PAGE_REPORTING
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE; 3]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::read_config_bytes(&self.config(), offset, data);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // Only `actual` is writable.
        if offset == 4 && data.len() == 4 {
            self.actual = u32::from_le_bytes(data.try_into().expect("4 bytes"));
        }
    }

    fn activate(&mut self, activation: Activation) -> Result<(), ActivateError> {
        if activation.queues.len() != 3 {
            return Err(ActivateError("the balloon needs three queues".into()));
        }
        self.active = Some(Active {
            memory: activation.memory,
            queues: activation.queues,
            interrupt: activation.interrupt,
            broken: false,
        });
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        if matches!(index, INFLATE | DEFLATE | REPORTING) {
            self.process(index);
        }
    }

    fn reset(&mut self) {
        self.active = None;
        self.actual = 0;
    }

    fn save_queues(&self) -> Vec<Queue> {
        self.active
            .iter()
            .flat_map(|a| a.queues.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::Interrupt;
    use crate::devices::virtio::testing::{DriverQueue, guest_memory};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(u64, u64)>>>;

    fn recording() -> (Balloon, Log) {
        let log: Log = Arc::default();
        let seen = Arc::clone(&log);
        let balloon = Balloon::new(Box::new(move |_, gpa, len| {
            seen.lock().unwrap().push((gpa, len));
        }));
        (balloon, log)
    }

    fn activate(balloon: &mut Balloon, memory: &GuestMemory) -> (Vec<DriverQueue>, Interrupt) {
        let driver: Vec<_> = (0..3)
            .map(|i| DriverQueue::at(memory, QUEUE_SIZE, i))
            .collect();
        let interrupt = Interrupt::new();
        balloon
            .activate(Activation {
                memory: memory.clone(),
                queues: driver.iter().map(DriverQueue::device_queue).collect(),
                features: features::PAGE_REPORTING,
                interrupt: interrupt.clone(),
            })
            .unwrap();
        (driver, interrupt)
    }

    #[test]
    fn offers_only_page_reporting_and_never_asks_to_inflate() {
        let (mut balloon, _) = recording();
        assert_eq!(balloon.device_type(), 5);
        assert_eq!(balloon.features(), 1 << 5);
        assert_eq!(balloon.queue_max_sizes().len(), 3);
        let mut config = [0xffu8; 8];
        balloon.read_config(0, &mut config);
        assert_eq!(config, [0; 8], "num_pages and actual");
        balloon.write_config(4, &7u32.to_le_bytes());
        balloon.write_config(0, &9u32.to_le_bytes());
        balloon.read_config(0, &mut config);
        assert_eq!(config, [0, 0, 0, 0, 7, 0, 0, 0], "only actual is writable");
    }

    #[test]
    fn reported_pages_are_handed_back_before_the_buffer_returns() {
        let memory = guest_memory();
        let (mut balloon, log) = recording();
        let (mut driver, interrupt) = activate(&mut balloon, &memory);
        let head = driver[2].add(&memory, &[], &[0x4000, 0x8000]);
        balloon.queue_notify(REPORTING);
        let ranges = log.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert_eq!(
            ranges.iter().map(|r| r.1).collect::<Vec<_>>(),
            [0x4000, 0x8000]
        );
        assert_eq!(driver[2].used(&memory), [(u32::from(head), 0)]);
        assert!(interrupt.level());
    }

    #[test]
    fn inflate_and_deflate_buffers_come_back_untouched() {
        let memory = guest_memory();
        let (mut balloon, log) = recording();
        let (mut driver, _) = activate(&mut balloon, &memory);
        let pfns = 0x123u32.to_le_bytes();
        let inflated = driver[0].add(&memory, &[&pfns], &[]);
        let deflated = driver[1].add(&memory, &[&pfns], &[]);
        balloon.queue_notify(INFLATE);
        balloon.queue_notify(DEFLATE);
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(driver[0].used(&memory), [(u32::from(inflated), 0)]);
        assert_eq!(driver[1].used(&memory), [(u32::from(deflated), 0)]);
    }

    #[test]
    fn a_reset_drops_the_queues_and_actual() {
        let memory = guest_memory();
        let (mut balloon, _) = recording();
        activate(&mut balloon, &memory);
        balloon.write_config(4, &3u32.to_le_bytes());
        assert_eq!(balloon.save_queues().len(), 3);
        balloon.reset();
        assert!(balloon.save_queues().is_empty());
        let mut actual = [0xffu8; 4];
        balloon.read_config(4, &mut actual);
        assert_eq!(actual, [0; 4]);
    }
}
