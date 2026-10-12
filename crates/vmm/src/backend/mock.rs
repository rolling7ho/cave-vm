//! A backend that runs no guest. Each vCPU replays a script of exits and
//! the backend records what the VMM did, so device models and the run loop
//! can be tested on any machine.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Backend, BackendVcpu, Error, Exit, IccReg, IrqChip, Kick, Reg, Result, SysReg};
use crate::memory::GuestMemory;

/// Something the VMM did to the mock backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MapMemory {
        gpa: u64,
        size: usize,
    },
    CreateVcpu {
        vcpu: u32,
    },
    SetReg {
        vcpu: u32,
        reg: Reg,
        value: u64,
    },
    SetSysReg {
        vcpu: u32,
        reg: SysReg,
        value: u64,
    },
    Run {
        vcpu: u32,
    },
    IrqLine {
        intid: u32,
        level: bool,
    },
    CreateIrqChip,
    VtimerMasked {
        vcpu: u32,
        masked: bool,
    },
    PpiPending {
        vcpu: u32,
        intid: u32,
        pending: bool,
    },
}

/// One scripted exit: the registers the "guest" leaves behind, then the
/// exit `run` reports.
#[derive(Debug, Clone)]
pub struct Step {
    pub regs: Vec<(Reg, u64)>,
    pub exit: Exit,
}

impl Step {
    /// An exit that changes no registers.
    pub fn exit(exit: Exit) -> Step {
        Step {
            regs: Vec::new(),
            exit,
        }
    }

    /// Sets a register before the exit is reported, as the guest would.
    pub fn with_reg(mut self, reg: Reg, value: u64) -> Step {
        self.regs.push((reg, value));
        self
    }
}

#[derive(Debug, Default)]
struct State {
    events: Vec<Event>,
    scripts: HashMap<u32, VecDeque<Step>>,
    irqchip_state: Vec<u8>,
    now: u64,
}

/// The mock backend. Clones share the same script and event log, so a test
/// can keep one to inspect while the VMM owns another.
#[derive(Debug, Clone, Default)]
pub struct MockBackend {
    state: Arc<Mutex<State>>,
}

impl MockBackend {
    pub fn new() -> MockBackend {
        MockBackend::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues exits for vCPU `vcpu`, in order.
    pub fn script(&self, vcpu: u32, steps: impl IntoIterator<Item = Step>) {
        self.state().scripts.entry(vcpu).or_default().extend(steps);
    }

    /// Everything recorded so far.
    pub fn events(&self) -> Vec<Event> {
        self.state().events.clone()
    }

    /// Sets the host clock [`Backend::now_ticks`] reports.
    pub fn set_now(&self, ticks: u64) {
        self.state().now = ticks;
    }

    fn record(&self, event: Event) {
        self.state().events.push(event);
    }
}

impl Backend for MockBackend {
    type Vcpu<'a> = MockVcpu;

    fn map_memory(&mut self, memory: &GuestMemory) -> Result<()> {
        for region in memory.regions() {
            self.record(Event::MapMemory {
                gpa: region.gpa(),
                size: region.size(),
            });
        }
        Ok(())
    }

    fn create_vcpu(&self, index: u32) -> Result<MockVcpu> {
        self.record(Event::CreateVcpu { vcpu: index });
        Ok(MockVcpu {
            index,
            backend: self.clone(),
            regs: HashMap::new(),
            sys_regs: HashMap::new(),
            simd: HashMap::new(),
            icc: HashMap::new(),
            vtimer_masked: false,
            vtimer_offset: 0,
        })
    }

    fn create_irqchip(&mut self) -> Result<IrqChip> {
        self.record(Event::CreateIrqChip);
        Ok(IrqChip {
            dist_size: 0x1_0000,
            redist_size: 0x2_0000,
        })
    }

    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()> {
        self.record(Event::IrqLine { intid, level });
        Ok(())
    }

    fn irqchip_state(&self) -> Result<Vec<u8>> {
        Ok(self.state().irqchip_state.clone())
    }

    fn set_irqchip_state(&self, state: &[u8]) -> Result<()> {
        self.state().irqchip_state = state.to_vec();
        Ok(())
    }

    /// The mock clock stands still until [`MockBackend::set_now`] moves it.
    fn now_ticks(&self) -> u64 {
        self.state().now
    }

    fn ticks_to_duration(&self, ticks: u64) -> std::time::Duration {
        std::time::Duration::from_nanos(ticks)
    }
}

/// A mock vCPU. Registers start at zero.
#[derive(Debug)]
pub struct MockVcpu {
    index: u32,
    backend: MockBackend,
    regs: HashMap<Reg, u64>,
    sys_regs: HashMap<SysReg, u64>,
    simd: HashMap<u8, u128>,
    icc: HashMap<IccReg, u64>,
    vtimer_masked: bool,
    vtimer_offset: u64,
}

impl BackendVcpu for MockVcpu {
    fn run(&mut self) -> Result<Exit> {
        self.backend.record(Event::Run { vcpu: self.index });
        let step = self
            .backend
            .state()
            .scripts
            .get_mut(&self.index)
            .and_then(VecDeque::pop_front)
            .ok_or(Error::ScriptExhausted { vcpu: self.index })?;
        self.regs.extend(step.regs);
        // Like Hypervisor.framework, a timer exit masks the timer.
        if step.exit == Exit::VtimerActivated {
            self.vtimer_masked = true;
        }
        Ok(step.exit)
    }

    fn reg(&self, reg: Reg) -> Result<u64> {
        Ok(self.regs.get(&reg).copied().unwrap_or(0))
    }

    fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
        self.backend.record(Event::SetReg {
            vcpu: self.index,
            reg,
            value,
        });
        self.regs.insert(reg, value);
        Ok(())
    }

    fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        Ok(self.sys_regs.get(&reg).copied().unwrap_or(0))
    }

    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> Result<()> {
        self.backend.record(Event::SetSysReg {
            vcpu: self.index,
            reg,
            value,
        });
        self.sys_regs.insert(reg, value);
        Ok(())
    }

    fn vtimer_masked(&self) -> Result<bool> {
        Ok(self.vtimer_masked)
    }

    fn set_vtimer_masked(&mut self, masked: bool) -> Result<()> {
        self.backend.record(Event::VtimerMasked {
            vcpu: self.index,
            masked,
        });
        self.vtimer_masked = masked;
        Ok(())
    }

    fn vtimer_offset(&self) -> Result<u64> {
        Ok(self.vtimer_offset)
    }

    fn set_vtimer_offset(&mut self, offset: u64) -> Result<()> {
        self.vtimer_offset = offset;
        Ok(())
    }

    fn simd_reg(&self, n: u8) -> Result<u128> {
        Ok(self.simd.get(&n).copied().unwrap_or(0))
    }

    fn set_simd_reg(&mut self, n: u8, value: u128) -> Result<()> {
        self.simd.insert(n, value);
        Ok(())
    }

    fn icc_reg(&self, reg: IccReg) -> Result<u64> {
        Ok(self.icc.get(&reg).copied().unwrap_or(0))
    }

    fn set_icc_reg(&mut self, reg: IccReg, value: u64) -> Result<()> {
        self.icc.insert(reg, value);
        Ok(())
    }

    fn set_ppi_pending(&mut self, intid: u32, pending: bool) -> Result<()> {
        self.backend.record(Event::PpiPending {
            vcpu: self.index,
            intid,
            pending,
        });
        Ok(())
    }

    /// Does nothing: scripted runs never block.
    fn kicker(&self) -> Kick {
        Box::new(|| {})
    }
}
