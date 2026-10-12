//! The VM: configuration, guest memory mappings and the GIC.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::ffi;
use crate::vcpu::{Vcpu, VcpuHandle};

/// Set while this process has a VM. Hypervisor.framework allows one VM per
/// process, so a second `Vm::new` is refused instead of failing deep inside
/// the framework.
static VM_EXISTS: AtomicBool = AtomicBool::new(false);

/// Releases a framework configuration object on drop.
struct OsObject(*mut c_void);

impl OsObject {
    fn new(raw: *mut c_void) -> Result<OsObject> {
        if raw.is_null() {
            Err(Error::NoResources)
        } else {
            Ok(OsObject(raw))
        }
    }
}

impl Drop for OsObject {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null object returned retained by a
        // `*_create` call and released exactly once, here.
        unsafe { ffi::os_release(self.0) };
    }
}

/// Guest physical address space size, in bits, that this host supports.
pub fn max_ipa_bits() -> Result<u32> {
    let mut bits = 0;
    // SAFETY: `bits` is a valid, writable u32 for the duration of the call.
    Error::check(unsafe { ffi::hv_vm_config_get_max_ipa_size(&mut bits) })?;
    Ok(bits)
}

/// Guest physical address space size, in bits, used when none is set.
pub fn default_ipa_bits() -> Result<u32> {
    let mut bits = 0;
    // SAFETY: `bits` is a valid, writable u32 for the duration of the call.
    Error::check(unsafe { ffi::hv_vm_config_get_default_ipa_size(&mut bits) })?;
    Ok(bits)
}

/// Maximum number of vCPUs per VM on this host.
pub fn max_vcpus() -> Result<u32> {
    let mut count = 0;
    // SAFETY: `count` is a valid, writable u32 for the duration of the call.
    Error::check(unsafe { ffi::hv_vm_get_max_vcpu_count(&mut count) })?;
    Ok(count)
}

/// Options for [`Vm::new`].
#[derive(Debug, Clone, Default)]
pub struct VmConfig {
    /// Guest physical address space size in bits. `None` uses the host
    /// default. Must not exceed [`max_ipa_bits`].
    pub ipa_bits: Option<u32>,
}

/// Access the guest has to a mapped region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemFlags(u64);

impl MemFlags {
    pub const READ: MemFlags = MemFlags(1 << 0);
    pub const WRITE: MemFlags = MemFlags(1 << 1);
    pub const EXEC: MemFlags = MemFlags(1 << 2);
    /// Read, write and execute: ordinary guest RAM.
    pub const RWX: MemFlags = MemFlags(0b111);

    /// Both sets of permissions.
    pub const fn union(self, other: MemFlags) -> MemFlags {
        MemFlags(self.0 | other.0)
    }
}

/// Where the GIC's MMIO regions sit in guest physical memory.
#[derive(Debug, Clone, Copy)]
pub struct GicConfig {
    /// Base of the distributor. Aligned to
    /// [`GicParams::distributor_alignment`].
    pub distributor_base: u64,
    /// Base of the redistributor region, one redistributor per vCPU.
    /// Aligned to [`GicParams::redistributor_alignment`].
    pub redistributor_base: u64,
}

/// Sizes and alignments the host's GIC requires, for laying out the guest
/// physical address map and the device tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicParams {
    pub distributor_size: usize,
    pub distributor_alignment: usize,
    /// Size of the whole redistributor region, covering every vCPU.
    pub redistributor_region_size: usize,
    /// Size of one vCPU's redistributor.
    pub redistributor_size: usize,
    pub redistributor_alignment: usize,
    /// First shared peripheral interrupt ID devices may use.
    pub spi_base: u32,
    /// Number of shared peripheral interrupt IDs.
    pub spi_count: u32,
}

impl GicParams {
    /// Reads the parameters from the host.
    pub fn query() -> Result<GicParams> {
        let mut p = GicParams {
            distributor_size: 0,
            distributor_alignment: 0,
            redistributor_region_size: 0,
            redistributor_size: 0,
            redistributor_alignment: 0,
            spi_base: 0,
            spi_count: 0,
        };
        // SAFETY: each pointer is to a distinct, writable field of `p`, valid
        // for the duration of its call.
        unsafe {
            Error::check(ffi::hv_gic_get_distributor_size(&mut p.distributor_size))?;
            Error::check(ffi::hv_gic_get_distributor_base_alignment(
                &mut p.distributor_alignment,
            ))?;
            Error::check(ffi::hv_gic_get_redistributor_region_size(
                &mut p.redistributor_region_size,
            ))?;
            Error::check(ffi::hv_gic_get_redistributor_size(
                &mut p.redistributor_size,
            ))?;
            Error::check(ffi::hv_gic_get_redistributor_base_alignment(
                &mut p.redistributor_alignment,
            ))?;
            Error::check(ffi::hv_gic_get_spi_interrupt_range(
                &mut p.spi_base,
                &mut p.spi_count,
            ))?;
        }
        Ok(p)
    }
}

/// A private peripheral interrupt the GIC wires up itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicInterrupt {
    PerformanceMonitor,
    Maintenance,
    El2PhysicalTimer,
    El1VirtualTimer,
    El1PhysicalTimer,
}

impl GicInterrupt {
    /// The interrupt ID the host GIC uses, for the device tree.
    pub fn intid(self) -> Result<u32> {
        let raw: u16 = match self {
            GicInterrupt::PerformanceMonitor => 23,
            GicInterrupt::Maintenance => 25,
            GicInterrupt::El2PhysicalTimer => 26,
            GicInterrupt::El1VirtualTimer => 27,
            GicInterrupt::El1PhysicalTimer => 30,
        };
        let mut intid = 0;
        // SAFETY: `intid` is a valid, writable u32 for the duration of the call.
        Error::check(unsafe { ffi::hv_gic_get_intid(raw, &mut intid) })?;
        Ok(intid)
    }
}

/// This process's VM. Destroyed on drop, after every [`Vcpu`] borrowed from
/// it is gone.
#[derive(Debug)]
pub struct Vm {
    _private: (),
}

impl Vm {
    /// Creates the VM. Fails with [`Error::VmExists`] if this process
    /// already has one, and with [`Error::Denied`] if the binary lacks the
    /// hypervisor entitlement.
    pub fn new(config: &VmConfig) -> Result<Vm> {
        if VM_EXISTS.swap(true, Ordering::AcqRel) {
            return Err(Error::VmExists);
        }
        let result = Self::create(config);
        if result.is_err() {
            VM_EXISTS.store(false, Ordering::Release);
        }
        result
    }

    fn create(config: &VmConfig) -> Result<Vm> {
        // SAFETY: no arguments; returns a retained object or null.
        let cfg = OsObject::new(unsafe { ffi::hv_vm_config_create() })?;
        if let Some(bits) = config.ipa_bits {
            // SAFETY: `cfg.0` is a live VM config object.
            Error::check(unsafe { ffi::hv_vm_config_set_ipa_size(cfg.0, bits) })?;
        }
        // SAFETY: `cfg.0` is a live VM config object, and `VM_EXISTS` ensures
        // no other VM exists in this process.
        Error::check(unsafe { ffi::hv_vm_create(cfg.0) })?;
        Ok(Vm { _private: () })
    }

    /// Maps host memory into the guest physical address space.
    ///
    /// # Safety
    ///
    /// - `host` must point to `size` bytes of memory owned by the caller,
    ///   aligned to the host page size (16 KiB on Apple Silicon), and `size`
    ///   and `ipa` must be multiples of it.
    /// - That memory must stay allocated and mapped in this process until
    ///   the range is unmapped with [`Vm::unmap`] or the VM is dropped.
    /// - The guest can read and write it whenever a vCPU runs, so the host
    ///   must not hold Rust references into it across a vCPU run; access it
    ///   through raw pointers or volatile reads and writes.
    pub unsafe fn map(&self, host: *mut u8, ipa: u64, size: usize, flags: MemFlags) -> Result<()> {
        // SAFETY: the caller upholds this function's contract.
        Error::check(unsafe { ffi::hv_vm_map(host.cast(), ipa, size, flags.0) })
    }

    /// Removes a mapping made with [`Vm::map`].
    pub fn unmap(&self, ipa: u64, size: usize) -> Result<()> {
        // SAFETY: unmapping only removes guest access; the framework checks
        // the range.
        Error::check(unsafe { ffi::hv_vm_unmap(ipa, size) })
    }

    /// Changes the guest's access to a mapped range.
    pub fn protect(&self, ipa: u64, size: usize, flags: MemFlags) -> Result<()> {
        // SAFETY: changes guest permissions only; the framework checks the
        // range.
        Error::check(unsafe { ffi::hv_vm_protect(ipa, size, flags.0) })
    }

    /// Creates the GICv3 interrupt controller. Must be called before any
    /// vCPU is created.
    pub fn create_gic(&self, config: &GicConfig) -> Result<()> {
        // SAFETY: no arguments; returns a retained object or null.
        let cfg = OsObject::new(unsafe { ffi::hv_gic_config_create() })?;
        // SAFETY: `cfg.0` is a live GIC config object for all three calls.
        unsafe {
            Error::check(ffi::hv_gic_config_set_distributor_base(
                cfg.0,
                config.distributor_base,
            ))?;
            Error::check(ffi::hv_gic_config_set_redistributor_base(
                cfg.0,
                config.redistributor_base,
            ))?;
            Error::check(ffi::hv_gic_create(cfg.0))
        }
    }

    /// Raises (`true`) or lowers a shared peripheral interrupt line.
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<()> {
        // SAFETY: plain values; the framework checks the interrupt ID.
        Error::check(unsafe { ffi::hv_gic_set_spi(intid, level) })
    }

    /// Saves the GIC distributor and redistributor state. The per-vCPU CPU
    /// interface registers are saved separately with [`Vcpu::icc_reg`].
    pub fn gic_state(&self) -> Result<Vec<u8>> {
        // SAFETY: no arguments; returns a retained object or null.
        let state = OsObject::new(unsafe { ffi::hv_gic_state_create() })?;
        let mut size = 0;
        // SAFETY: `state.0` is a live GIC state object and `size` is writable.
        Error::check(unsafe { ffi::hv_gic_state_get_size(state.0, &mut size) })?;
        let mut data = vec![0u8; size];
        // SAFETY: `data` has exactly the `size` bytes the framework asked for.
        Error::check(unsafe { ffi::hv_gic_state_get_data(state.0, data.as_mut_ptr().cast()) })?;
        Ok(data)
    }

    /// Restores state saved by [`Vm::gic_state`]. The GIC and every vCPU
    /// must exist, and no vCPU may have run yet. Fails if a macOS update
    /// changed the state format.
    pub fn set_gic_state(&self, data: &[u8]) -> Result<()> {
        // SAFETY: `data` is valid for reads of `data.len()` bytes.
        Error::check(unsafe { ffi::hv_gic_set_state(data.as_ptr().cast(), data.len()) })
    }

    /// Creates a vCPU owned by the calling thread.
    pub fn create_vcpu(&self) -> Result<Vcpu<'_>> {
        Vcpu::new(self)
    }

    /// Makes the given vCPUs return from [`Vcpu::run`] with
    /// [`crate::Exit::Canceled`]. Callable from any thread.
    pub fn exit_vcpus(&self, vcpus: &[VcpuHandle]) -> Result<()> {
        let ids: Vec<u64> = vcpus.iter().map(|v| v.0).collect();
        let count = u32::try_from(ids.len()).map_err(|_| Error::BadArgument)?;
        // SAFETY: `ids` holds `count` vCPU IDs and outlives the call.
        Error::check(unsafe { ffi::hv_vcpus_exit(ids.as_ptr(), count) })
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: every `Vcpu` borrows this `Vm`, so all have been destroyed.
        // Errors cannot be reported from drop; the flag is cleared either way
        // so a later VM gets a clear framework error rather than VmExists.
        unsafe { ffi::hv_vm_destroy() };
        VM_EXISTS.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn rwx_is_union_of_flags() {
        assert_eq!(
            MemFlags::READ.union(MemFlags::WRITE).union(MemFlags::EXEC),
            MemFlags::RWX
        );
    }

    #[test]
    fn null_object_is_an_error() {
        assert_eq!(
            OsObject::new(ptr::null_mut()).err(),
            Some(Error::NoResources)
        );
    }
}
