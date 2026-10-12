//! Setting up a VM for direct kernel boot.

use std::fmt;

use crate::backend::{self, Backend, BackendVcpu, IrqChip, Reg, SysReg};
use crate::fdt::{self, DeviceTreeConfig};
use crate::kernel::{self, BootLayout, ImageHeader};
use crate::layout;
use crate::memory::{self, GuestMemory, Region};

/// EL1h with debug, SError, IRQ and FIQ masked, as Linux expects at entry.
const PSTATE_EL1H_MASKED: u64 = 0x3c5;

/// What to boot.
#[derive(Debug, Clone)]
pub struct BootConfig {
    /// An uncompressed arm64 `Image`.
    pub kernel: Vec<u8>,
    /// An initramfs (cpio, optionally compressed).
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Guest RAM in bytes, a multiple of 16 KiB.
    pub memory_size: u64,
    /// vCPUs in the device tree. The spike runs only the first.
    pub cpus: u32,
    /// Virtio-mmio slots in the device tree, from slot 0.
    pub virtio_slots: u32,
}

/// An error setting up the boot.
#[derive(Debug)]
pub enum Error {
    Kernel(kernel::Error),
    DeviceTree(vm_fdt::Error),
    Memory(memory::Error),
    Backend(backend::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Kernel(e) => e.fmt(f),
            Error::DeviceTree(e) => write!(f, "could not build the device tree: {e}"),
            Error::Memory(e) => e.fmt(f),
            Error::Backend(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<memory::Error> for Error {
    fn from(e: memory::Error) -> Error {
        Error::Memory(e)
    }
}

impl From<backend::Error> for Error {
    fn from(e: backend::Error) -> Error {
        Error::Backend(e)
    }
}

/// A VM ready for its boot vCPU.
#[derive(Debug)]
pub struct Prepared {
    pub memory: GuestMemory,
    pub layout: BootLayout,
    pub irqchip: IrqChip,
    /// The device tree blob written to guest memory.
    pub dtb: Vec<u8>,
}

/// Creates the interrupt controller, allocates RAM, loads the kernel,
/// initramfs and device tree, and maps RAM into the guest.
pub fn prepare<B: Backend>(backend: &mut B, config: &BootConfig) -> Result<Prepared, Error> {
    let irqchip = backend.create_irqchip()?;
    let header = ImageHeader::parse(&config.kernel).map_err(Error::Kernel)?;
    let layout = kernel::plan(
        &header,
        config.kernel.len() as u64,
        config.initrd.as_ref().map(|i| i.len() as u64),
        layout::RAM_BASE,
        config.memory_size,
    )
    .map_err(Error::Kernel)?;

    let memory = GuestMemory::new(vec![Region::anonymous(
        layout::RAM_BASE,
        config.memory_size,
    )?])?;
    memory.write(layout.kernel, &config.kernel)?;
    let initrd = match (&config.initrd, layout.initrd) {
        (Some(data), Some(start)) => {
            memory.write(start, data)?;
            Some((start, start + data.len() as u64))
        }
        _ => None,
    };
    let dtb = fdt::build(&DeviceTreeConfig {
        cpus: config.cpus,
        memory_size: config.memory_size,
        gic_dist_size: irqchip.dist_size,
        gic_redist_size: irqchip.redist_size,
        cmdline: config.cmdline.clone(),
        initrd,
        virtio_slots: config.virtio_slots,
    })
    .map_err(Error::DeviceTree)?;
    memory.write(layout.dtb, &dtb)?;
    backend.map_memory(&memory)?;
    Ok(Prepared {
        memory,
        layout,
        irqchip,
        dtb,
    })
}

/// Sets vCPU `index`'s affinity, and for the boot vCPU the entry state:
/// `PC` at the kernel, `x0` the device tree, `x1` to `x3` zero, EL1h with
/// interrupts masked and the MMU off.
pub fn setup_vcpu<V: BackendVcpu>(
    vcpu: &mut V,
    index: u32,
    layout: &BootLayout,
) -> backend::Result<()> {
    vcpu.set_sys_reg(SysReg::MPIDR_EL1, fdt::mpidr(index))?;
    if index == 0 {
        vcpu.set_reg(Reg::PC, layout.kernel)?;
        vcpu.set_reg(Reg::CPSR, PSTATE_EL1H_MASKED)?;
        vcpu.set_reg(Reg::x(0).expect("x0"), layout.dtb)?;
        for n in 1..=3 {
            vcpu.set_reg(Reg::x(n).expect("x1 to x3"), 0)?;
        }
    }
    Ok(())
}
