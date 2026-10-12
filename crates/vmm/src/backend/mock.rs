//! A backend that runs no guest. Each vCPU replays a script of exits and
//! the backend records what the VMM did, so device models and the run loop
//! can be tested on any machine.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Backend, BackendVcpu, Error, Exit, Reg, Result, SysReg};
use crate::memory::GuestMemory;

/// Something the VMM did to the mock backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MapMemory { gpa: u64, size: usize },
    CreateVcpu { vcpu: u32 },
    SetReg { vcpu: u32, reg: Reg, value: u64 },
    SetSysReg { vcpu: u32, reg: SysReg, value: u64 },
    Run { vcpu: u32 },
    IrqLine { intid: u32, level: bool },
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
        })
    }

    fn set_irq_line(&self, intid: u32, level: bool) -> Result<()> {
        self.record(Event::IrqLine { intid, level });
        Ok(())
    }
}

/// A mock vCPU. Registers start at zero.
#[derive(Debug)]
pub struct MockVcpu {
    index: u32,
    backend: MockBackend,
    regs: HashMap<Reg, u64>,
    sys_regs: HashMap<SysReg, u64>,
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
}
