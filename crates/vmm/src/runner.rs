//! The vCPU run loop: runs the guest and handles what it traps on.
//!
//! Handles PSCI over HVC, MMIO to the UART and the virtio-mmio slots, WFI
//! by sleeping until the
//! virtual timer is due or input arrives, and the virtual timer interrupt.
//! Hypervisor.framework handles WFI itself once the VM has a GIC, so a
//! thread that gives the machine work (such as input) kicks the vCPU with
//! [`BackendVcpu::kicker`](crate::backend::BackendVcpu::kicker).
//! Generic over [`Backend`] so it runs against the mock in tests.

use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::backend::{self, Backend, BackendVcpu, DataAccess, Exit, Reg, SysReg, Trap};
use crate::devices::pl011::Pl011;
use crate::devices::virtio::mmio::MmioTransport;
use crate::layout;
use crate::psci;

/// Interrupt ID of the EL1 virtual timer (PPI 11).
pub const VTIMER_INTID: u32 = 27;
/// Longest a WFI sleeps before checking for input again.
const MAX_WFI_SLEEP: Duration = Duration::from_millis(10);

// CNTV_CTL_EL0 bits.
const CNTV_ENABLE: u64 = 1 << 0;
const CNTV_IMASK: u64 = 1 << 1;

/// Why the run loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest powered off (PSCI `SYSTEM_OFF`).
    PowerOff,
    /// The guest asked for a reset (PSCI `SYSTEM_RESET`).
    Reset,
    /// The vCPU turned itself off (PSCI `CPU_OFF`).
    CpuOff,
    /// Another thread stopped the vCPU through [`Machine::stop_flag`].
    Canceled,
}

/// An error that stops the run loop.
#[derive(Debug)]
pub enum Error {
    Backend(backend::Error),
    /// The guest trapped on something the VMM does not handle.
    UnhandledTrap {
        trap: Trap,
        pc: u64,
    },
    /// A data abort without a syndrome the VMM can emulate.
    UndecodableMmio {
        ipa: u64,
        pc: u64,
    },
    /// An exit reason the VMM does not know.
    UnknownExit(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Backend(e) => e.fmt(f),
            Error::UnhandledTrap { trap, pc } => write!(f, "guest trapped at {pc:#x}: {trap:?}"),
            Error::UndecodableMmio { ipa, pc } => {
                write!(
                    f,
                    "guest at {pc:#x} made an MMIO access to {ipa:#x} that cannot be emulated"
                )
            }
            Error::UnknownExit(reason) => write!(f, "unknown vCPU exit reason {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<backend::Error> for Error {
    fn from(e: backend::Error) -> Error {
        Error::Backend(e)
    }
}

/// Result of the run loop.
pub type Result<T> = std::result::Result<T, Error>;

/// The devices and settings the run loop serves.
pub struct Machine<W: Write> {
    pub uart: Pl011<W>,
    /// Bytes typed on the host, for the UART.
    pub input: Option<Receiver<Vec<u8>>>,
    /// Number of vCPUs in the device tree.
    pub cpus: u32,
    /// Virtio devices, by slot.
    virtio: Vec<MmioTransport>,
    uart_irq: bool,
    virtio_irq: Vec<bool>,
    stop: Arc<AtomicBool>,
}

impl<W: Write> Machine<W> {
    pub fn new(uart_out: W, input: Option<Receiver<Vec<u8>>>, cpus: u32) -> Machine<W> {
        Machine {
            uart: Pl011::new(uart_out),
            input,
            cpus,
            virtio: Vec::new(),
            uart_irq: false,
            virtio_irq: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set this, then kick the vCPU, to stop the run loop with
    /// [`Stop::Canceled`]. A kick without it only makes the loop serve
    /// device work, such as new input.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Uses `flag` as the stop flag, for a caller that needs it before the
    /// machine exists.
    pub fn with_stop_flag(mut self, flag: Arc<AtomicBool>) -> Machine<W> {
        self.stop = flag;
        self
    }

    /// Puts a virtio device in the next free slot and returns the slot. The
    /// device tree must list at least this many slots.
    pub fn add_virtio(&mut self, transport: MmioTransport) -> u32 {
        assert!(
            (self.virtio.len() as u32) < layout::VIRTIO_MMIO_SLOTS,
            "out of virtio-mmio slots"
        );
        self.virtio.push(transport);
        self.virtio_irq.push(false);
        self.virtio.len() as u32 - 1
    }

    /// The virtio device in `slot`.
    pub fn virtio(&self, slot: u32) -> Option<&MmioTransport> {
        self.virtio.get(slot as usize)
    }

    /// Moves waiting input into the UART, waiting up to `timeout` for some
    /// to arrive. Returns whether any did.
    fn take_input(&mut self, timeout: Duration) -> bool {
        let Some(rx) = &self.input else {
            if !timeout.is_zero() {
                std::thread::sleep(timeout);
            }
            return false;
        };
        let mut disconnected = false;
        let first = if timeout.is_zero() {
            rx.try_recv().ok()
        } else {
            match rx.recv_timeout(timeout) {
                Ok(bytes) => Some(bytes),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    None
                }
            }
        };
        let mut rest = Vec::new();
        while let Ok(more) = rx.try_recv() {
            rest.push(more);
        }
        if disconnected {
            self.input = None;
        }
        let Some(first) = first else { return false };
        self.uart.push_input(&first);
        for more in rest {
            self.uart.push_input(&more);
        }
        true
    }

    /// Drives the UART's interrupt line to match the device.
    fn sync_uart_irq<B: Backend>(&mut self, backend: &B) -> Result<()> {
        let level = self.uart.irq_level();
        if level != self.uart_irq {
            backend.set_irq_line(layout::uart_intid(), level)?;
            self.uart_irq = level;
        }
        Ok(())
    }

    /// Drives each virtio device's interrupt line to match its status.
    fn sync_virtio_irqs<B: Backend>(&mut self, backend: &B) -> Result<()> {
        for (slot, (transport, line)) in self.virtio.iter().zip(&mut self.virtio_irq).enumerate() {
            let level = transport.interrupt().level();
            if level != *line {
                backend.set_irq_line(layout::virtio_intid(slot as u32), level)?;
                *line = level;
            }
        }
        Ok(())
    }
}

/// Runs `vcpu` until the guest stops it or an error occurs.
pub fn run<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &mut V,
    machine: &mut Machine<W>,
) -> Result<Stop> {
    loop {
        machine.take_input(Duration::ZERO);
        machine.sync_uart_irq(backend)?;
        machine.sync_virtio_irqs(backend)?;
        sync_vtimer(backend, vcpu)?;

        match vcpu.run()? {
            Exit::Canceled if machine.stop.load(Ordering::Acquire) => return Ok(Stop::Canceled),
            Exit::Canceled => {}
            Exit::VtimerActivated => vcpu.set_ppi_pending(VTIMER_INTID, true)?,
            Exit::Unknown(reason) => return Err(Error::UnknownExit(reason)),
            Exit::Exception(exception) => {
                if let Some(stop) = handle_trap(
                    backend,
                    vcpu,
                    machine,
                    exception.decode(),
                    exception.physical_address,
                )? {
                    return Ok(stop);
                }
            }
        }
    }
}

fn handle_trap<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &mut V,
    machine: &mut Machine<W>,
    trap: Trap,
    ipa: u64,
) -> Result<Option<Stop>> {
    match trap {
        // HVC returns past the instruction already.
        Trap::Hvc { .. } => return psci_call(vcpu, machine.cpus),
        Trap::Smc { .. } => {
            let stop = psci_call(vcpu, machine.cpus)?;
            advance_pc(vcpu)?;
            return Ok(stop);
        }
        Trap::WaitFor { .. } => {
            wait_for_interrupt(backend, vcpu, machine)?;
        }
        Trap::DataAbort {
            access: Some(access),
            ..
        } => mmio(vcpu, machine, ipa, access)?,
        Trap::DataAbort { access: None, ipa } => {
            let pc = vcpu.reg(Reg::PC)?;
            return Err(Error::UndecodableMmio { ipa, pc });
        }
        // Reads of unknown system registers return zero; writes are ignored.
        Trap::SysReg { rt, read, .. } => {
            if read && rt != 31 {
                vcpu.set_reg(Reg::x(rt).expect("rt below 31"), 0)?;
            }
        }
        other => {
            let pc = vcpu.reg(Reg::PC)?;
            return Err(Error::UnhandledTrap { trap: other, pc });
        }
    }
    advance_pc(vcpu)?;
    Ok(None)
}

fn advance_pc<V: BackendVcpu>(vcpu: &mut V) -> Result<()> {
    let pc = vcpu.reg(Reg::PC)?;
    vcpu.set_reg(Reg::PC, pc + 4)?;
    Ok(())
}

fn psci_call<V: BackendVcpu>(vcpu: &mut V, cpus: u32) -> Result<Option<Stop>> {
    let mut args = [0u64; 4];
    for (i, arg) in args.iter_mut().enumerate() {
        *arg = vcpu.reg(Reg::x(i as u8).expect("x0 to x3"))?;
    }
    Ok(match psci::handle(args, cpus) {
        psci::Action::Return(value) => {
            vcpu.set_reg(Reg::x(0).expect("x0"), value as u64)?;
            None
        }
        psci::Action::CpuOff => Some(Stop::CpuOff),
        psci::Action::SystemOff => Some(Stop::PowerOff),
        psci::Action::SystemReset => Some(Stop::Reset),
    })
}

fn mmio<V: BackendVcpu, W: Write>(
    vcpu: &mut V,
    machine: &mut Machine<W>,
    ipa: u64,
    access: DataAccess,
) -> Result<()> {
    let in_uart = (layout::UART_BASE..layout::UART_BASE + layout::UART_SIZE).contains(&ipa);
    let virtio =
        layout::virtio_slot_of(ipa).filter(|&(slot, _)| (slot as usize) < machine.virtio.len());
    let size = usize::from(access.size).clamp(1, 8);
    let mask = if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    };
    if access.write {
        let value = if access.reg == 31 {
            0
        } else {
            vcpu.reg(Reg::x(access.reg).expect("srt below 31"))?
        };
        if in_uart {
            machine
                .uart
                .write(ipa - layout::UART_BASE, (value & mask) as u32);
        } else if let Some((slot, offset)) = virtio {
            machine.virtio[slot as usize].write(offset, &value.to_le_bytes()[..size]);
        }
    } else {
        let value = if in_uart {
            u64::from(machine.uart.read(ipa - layout::UART_BASE)) & mask
        } else if let Some((slot, offset)) = virtio {
            let mut bytes = [0u8; 8];
            machine.virtio[slot as usize].read(offset, &mut bytes[..size]);
            u64::from_le_bytes(bytes)
        } else {
            0
        };
        if access.reg != 31 {
            vcpu.set_reg(Reg::x(access.reg).expect("srt below 31"), value)?;
        }
    }
    Ok(())
}

/// The guest's virtual counter now, and the deadline it has armed, if the
/// timer is enabled and not masked by the guest.
fn vtimer_state<B: Backend, V: BackendVcpu>(backend: &B, vcpu: &V) -> Result<(u64, Option<u64>)> {
    let now = backend.now_ticks().wrapping_sub(vcpu.vtimer_offset()?);
    let ctl = vcpu.sys_reg(SysReg::CNTV_CTL_EL0)?;
    let armed = ctl & CNTV_ENABLE != 0 && ctl & CNTV_IMASK == 0;
    let deadline = if armed {
        Some(vcpu.sys_reg(SysReg::CNTV_CVAL_EL0)?)
    } else {
        None
    };
    Ok((now, deadline))
}

/// After a timer exit the framework keeps the timer masked. Once the guest
/// has handled it (re-armed it later, masked or disabled it), clear the
/// pending interrupt and let the timer exit again.
fn sync_vtimer<B: Backend, V: BackendVcpu>(backend: &B, vcpu: &mut V) -> Result<()> {
    if !vcpu.vtimer_masked()? {
        return Ok(());
    }
    let (now, deadline) = vtimer_state(backend, vcpu)?;
    let still_firing = deadline.is_some_and(|d| now >= d);
    if !still_firing {
        vcpu.set_ppi_pending(VTIMER_INTID, false)?;
        vcpu.set_vtimer_masked(false)?;
    }
    Ok(())
}

/// Sleeps until the virtual timer is due or input arrives, capped so input
/// is never left waiting long.
fn wait_for_interrupt<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &V,
    machine: &mut Machine<W>,
) -> Result<()> {
    let (now, deadline) = vtimer_state(backend, vcpu)?;
    let until_timer = match deadline {
        Some(d) if d > now => backend.ticks_to_duration(d - now),
        Some(_) => Duration::ZERO,
        None => MAX_WFI_SLEEP,
    };
    let sleep = until_timer.min(MAX_WFI_SLEEP);
    if !sleep.is_zero() {
        machine.take_input(sleep);
    }
    Ok(())
}
