//! The hypervisor backend trait.
//!
//! The VMM drives guests through [`Backend`] and [`BackendVcpu`] so device
//! models and the run loop can be tested against [`mock::MockBackend`]
//! without booting a VM, and so another hypervisor (KVM) can be added later
//! (design, "Architecture"). [`hvf::HvfBackend`] is the Hypervisor.framework
//! adapter.
//!
//! Exits and registers use the Arm vocabulary from `cave-vm-hvf`
//! ([`Exit`], [`Reg`], [`SysReg`]), which is pure data and builds on any
//! host. Another backend translates into it.

use std::fmt;

pub use cave_vm_hvf::{DataAccess, Exception, Exit, Reg, SysReg, Trap};

use crate::memory::GuestMemory;

pub mod mock;

#[cfg(target_arch = "aarch64")]
pub mod hvf;

/// An error from a backend.
#[derive(Debug)]
pub enum Error {
    /// The hypervisor refused an operation.
    Hypervisor(cave_vm_hvf::Error),
    /// Guest memory could not be set up.
    Memory(crate::memory::Error),
    /// The mock backend's script ran out of exits.
    ScriptExhausted { vcpu: u32 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hypervisor(e) => e.fmt(f),
            Error::Memory(e) => e.fmt(f),
            Error::ScriptExhausted { vcpu } => {
                write!(f, "mock vCPU {vcpu} has no scripted exits left")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<cave_vm_hvf::Error> for Error {
    fn from(e: cave_vm_hvf::Error) -> Error {
        Error::Hypervisor(e)
    }
}

impl From<crate::memory::Error> for Error {
    fn from(e: crate::memory::Error) -> Error {
        Error::Memory(e)
    }
}

/// Result of a backend call.
pub type Result<T> = std::result::Result<T, Error>;

/// A VM as the VMM sees it.
pub trait Backend {
    /// A vCPU of this backend. It may be tied to the thread that created it.
    type Vcpu<'a>: BackendVcpu
    where
        Self: 'a;

    /// Makes guest memory visible to the guest at its guest addresses.
    fn map_memory(&mut self, memory: &GuestMemory) -> Result<()>;

    /// Creates vCPU number `index`, on the calling thread.
    fn create_vcpu(&self, index: u32) -> Result<Self::Vcpu<'_>>;

    /// Creates the GICv3 at [`crate::layout::GIC_DIST_BASE`] and
    /// [`crate::layout::GIC_REDIST_BASE`]. Must come before any vCPU.
    fn create_irqchip(&mut self) -> Result<IrqChip>;

    /// Raises or lowers a shared peripheral interrupt line.
    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()>;

    /// The host clock the guest's virtual counter derives from, in ticks.
    fn now_ticks(&self) -> u64;

    /// Converts host clock ticks to a duration.
    fn ticks_to_duration(&self, ticks: u64) -> std::time::Duration;
}

/// Sizes of the interrupt controller's MMIO regions, for the device tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqChip {
    pub dist_size: u64,
    /// Size of one vCPU's redistributor.
    pub redist_size: u64,
}

/// Interrupts a vCPU's run from another thread; see [`BackendVcpu::kicker`].
pub type Kick = Box<dyn Fn() + Send + Sync>;

/// A vCPU as the VMM sees it.
pub trait BackendVcpu {
    /// Runs the guest until it exits to the host.
    fn run(&mut self) -> Result<Exit>;

    /// Reads a general register.
    fn reg(&self, reg: Reg) -> Result<u64>;

    /// Writes a general register.
    fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()>;

    /// Reads a system register.
    fn sys_reg(&self, reg: SysReg) -> Result<u64>;

    /// Writes a system register.
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()>;

    /// Whether the virtual timer is masked from exiting.
    fn vtimer_masked(&self) -> Result<bool>;

    /// Masks or unmasks virtual timer exits.
    fn set_vtimer_masked(&mut self, masked: bool) -> Result<()>;

    /// Host ticks minus guest virtual counter.
    fn vtimer_offset(&self) -> Result<u64>;

    /// Marks a private peripheral interrupt (interrupt ID below 32) of this
    /// vCPU pending or not.
    fn set_ppi_pending(&mut self, intid: u32, pending: bool) -> Result<()>;

    /// A function other threads call to make [`run`](Self::run) return
    /// [`Exit::Canceled`], so the run loop can serve new device work. The
    /// hypervisor handles a guest's WFI itself, so without a kick the vCPU
    /// sleeps until an interrupt it already knows about.
    fn kicker(&self) -> Kick;
}
