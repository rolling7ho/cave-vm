//! The vCPU run loop: runs the guest and handles what it traps on.
//!
//! Handles PSCI over HVC (including starting and stopping secondary vCPUs),
//! MMIO to the UART and the virtio-mmio slots, WFI by sleeping until the
//! virtual timer is due or input arrives, and the virtual timer interrupt.
//! Hypervisor.framework handles WFI itself once the VM has a GIC, so a
//! thread that gives the machine work (such as input) kicks the vCPU with
//! [`BackendVcpu::kicker`](crate::backend::BackendVcpu::kicker).
//! Generic over [`Backend`] so it runs against the mock in tests.
//!
//! Each vCPU runs on its own thread, because a Hypervisor.framework vCPU
//! belongs to the thread that created it. The vCPUs share one [`Machine`]:
//! devices sit behind a lock taken only for a single access, and the PSCI
//! power state of every vCPU lives in [`Machine`] too. vCPU 0 runs with
//! [`run`]; the others with [`run_secondary`], which waits for the guest to
//! start them with `CPU_ON`.

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::{self, Backend, BackendVcpu, DataAccess, Exit, Kick, Reg, SysReg, Trap};
use crate::boot;
use crate::devices::pl011::Pl011;
use crate::devices::virtio::mmio::MmioTransport;
use crate::layout;
use crate::psci;

/// Interrupt ID of the EL1 virtual timer (PPI 11).
pub const VTIMER_INTID: u32 = 27;
/// Longest a WFI sleeps before checking for input again.
const MAX_WFI_SLEEP: Duration = Duration::from_millis(10);
/// How often a powered-off vCPU checks whether the machine is stopping.
const POWER_POLL: Duration = Duration::from_millis(20);

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
    /// Another thread stopped the machine through [`Machine::stop_flag`].
    Canceled,
}

/// PSCI power state of one vCPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Off,
    /// `CPU_ON` was called; the vCPU starts at `entry` with `context` in
    /// `x0` when its thread next looks.
    OnPending {
        entry: u64,
        context: u64,
    },
    On,
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

/// The devices the vCPUs share.
pub struct Devices<W: Write> {
    pub uart: Pl011<W>,
    /// Virtio devices, by slot.
    virtio: Vec<MmioTransport>,
    uart_irq: bool,
    virtio_irq: Vec<bool>,
}

impl<W: Write> Devices<W> {
    /// The virtio device in `slot`.
    pub fn virtio(&self, slot: u32) -> Option<&MmioTransport> {
        self.virtio.get(slot as usize)
    }

    /// Drives the UART's and each virtio device's interrupt line to match
    /// the device.
    fn sync_irqs<B: Backend>(&mut self, backend: &B) -> Result<()> {
        let level = self.uart.irq_level();
        if level != self.uart_irq {
            backend.set_irq_line(layout::uart_intid(), level)?;
            self.uart_irq = level;
        }
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

struct CpuTable {
    power: Vec<Power>,
    kicks: Vec<Option<Kick>>,
    /// Set when the guest powers off or resets the whole machine.
    system: Option<Stop>,
}

/// The devices, vCPU power states and settings the run loops share.
pub struct Machine<W: Write> {
    devices: Mutex<Devices<W>>,
    /// Bytes typed on the host, for the UART.
    input: Mutex<Option<Receiver<Vec<u8>>>>,
    cpus: Mutex<CpuTable>,
    power_changed: Condvar,
    stop: Arc<AtomicBool>,
}

impl<W: Write> Machine<W> {
    /// A machine with `cpus` vCPUs, of which only vCPU 0 is on.
    pub fn new(uart_out: W, input: Option<Receiver<Vec<u8>>>, cpus: u32) -> Machine<W> {
        assert!(cpus >= 1, "a machine needs a vCPU");
        let mut power = vec![Power::Off; cpus as usize];
        power[0] = Power::On;
        Machine {
            devices: Mutex::new(Devices {
                uart: Pl011::new(uart_out),
                virtio: Vec::new(),
                uart_irq: false,
                virtio_irq: Vec::new(),
            }),
            input: Mutex::new(input),
            cpus: Mutex::new(CpuTable {
                power,
                kicks: (0..cpus).map(|_| None).collect(),
                system: None,
            }),
            power_changed: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set this, then kick any vCPU, to stop every run loop with
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

    /// Number of vCPUs.
    pub fn cpus(&self) -> u32 {
        self.cpu_table().power.len() as u32
    }

    /// The shared devices. Hold the guard briefly: vCPUs wait for it.
    pub fn devices(&self) -> MutexGuard<'_, Devices<W>> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cpu_table(&self) -> MutexGuard<'_, CpuTable> {
        self.cpus.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts a virtio device in the next free slot and returns the slot. The
    /// device tree must list at least this many slots.
    pub fn add_virtio(&mut self, transport: MmioTransport) -> u32 {
        let devices = self.devices.get_mut().unwrap_or_else(|e| e.into_inner());
        assert!(
            (devices.virtio.len() as u32) < layout::VIRTIO_MMIO_SLOTS,
            "out of virtio-mmio slots"
        );
        devices.virtio.push(transport);
        devices.virtio_irq.push(false);
        devices.virtio.len() as u32 - 1
    }

    /// The power state of every vCPU, for a snapshot.
    pub fn power(&self) -> Vec<Power> {
        self.cpu_table().power.clone()
    }

    /// Sets the power state of every vCPU, when restoring a snapshot.
    pub fn set_power(&self, power: &[Power]) {
        let mut table = self.cpu_table();
        assert_eq!(power.len(), table.power.len(), "one state per vCPU");
        table.power.copy_from_slice(power);
        self.power_changed.notify_all();
    }

    /// Why the machine as a whole stopped, if it did.
    fn stopping(&self) -> Option<Stop> {
        if let Some(stop) = self.cpu_table().system {
            return Some(stop);
        }
        self.stop.load(Ordering::Acquire).then_some(Stop::Canceled)
    }

    /// Stops every vCPU with `stop`, from the guest's `SYSTEM_OFF` or
    /// `SYSTEM_RESET`.
    fn stop_all(&self, stop: Stop) {
        let mut table = self.cpu_table();
        table.system.get_or_insert(stop);
        for kick in table.kicks.iter().flatten() {
            kick();
        }
        self.power_changed.notify_all();
    }

    /// Sets the stop flag and kicks every vCPU, so all run loops return
    /// [`Stop::Canceled`]. For a caller whose vCPU thread failed and needs
    /// the others to stop too.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.kick_all();
    }

    /// Kicks every running vCPU so it notices the stop flag.
    fn kick_all(&self) {
        let table = self.cpu_table();
        for kick in table.kicks.iter().flatten() {
            kick();
        }
        self.power_changed.notify_all();
    }

    fn register_kick(&self, index: u32, kick: Kick) {
        self.cpu_table().kicks[index as usize] = Some(kick);
    }

    /// PSCI `CPU_ON`.
    fn power_on(&self, target: u32, entry: u64, context: u64) -> i64 {
        let mut table = self.cpu_table();
        match table.power[target as usize] {
            Power::On => psci::ret::ALREADY_ON,
            Power::OnPending { .. } => psci::ret::ON_PENDING,
            Power::Off => {
                table.power[target as usize] = Power::OnPending { entry, context };
                self.power_changed.notify_all();
                psci::ret::SUCCESS
            }
        }
    }

    /// PSCI `AFFINITY_INFO`.
    fn affinity_info(&self, target: u32) -> i64 {
        match self.cpu_table().power[target as usize] {
            Power::On => psci::affinity::ON,
            Power::Off => psci::affinity::OFF,
            Power::OnPending { .. } => psci::affinity::ON_PENDING,
        }
    }

    fn power_off(&self, index: u32) {
        self.cpu_table().power[index as usize] = Power::Off;
    }

    /// Waits until vCPU `index` is started, returning the entry point and
    /// context ID and marking it on, or `None` if the machine stops first.
    fn wait_for_power_on(&self, index: u32) -> Option<(u64, u64)> {
        let mut table = self.cpu_table();
        loop {
            if table.system.is_some() || self.stop.load(Ordering::Acquire) {
                return None;
            }
            if let Power::OnPending { entry, context } = table.power[index as usize] {
                table.power[index as usize] = Power::On;
                return Some((entry, context));
            }
            table = self
                .power_changed
                .wait_timeout(table, POWER_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Moves waiting input into the UART, waiting up to `timeout` for some
    /// to arrive. Returns whether any did. Only one vCPU waits for input
    /// at a time; another one just sleeps.
    fn take_input(&self, timeout: Duration) -> bool {
        let Ok(mut input) = self.input.try_lock() else {
            if !timeout.is_zero() {
                std::thread::sleep(timeout);
            }
            return false;
        };
        let Some(rx) = input.as_ref() else {
            drop(input);
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
            *input = None;
        }
        drop(input);
        let Some(first) = first else { return false };
        let mut devices = self.devices();
        devices.uart.push_input(&first);
        for more in rest {
            devices.uart.push_input(&more);
        }
        true
    }
}

/// Runs vCPU 0 until the guest stops it or an error occurs.
pub fn run<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &mut V,
    machine: &Machine<W>,
) -> Result<Stop> {
    run_vcpu(backend, vcpu, 0, machine)
}

/// Runs secondary vCPU `index`: waits for the guest to start it with PSCI
/// `CPU_ON`, runs it, and waits again after `CPU_OFF`. A vCPU already on
/// (from a restored snapshot) runs straight away with its restored state.
/// Returns when the machine stops.
pub fn run_secondary<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &mut V,
    index: u32,
    machine: &Machine<W>,
) -> Result<Stop> {
    assert!(index > 0 && index < machine.cpus(), "not a secondary vCPU");
    machine.register_kick(index, vcpu.kicker());
    let mut restored_on = machine.cpu_table().power[index as usize] == Power::On;
    loop {
        if !restored_on {
            let Some((entry, context)) = machine.wait_for_power_on(index) else {
                return Ok(machine.stopping().unwrap_or(Stop::Canceled));
            };
            boot::start_secondary(vcpu, entry, context)?;
        }
        restored_on = false;
        match run_vcpu(backend, vcpu, index, machine)? {
            Stop::CpuOff => continue,
            stop => return Ok(stop),
        }
    }
}

fn run_vcpu<B: Backend, V: BackendVcpu, W: Write>(
    backend: &B,
    vcpu: &mut V,
    index: u32,
    machine: &Machine<W>,
) -> Result<Stop> {
    machine.register_kick(index, vcpu.kicker());
    loop {
        if let Some(stop) = machine.stopping() {
            machine.kick_all();
            return Ok(stop);
        }
        machine.take_input(Duration::ZERO);
        machine.devices().sync_irqs(backend)?;
        sync_vtimer(backend, vcpu)?;

        match vcpu.run()? {
            // The loop top checks for a stop.
            Exit::Canceled => {}
            Exit::VtimerActivated => vcpu.set_ppi_pending(VTIMER_INTID, true)?,
            Exit::Unknown(reason) => return Err(Error::UnknownExit(reason)),
            Exit::Exception(exception) => {
                if let Some(stop) = handle_trap(
                    backend,
                    vcpu,
                    index,
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
    index: u32,
    machine: &Machine<W>,
    trap: Trap,
    ipa: u64,
) -> Result<Option<Stop>> {
    match trap {
        // HVC returns past the instruction already.
        Trap::Hvc { .. } => return psci_call(vcpu, index, machine),
        Trap::Smc { .. } => {
            let stop = psci_call(vcpu, index, machine)?;
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

fn psci_call<V: BackendVcpu, W: Write>(
    vcpu: &mut V,
    index: u32,
    machine: &Machine<W>,
) -> Result<Option<Stop>> {
    let mut args = [0u64; 4];
    for (i, arg) in args.iter_mut().enumerate() {
        *arg = vcpu.reg(Reg::x(i as u8).expect("x0 to x3"))?;
    }
    let value = match psci::handle(args, machine.cpus()) {
        psci::Action::Return(value) => value,
        psci::Action::CpuOn {
            target,
            entry,
            context,
        } => machine.power_on(target, entry, context),
        psci::Action::AffinityInfo { target } => machine.affinity_info(target),
        psci::Action::CpuOff => {
            machine.power_off(index);
            return Ok(Some(Stop::CpuOff));
        }
        psci::Action::SystemOff => {
            machine.stop_all(Stop::PowerOff);
            return Ok(Some(Stop::PowerOff));
        }
        psci::Action::SystemReset => {
            machine.stop_all(Stop::Reset);
            return Ok(Some(Stop::Reset));
        }
    };
    vcpu.set_reg(Reg::x(0).expect("x0"), value as u64)?;
    Ok(None)
}

fn mmio<V: BackendVcpu, W: Write>(
    vcpu: &mut V,
    machine: &Machine<W>,
    ipa: u64,
    access: DataAccess,
) -> Result<()> {
    let in_uart = (layout::UART_BASE..layout::UART_BASE + layout::UART_SIZE).contains(&ipa);
    let virtio = layout::virtio_slot_of(ipa)
        .filter(|&(slot, _)| (slot as usize) < machine.devices().virtio.len());
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
        let mut devices = machine.devices();
        if in_uart {
            devices
                .uart
                .write(ipa - layout::UART_BASE, (value & mask) as u32);
        } else if let Some((slot, offset)) = virtio {
            devices.virtio[slot as usize].write(offset, &value.to_le_bytes()[..size]);
        }
    } else {
        let mut devices = machine.devices();
        let value = if in_uart {
            u64::from(devices.uart.read(ipa - layout::UART_BASE)) & mask
        } else if let Some((slot, offset)) = virtio {
            let mut bytes = [0u8; 8];
            devices.virtio[slot as usize].read(offset, &mut bytes[..size]);
            u64::from_le_bytes(bytes)
        } else {
            0
        };
        drop(devices);
        if access.reg != 31 {
            vcpu.set_reg(
                Reg::x(access.reg).expect("srt below 31"),
                extend_load(value, size, access),
            )?;
        }
    }
    Ok(())
}

/// Applies a load's sign extension (`LDRSB`, `LDRSH`, `LDRSW`) and register
/// width to the `size`-byte value a device returned.
fn extend_load(value: u64, size: usize, access: DataAccess) -> u64 {
    let bits = size as u32 * 8;
    let value = if access.sign_extend && bits < 64 {
        let shift = 64 - bits;
        (((value << shift) as i64) >> shift) as u64
    } else {
        value
    };
    if access.wide_reg {
        value
    } else {
        value & 0xffff_ffff
    }
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
    machine: &Machine<W>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn load(size: u8, sign_extend: bool, wide_reg: bool) -> DataAccess {
        DataAccess {
            write: false,
            size,
            reg: 0,
            sign_extend,
            wide_reg,
        }
    }

    #[test]
    fn loads_sign_extend_to_the_register_width() {
        // LDRSB x0 and LDRSB w0 of 0x80.
        assert_eq!(
            extend_load(0x80, 1, load(1, true, true)),
            0xffff_ffff_ffff_ff80
        );
        assert_eq!(extend_load(0x80, 1, load(1, true, false)), 0xffff_ff80);
        // LDRSW x0 of a negative word.
        assert_eq!(
            extend_load(0x8000_0000, 4, load(4, true, true)),
            0xffff_ffff_8000_0000
        );
        // LDRSH of a positive halfword stays positive.
        assert_eq!(extend_load(0x7fff, 2, load(2, true, true)), 0x7fff);
        // Plain loads zero-extend.
        assert_eq!(extend_load(0x80, 1, load(1, false, false)), 0x80);
        assert_eq!(extend_load(u64::MAX, 8, load(8, false, true)), u64::MAX);
    }
}
