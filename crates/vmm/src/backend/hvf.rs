//! The Hypervisor.framework backend: an adapter over `cave-vm-hvf`'s safe
//! API.

use cave_vm_hvf::{Vcpu, Vm};

use super::{Backend, BackendVcpu, Exit, Reg, Result, SysReg};
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

    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()> {
        Ok(self.vm.set_spi(intid, level)?)
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
}
