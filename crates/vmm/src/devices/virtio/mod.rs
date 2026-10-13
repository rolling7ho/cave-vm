//! Virtio devices over the virtio-mmio transport (virtio 1.2).
//!
//! There is no PCI, ACPI or UEFI (design, "Architecture"): each device sits
//! in a virtio-mmio slot described in the device tree. [`mmio::MmioTransport`]
//! implements the registers and the device status state machine, and hands
//! the driver's virtqueues to a [`VirtioDevice`] once the driver sets
//! `DRIVER_OK`.
//!
//! Interrupts are level-triggered. A device sets bits in its [`Interrupt`]
//! and the run loop drives the line to match, the same way it does for the
//! UART. A device that does work on its own thread kicks the vCPU so the
//! run loop notices.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::memory::GuestMemory;

pub mod mmio;
pub mod net;
pub mod queue;
pub mod vsock;

#[cfg(test)]
pub mod testing;

pub use queue::{DescriptorChain, Queue};

/// Device type IDs (virtio 1.2, section 5).
pub mod device_type {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const VSOCK: u32 = 19;
}

/// Feature bits common to every device type.
pub mod features {
    /// The device follows virtio 1.0 or later. Required: legacy devices are
    /// not supported.
    pub const VERSION_1: u64 = 1 << 32;
}

/// Device status bits (virtio 1.2, section 2.1).
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

/// Bits of the transport's interrupt status register.
pub mod interrupt {
    /// A queue has new used buffers.
    pub const USED_BUFFER: u32 = 1;
    /// The device configuration changed.
    pub const CONFIG_CHANGE: u32 = 2;
}

/// A function that interrupts a vCPU's run; see
/// [`BackendVcpu::kicker`](crate::backend::BackendVcpu::kicker).
pub type Kick = Arc<dyn Fn() + Send + Sync>;

/// A device's interrupt status, shared between the transport, the device
/// (possibly on another thread) and the run loop. Cheap to clone.
#[derive(Clone, Default)]
pub struct Interrupt {
    inner: Arc<InterruptInner>,
}

#[derive(Default)]
struct InterruptInner {
    status: AtomicU32,
    kick: Mutex<Option<Kick>>,
}

impl fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interrupt")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Interrupt {
    pub fn new() -> Interrupt {
        Interrupt::default()
    }

    /// Sets the function called after a new interrupt is raised, so a vCPU
    /// sleeping in the hypervisor wakes and the run loop raises the line.
    pub fn set_kick(&self, kick: Kick) {
        *self.inner.kick.lock().expect("kick lock") = Some(kick);
    }

    /// Raises `bits` (from [`interrupt`]) and kicks the vCPU.
    pub fn raise(&self, bits: u32) {
        let before = self.inner.status.fetch_or(bits, Ordering::AcqRel);
        if before & bits != bits {
            let kick = self.inner.kick.lock().expect("kick lock").clone();
            if let Some(kick) = kick {
                kick();
            }
        }
    }

    /// Clears `bits`, as the driver's `InterruptACK` write does.
    pub fn ack(&self, bits: u32) {
        self.inner.status.fetch_and(!bits, Ordering::AcqRel);
    }

    /// Clears everything, on device reset.
    pub fn clear(&self) {
        self.inner.status.store(0, Ordering::Release);
    }

    /// The interrupt status register.
    pub fn status(&self) -> u32 {
        self.inner.status.load(Ordering::Acquire)
    }

    /// Whether the interrupt line should be high.
    pub fn level(&self) -> bool {
        self.status() != 0
    }
}

/// What a device receives when the driver sets `DRIVER_OK`.
#[derive(Debug)]
pub struct Activation {
    pub memory: GuestMemory,
    /// The driver's queues, in queue-index order. Queues the driver left
    /// disabled have `ready` unset.
    pub queues: Vec<Queue>,
    /// Feature bits the driver accepted.
    pub features: u64,
    pub interrupt: Interrupt,
}

/// Why a device could not start. The transport reports it to the driver as
/// `DEVICE_NEEDS_RESET`.
#[derive(Debug)]
pub struct ActivateError(pub String);

impl fmt::Display for ActivateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ActivateError {}

/// A device type behind a virtio-mmio transport.
pub trait VirtioDevice: Send {
    /// Device type ID, from [`device_type`].
    fn device_type(&self) -> u32;

    /// Feature bits the device offers. The transport adds
    /// [`features::VERSION_1`].
    fn features(&self) -> u64;

    /// Largest size of each queue, which also fixes the number of queues.
    /// Each must be a power of two.
    fn queue_max_sizes(&self) -> &[u16];

    /// Reads device configuration space at `offset`. Bytes past the end
    /// read as zero.
    fn read_config(&self, offset: u64, data: &mut [u8]);

    /// Writes device configuration space. Most devices ignore this.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    /// Starts the device with the driver's queues.
    fn activate(&mut self, activation: Activation) -> Result<(), ActivateError>;

    /// The driver notified queue `index` of new buffers.
    fn queue_notify(&mut self, index: u16);

    /// Stops the device and drops its queues, on a driver reset.
    fn reset(&mut self);
}

/// Copies `config[offset..]` into `data`, zero-filling past the end. For
/// [`VirtioDevice::read_config`] implementations.
pub fn read_config_bytes(config: &[u8], offset: u64, data: &mut [u8]) {
    data.fill(0);
    let Ok(start) = usize::try_from(offset) else {
        return;
    };
    if let Some(src) = config.get(start..) {
        let n = src.len().min(data.len());
        data[..n].copy_from_slice(&src[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn raise_kicks_once_until_acked() {
        let irq = Interrupt::new();
        let kicks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&kicks);
        irq.set_kick(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        assert!(!irq.level());
        irq.raise(interrupt::USED_BUFFER);
        irq.raise(interrupt::USED_BUFFER);
        assert!(irq.level());
        assert_eq!(kicks.load(Ordering::SeqCst), 1, "already raised");

        irq.raise(interrupt::CONFIG_CHANGE);
        assert_eq!(irq.status(), 3);
        irq.ack(interrupt::USED_BUFFER);
        assert_eq!(irq.status(), interrupt::CONFIG_CHANGE);
        irq.ack(interrupt::CONFIG_CHANGE);
        assert!(!irq.level());
        irq.raise(interrupt::USED_BUFFER);
        assert_eq!(kicks.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn config_reads_zero_fill_past_the_end() {
        let config = [1, 2, 3, 4];
        let mut buf = [0xff; 4];
        read_config_bytes(&config, 2, &mut buf);
        assert_eq!(buf, [3, 4, 0, 0]);
        read_config_bytes(&config, 100, &mut buf);
        assert_eq!(buf, [0; 4]);
    }
}
