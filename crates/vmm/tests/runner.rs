//! The run loop against the mock backend: UART output, PSCI, the virtual
//! timer and virtio-mmio routing, from scripted exits.

#![cfg(target_os = "macos")]

use cave_vm_vmm::backend::mock::{Event, MockBackend, Step};
use cave_vm_vmm::backend::{Backend, BackendVcpu, Exception, Exit, Reg};
use cave_vm_vmm::devices::virtio::mmio::MmioTransport;
use cave_vm_vmm::devices::virtio::{
    ActivateError, Activation, VirtioDevice, device_type, read_config_bytes,
};
use cave_vm_vmm::layout::{UART_BASE, uart_intid, virtio_intid, virtio_mmio_base};
use cave_vm_vmm::memory::{GuestMemory, Region};
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
    let machine = Machine::new(&mut out, None, 1);
    let stop = runner::run(&mock, &mut vcpu, &machine).unwrap();
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
    let machine = Machine::new(Vec::new(), None, 1);
    assert_eq!(
        runner::run(&mock, &mut vcpu, &machine).unwrap(),
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
    let machine = Machine::new(Vec::new(), Some(rx), 1);
    runner::run(&mock, &mut vcpu, &machine).unwrap();

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
    let machine = Machine::new(Vec::new(), None, 1);
    runner::run(&mock, &mut vcpu, &machine).unwrap();

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
    let machine = Machine::new(Vec::new(), None, 1);
    runner::run(&mock, &mut vcpu, &machine).unwrap();
    assert_eq!(vcpu.reg(Reg::PC).unwrap(), PC + 4);
}

#[test]
fn unknown_trap_is_an_error_with_the_pc() {
    let mock = MockBackend::new();
    mock.script(0, [Step::exit(exception(0x3c, 0, 0))]); // BRK
    let mut vcpu = start(&mock);
    let machine = Machine::new(Vec::new(), None, 1);
    let err = runner::run(&mock, &mut vcpu, &machine).unwrap_err();
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
    let machine = Machine::new(Vec::new(), None, 1);
    let stop = runner::run(&mock, &mut vcpu, &machine).unwrap();
    assert_eq!(stop, Stop::PowerOff);
}

#[test]
fn a_kick_with_the_stop_flag_stops() {
    let mock = MockBackend::new();
    mock.script(0, [Step::exit(Exit::Canceled)]);
    let mut vcpu = start(&mock);
    let machine = Machine::new(Vec::new(), None, 1);
    machine
        .stop_flag()
        .store(true, std::sync::atomic::Ordering::Release);
    let stop = runner::run(&mock, &mut vcpu, &machine).unwrap();
    assert_eq!(stop, Stop::Canceled);
}

/// A virtio device with no queues, enough to check the run loop's routing.
struct Blank;

impl VirtioDevice for Blank {
    fn device_type(&self) -> u32 {
        device_type::BLOCK
    }
    fn features(&self) -> u64 {
        0
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[]
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&[0x11, 0x22, 0x33, 0x44], offset, data);
    }
    fn activate(&mut self, _: Activation) -> Result<(), ActivateError> {
        Ok(())
    }
    fn queue_notify(&mut self, _: u16) {}
    fn reset(&mut self) {}
}

fn blank_transport() -> MmioTransport {
    let memory = GuestMemory::new(vec![Region::anonymous(PC, 1 << 20).unwrap()]).unwrap();
    MmioTransport::new(Box::new(Blank), memory)
}

/// An 8-bit load into `reg` from `ipa`.
fn load8(ipa: u64, reg: u64) -> Exit {
    exception(0x24, (1 << 24) | (reg << 16), ipa)
}

#[test]
fn virtio_registers_are_routed_by_slot() {
    let mock = MockBackend::new();
    let slot1 = virtio_mmio_base(1);
    mock.script(
        0,
        [
            Step::exit(load32(slot1, 1)),        // MagicValue
            Step::exit(load32(slot1 + 0x8, 2)),  // DeviceID
            Step::exit(load8(slot1 + 0x102, 3)), // config byte 2
            Step::exit(load32(virtio_mmio_base(2), 4)).with_reg(x(4), 7), // empty slot
            Step::exit(store32(slot1 + 0x70, 5)).with_reg(x(5), 1), // Status = ACKNOWLEDGE
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    assert_eq!(machine.add_virtio(blank_transport()), 0);
    assert_eq!(machine.add_virtio(blank_transport()), 1);
    runner::run(&mock, &mut vcpu, &machine).unwrap();

    assert_eq!(vcpu.reg(x(1)).unwrap(), 0x7472_6976);
    assert_eq!(vcpu.reg(x(2)).unwrap(), u64::from(device_type::BLOCK));
    assert_eq!(vcpu.reg(x(3)).unwrap(), 0x33);
    assert_eq!(vcpu.reg(x(4)).unwrap(), 0, "unused slot reads as zero");
    assert_eq!(machine.devices().virtio(1).unwrap().status(), 1);
    assert_eq!(machine.devices().virtio(0).unwrap().status(), 0);
}

#[test]
fn virtio_interrupt_drives_its_line() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            // InterruptACK of bit 0 on slot 0.
            Step::exit(store32(virtio_mmio_base(0) + 0x64, 1)).with_reg(x(1), 1),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut vcpu = start(&mock);
    let mut machine = Machine::new(Vec::new(), None, 1);
    let transport = blank_transport();
    transport.interrupt().raise(1);
    machine.add_virtio(transport);
    runner::run(&mock, &mut vcpu, &machine).unwrap();

    let lines: Vec<Event> = mock
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::IrqLine { .. }))
        .collect();
    assert_eq!(
        lines,
        vec![
            Event::IrqLine {
                intid: virtio_intid(0),
                level: true
            },
            Event::IrqLine {
                intid: virtio_intid(0),
                level: false
            },
        ]
    );
}

#[test]
fn virtio_interrupt_from_another_thread_wakes_an_idle_vcpu() {
    let mock = MockBackend::new();
    // The guest idles in WFI, which the hypervisor handles without an exit,
    // until something kicks the vCPU.
    mock.script(
        0,
        [
            Step::until_kicked(),
            Step::exit(hvc()).with_reg(x(0), 0x8400_0008),
        ],
    );
    let mut machine = Machine::new(Vec::new(), None, 1);
    let transport = blank_transport();
    let interrupt = transport.interrupt().clone();
    machine.add_virtio(transport);

    let (done, finished) = std::sync::mpsc::channel();
    let runner_mock = mock.clone();
    std::thread::spawn(move || {
        let mut vcpu = start(&runner_mock);
        let stop = runner::run(&runner_mock, &mut vcpu, &machine).unwrap();
        let _ = done.send(stop);
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    interrupt.raise(1);
    let stop = finished
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the interrupt kicked the vCPU");
    assert_eq!(stop, Stop::PowerOff);
    assert!(mock.events().contains(&Event::IrqLine {
        intid: virtio_intid(0),
        level: true
    }));
}
