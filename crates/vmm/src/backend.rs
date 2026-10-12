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

    /// Raises or lowers a shared peripheral interrupt line.
    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()>;
}

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
}
