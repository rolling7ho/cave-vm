//! The Hypervisor.framework backend: an adapter over `cave-vm-hvf`'s safe
//! API.

use cave_vm_hvf::{GicConfig, GicParams, RedistReg, Vcpu, Vm};

use super::{Backend, BackendVcpu, Error, Exit, IrqChip, Kick, Reg, Result, SysReg};
use crate::layout;
use crate::memory::{GuestMemory, Mapping};

/// Drives a [`Vm`] the caller created. Borrowing it keeps every vCPU and
/// memory mapping from outliving the VM.
#[derive(Debug)]
pub struct HvfBackend<'vm> {
    vm: &'vm Vm,
    mappings: Vec<Mapping<'vm>>,
}

impl<'vm> HvfBackend<'vm> {
    pub fn new(vm: &'vm Vm) -> HvfBackend<'vm> {
        HvfBackend {
            vm,
            mappings: Vec::new(),
        }
    }
}

impl<'vm> Backend for HvfBackend<'vm> {
    type Vcpu<'a>
        = HvfVcpu<'vm>
    where
        Self: 'a;

    fn map_memory(&mut self, memory: &GuestMemory) -> Result<()> {
        self.mappings.push(memory.map_into(self.vm)?);
        Ok(())
    }

    fn create_vcpu(&self, _index: u32) -> Result<HvfVcpu<'vm>> {
        Ok(HvfVcpu(self.vm.create_vcpu()?))
    }

    fn create_irqchip(&mut self) -> Result<IrqChip> {
        let params = GicParams::query()?;
        let aligned = layout::GIC_DIST_BASE.is_multiple_of(params.distributor_alignment as u64)
            && layout::GIC_REDIST_BASE.is_multiple_of(params.redistributor_alignment as u64)
            && layout::GIC_DIST_BASE + params.distributor_size as u64 <= layout::UART_BASE
            && layout::GIC_REDIST_BASE + params.redistributor_region_size as u64
                <= layout::RAM_BASE;
        if !aligned {
            return Err(Error::Hypervisor(cave_vm_hvf::Error::Unsupported));
        }
        self.vm.create_gic(&GicConfig {
            distributor_base: layout::GIC_DIST_BASE,
            redistributor_base: layout::GIC_REDIST_BASE,
        })?;
        Ok(IrqChip {
            dist_size: params.distributor_size as u64,
            redist_size: params.redistributor_size as u64,
        })
    }

    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()> {
        Ok(self.vm.set_spi(intid, level)?)
    }

    fn now_ticks(&self) -> u64 {
        cave_vm_hvf::host_ticks()
    }

    fn ticks_to_duration(&self, ticks: u64) -> std::time::Duration {
        cave_vm_hvf::ticks_to_duration(ticks)
    }
}

/// A Hypervisor.framework vCPU. Tied to the thread that created it.
#[derive(Debug)]
pub struct HvfVcpu<'vm>(Vcpu<'vm>);

impl BackendVcpu for HvfVcpu<'_> {
    fn run(&mut self) -> Result<Exit> {
        Ok(self.0.run()?)
    }

    fn reg(&self, reg: Reg) -> Result<u64> {
        Ok(self.0.reg(reg)?)
    }

    fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
        Ok(self.0.set_reg(reg, value)?)
    }

    fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        Ok(self.0.sys_reg(reg)?)
    }

    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        Ok(self.0.set_sys_reg(reg, value)?)
    }

    fn vtimer_masked(&self) -> Result<bool> {
        Ok(self.0.vtimer_masked()?)
    }

    fn set_vtimer_masked(&mut self, masked: bool) -> Result<()> {
        Ok(self.0.set_vtimer_masked(masked)?)
    }

    fn vtimer_offset(&self) -> Result<u64> {
        Ok(self.0.vtimer_offset()?)
    }

    fn set_ppi_pending(&mut self, intid: u32, pending: bool) -> Result<()> {
        if intid >= 32 {
            return Err(Error::Hypervisor(cave_vm_hvf::Error::BadArgument));
        }
        let reg = if pending {
            RedistReg::ISPENDR0
        } else {
            RedistReg::ICPENDR0
        };
        Ok(self.0.set_redist_reg(reg, 1 << intid)?)
    }

    fn kicker(&self) -> Kick {
        let handle = self.0.handle();
        // A failed kick means the vCPU is gone, so there is nothing to wake.
        Box::new(move || {
            let _ = handle.exit();
        })
    }
}
