//! vCPUs: running guest code and reading and writing its registers.

use core::arch::aarch64::uint8x16_t;
use std::marker::PhantomData;
use std::ptr;

use crate::error::{Error, Result};
use crate::exit::Exit;
use crate::ffi;
use crate::regs::{IccReg, Reg, SIMD_FP_REGS, SysReg};
use crate::vm::Vm;

/// Which interrupt line [`Vcpu::set_pending_interrupt`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    Irq,
    Fiq,
}

impl InterruptKind {
    fn raw(self) -> u32 {
        match self {
            InterruptKind::Irq => 0,
            InterruptKind::Fiq => 1,
        }
    }
}

/// A vCPU. Hypervisor.framework requires every call on a vCPU to come from
/// the thread that created it, so this type is neither `Send` nor `Sync`.
/// Use [`Vcpu::handle`] to stop it from another thread.
#[derive(Debug)]
pub struct Vcpu<'vm> {
    id: ffi::hv_vcpu_t,
    exit: *const ffi::hv_vcpu_exit_t,
    _vm: PhantomData<&'vm Vm>,
}

/// Identifies a vCPU to other threads, for [`Vm::exit_vcpus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuHandle(pub(crate) ffi::hv_vcpu_t);

impl<'vm> Vcpu<'vm> {
    pub(crate) fn new(_vm: &'vm Vm) -> Result<Vcpu<'vm>> {
        let mut id = 0;
        let mut exit = ptr::null();
        // SAFETY: both out-pointers are valid and writable; a null config
        // asks for the default vCPU configuration.
        Error::check(unsafe { ffi::hv_vcpu_create(&mut id, &mut exit, ptr::null_mut()) })?;
        if exit.is_null() {
            return Err(Error::Failed);
        }
        Ok(Vcpu {
            id,
            exit,
            _vm: PhantomData,
        })
    }

    /// The vCPU's ID, valid for [`Vm::exit_vcpus`] from any thread.
    pub fn handle(&self) -> VcpuHandle {
        VcpuHandle(self.id)
    }

    /// Runs the guest until it exits to the host.
    pub fn run(&mut self) -> Result<Exit> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_run(self.id) })?;
        // SAFETY: the framework keeps the exit record alive for the vCPU's
        // lifetime and fills it before `hv_vcpu_run` returns.
        let exit = unsafe { &*self.exit };
        Ok(Exit::from_raw(
            exit.reason,
            exit.exception.syndrome,
            exit.exception.virtual_address,
            exit.exception.physical_address,
        ))
    }

    /// Reads a general register.
    pub fn reg(&self, reg: Reg) -> Result<u64> {
        let mut value = 0;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `value` is writable.
        Error::check(unsafe { ffi::hv_vcpu_get_reg(self.id, reg.raw(), &mut value) })?;
        Ok(value)
    }

    /// Writes a general register.
    pub fn set_reg(&self, reg: Reg, value: u64) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_set_reg(self.id, reg.raw(), value) })
    }

    /// Reads SIMD and floating-point register `q<n>`, `n` below 32.
    pub fn simd_fp_reg(&self, n: u8) -> Result<u128> {
        if n >= SIMD_FP_REGS {
            return Err(Error::BadArgument);
        }
        let mut out = ffi::hv_simd_fp_uchar16_t([0; 16]);
        // SAFETY: `self.id` is a live vCPU owned by this thread; `out` is a
        // writable, suitably aligned 16-byte buffer.
        Error::check(unsafe { ffi::hv_vcpu_get_simd_fp_reg(self.id, n as u32, &mut out) })?;
        let bytes = out.0;
        Ok(u128::from_le_bytes(bytes))
    }

    /// Writes SIMD and floating-point register `q<n>`, `n` below 32.
    pub fn set_simd_fp_reg(&self, n: u8, value: u128) -> Result<()> {
        if n >= SIMD_FP_REGS {
            return Err(Error::BadArgument);
        }
        // SAFETY: `[u8; 16]` and `uint8x16_t` have the same size and every
        // bit pattern is valid for both.
        let vector: uint8x16_t = unsafe { std::mem::transmute(value.to_le_bytes()) };
        let ret: u64;
        // SAFETY: calls `hv_vcpu_set_simd_fp_reg(vcpu, reg, vector)` following
        // the AAPCS64 C calling convention: the vCPU ID in x0, the register
        // number in w1 and the 128-bit vector in v0, with the result in w0.
        // `clobber_abi("C")` marks every caller-saved register as clobbered.
        // `self.id` is a live vCPU owned by this thread.
        unsafe {
            std::arch::asm!(
                "blr {f}",
                f = in(reg) ffi::hv_vcpu_set_simd_fp_reg as *const (),
                inlateout("x0") self.id => ret,
                in("x1") u64::from(n),
                in("v0") vector,
                clobber_abi("C"),
            );
        }
        Error::check(ret as u32 as i32)
    }

    /// Reads a system register.
    pub fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        let mut value = 0;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `value` is writable.
        Error::check(unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg.raw(), &mut value) })?;
        Ok(value)
    }

    /// Writes a system register.
    pub fn set_sys_reg(&self, reg: SysReg, value: u64) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg.raw(), value) })
    }

    /// Reads a GIC CPU interface register. Needs a GIC.
    pub fn icc_reg(&self, reg: IccReg) -> Result<u64> {
        let mut value = 0;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `value` is writable.
        Error::check(unsafe { ffi::hv_gic_get_icc_reg(self.id, reg.raw(), &mut value) })?;
        Ok(value)
    }

    /// Writes a GIC CPU interface register. Needs a GIC.
    pub fn set_icc_reg(&self, reg: IccReg, value: u64) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_gic_set_icc_reg(self.id, reg.raw(), value) })
    }

    /// Guest physical address of this vCPU's GIC redistributor. Needs a GIC,
    /// and the vCPU's affinity must already be set in `MPIDR_EL1`.
    pub fn redistributor_base(&self) -> Result<u64> {
        let mut base = 0;
        // SAFETY: `self.id` is a live vCPU; `base` is writable.
        Error::check(unsafe { ffi::hv_gic_get_redistributor_base(self.id, &mut base) })?;
        Ok(base)
    }

    /// Whether an IRQ or FIQ is pending for the next run.
    pub fn pending_interrupt(&self, kind: InterruptKind) -> Result<bool> {
        let mut pending = false;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `pending` is writable.
        Error::check(unsafe {
            ffi::hv_vcpu_get_pending_interrupt(self.id, kind.raw(), &mut pending)
        })?;
        Ok(pending)
    }

    /// Asserts or clears an IRQ or FIQ for the next run. Without a GIC, this
    /// is how the host delivers interrupts.
    pub fn set_pending_interrupt(&self, kind: InterruptKind, pending: bool) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_set_pending_interrupt(self.id, kind.raw(), pending) })
    }

    /// Whether the virtual timer is masked. The framework masks it when it
    /// reports [`Exit::VtimerActivated`].
    pub fn vtimer_masked(&self) -> Result<bool> {
        let mut masked = false;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `masked` is writable.
        Error::check(unsafe { ffi::hv_vcpu_get_vtimer_mask(self.id, &mut masked) })?;
        Ok(masked)
    }

    /// Masks or unmasks the virtual timer.
    pub fn set_vtimer_masked(&self, masked: bool) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_set_vtimer_mask(self.id, masked) })
    }

    /// The virtual timer offset: host counter minus guest counter.
    pub fn vtimer_offset(&self) -> Result<u64> {
        let mut offset = 0;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `offset` is writable.
        Error::check(unsafe { ffi::hv_vcpu_get_vtimer_offset(self.id, &mut offset) })?;
        Ok(offset)
    }

    /// Sets the virtual timer offset. A restore sets it so the guest
    /// counter continues from where the snapshot left it.
    pub fn set_vtimer_offset(&self, offset: u64) -> Result<()> {
        // SAFETY: `self.id` is a live vCPU owned by this thread.
        Error::check(unsafe { ffi::hv_vcpu_set_vtimer_offset(self.id, offset) })
    }

    /// Time the vCPU has spent running guest code, in Mach absolute time
    /// units.
    pub fn exec_time(&self) -> Result<u64> {
        let mut time = 0;
        // SAFETY: `self.id` is a live vCPU owned by this thread; `time` is writable.
        Error::check(unsafe { ffi::hv_vcpu_get_exec_time(self.id, &mut time) })?;
        Ok(time)
    }
}

impl Drop for Vcpu<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.id` is a live vCPU, and `Vcpu` is not `Send`, so
        // this runs on the thread that created it.
        unsafe { ffi::hv_vcpu_destroy(self.id) };
    }
}
