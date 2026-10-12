//! The run loop against the mock backend: UART output, PSCI and the
//! virtual timer, from scripted exits.

#![cfg(target_os = "macos")]

use cave_vm_vmm::backend::mock::{Event, MockBackend, Step};
use cave_vm_vmm::backend::{Backend, BackendVcpu, Exception, Exit, Reg};
use cave_vm_vmm::layout::{UART_BASE, uart_intid};
use cave_vm_vmm::runner::{self, Machine, Stop, VTIMER_INTID};

const PC: u64 = 0x4000_0000;

fn x(n: u8) -> Reg {
    Reg::x(n).unwrap()
}

fn exception(class: u64, iss: u64, ipa: u64) -> Exit {
    Exit::Exception(Exception {
        syndrome: (class << 26) | (1 << 25) | iss,
        virtual_address: ipa,
        physical_address: ipa,
    })
}

/// A 32-bit store of `reg` to `ipa`.
fn store32(ipa: u64, reg: u64) -> Exit {
    exception(0x24, (1 << 24) | (2 << 22) | (reg << 16) | (1 << 6), ipa)
}

/// A 32-bit load into `reg` from `ipa`.
fn load32(ipa: u64, reg: u64) -> Exit {
    exception(0x24, (1 << 24) | (2 << 22) | (reg << 16), ipa)
}

fn hvc() -> Exit {
    exception(0x16, 0, 0)
}

fn uart_byte(b: u8) -> Step {
    Step::exit(store32(UART_BASE, 1)).with_reg(x(1), u64::from(b))
}

fn start(mock: &MockBackend) -> cave_vm_vmm::backend::mock::MockVcpu {
    let mut vcpu = mock.create_vcpu(0).unwrap();
    vcpu.set_reg(Reg::PC, PC).unwrap();
    vcpu
}

#[test]
fn guest_prints_then_powers_off() {
    let mock = MockBackend::new();
    let mut steps: Vec<Step> = b"ok\n".iter().copied().map(uart_byte).collect();
    steps.push(Step::exit(hvc()).with_reg(x(0), 0x8400_0008)); // SYSTEM_OFF
    mock.script(0, steps);

    let mut vcpu = start(&mock);
    let mut out = Vec::new();
    let mut machine = Machine::new(&mut out, None, 1);
    let stop = runner::run(&mock, &mut vcpu, &mut machine).unwrap();
    drop(machine);

    assert_eq!(stop, Stop::PowerOff);
    assert_eq!(out, b"ok\n");
    assert_eq!(
        vcpu.reg(Reg::PC).unwrap(),
        PC + 12,
        "PC stepped past three stores"
    );
}

#[test]
fn psci_version_is_returned_in_x0() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            Step::exit(hvc()).with_reg(x(0), 0x8400_0000),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0009), // SYSTEM_RESET
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    assert_eq!(
        runner::run(&mock, &mut vcpu, &mut machine).unwrap(),
        Stop::Reset
    );
    assert!(mock.events().contains(&Event::SetReg {
        vcpu: 0,
        reg: x(0),
        value: 0x1_0000
    }));
}

#[test]
fn uart_input_raises_the_interrupt_and_is_read() {
    let mock = MockBackend::new();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(b"a".to_vec()).unwrap();
    mock.script(
        0,
        [
            // Unmask the receive interrupt: write 0x10 to IMSC.
            Step::exit(store32(UART_BASE + 0x38, 1)).with_reg(x(1), 0x10),
            // Read the data register into x2.
            Step::exit(load32(UART_BASE, 2)),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), Some(rx), 1);
    runner::run(&mock, &mut vcpu, &mut machine).unwrap();

    assert_eq!(vcpu.reg(x(2)).unwrap(), u64::from(b'a'));
    let lines: Vec<Event> = mock
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::IrqLine { .. }))
        .collect();
    assert_eq!(
        lines,
        vec![
            Event::IrqLine {
                intid: uart_intid(),
                level: true
            },
            Event::IrqLine {
                intid: uart_intid(),
                level: false
            },
        ]
    );
}

#[test]
fn timer_exit_marks_the_ppi_pending_then_clears_it() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            Step::exit(Exit::VtimerActivated),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    runner::run(&mock, &mut vcpu, &mut machine).unwrap();

    let events = mock.events();
    let pending = events
        .iter()
        .position(|e| {
            *e == Event::PpiPending {
                vcpu: 0,
                intid: VTIMER_INTID,
                pending: true,
            }
        })
        .expect("timer interrupt raised");
    // The guest's timer is disabled in the mock, so the loop clears the
    // interrupt and unmasks the timer before running again.
    assert_eq!(
        &events[pending + 1..pending + 3],
        &[
            Event::PpiPending {
                vcpu: 0,
                intid: VTIMER_INTID,
                pending: false
            },
            Event::VtimerMasked {
                vcpu: 0,
                masked: false
            },
        ]
    );
}

#[test]
fn wfi_steps_over_the_instruction() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            Step::exit(exception(0x01, 0, 0)),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    runner::run(&mock, &mut vcpu, &mut machine).unwrap();
    assert_eq!(vcpu.reg(Reg::PC).unwrap(), PC + 4);
}

#[test]
fn unknown_trap_is_an_error_with_the_pc() {
    let mock = MockBackend::new();
    mock.script(0, [Step::exit(exception(0x3c, 0, 0))]); // BRK
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    let err = runner::run(&mock, &mut vcpu, &mut machine).unwrap_err();
    assert!(err.to_string().contains("0x40000000"), "{err}");
}

#[test]
fn a_kick_without_the_stop_flag_keeps_running() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            Step::exit(Exit::Canceled),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    let stop = runner::run(&mock, &mut vcpu, &mut machine).unwrap();
    assert_eq!(stop, Stop::PowerOff);
}

#[test]
fn a_kick_with_the_stop_flag_stops() {
    let mock = MockBackend::new();
    mock.script(0, [Step::exit(Exit::Canceled)]);
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    machine
        .stop_flag()
        .store(true, std::sync::atomic::Ordering::Release);
    let stop = runner::run(&mock, &mut vcpu, &mut machine).unwrap();
    assert_eq!(stop, Stop::Canceled);
}
