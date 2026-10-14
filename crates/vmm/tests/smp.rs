//! Secondary vCPUs against the mock backend: PSCI `CPU_ON`, `CPU_OFF`,
//! `AFFINITY_INFO`, and stopping every vCPU together.

#![cfg(target_os = "macos")]

use cave_vm_vmm::backend::mock::{Event, MockBackend, Step};
use cave_vm_vmm::backend::{Backend, BackendVcpu, Exception, Exit, Reg};
use cave_vm_vmm::psci::function::*;
use cave_vm_vmm::psci::{affinity, ret};
use cave_vm_vmm::runner::{self, Machine, Power, Stop};

const PC: u64 = 0x4000_0000;
const ENTRY: u64 = 0x4008_1000;

fn x(n: u8) -> Reg {
    Reg::x(n).unwrap()
}

fn hvc() -> Exit {
    Exit::Exception(Exception {
        syndrome: (0x16 << 26) | (1 << 25),
        virtual_address: 0,
        physical_address: 0,
    })
}

/// A PSCI call with `x0` to `x3`.
fn psci(func: u32, args: [u64; 3]) -> Step {
    Step::exit(hvc())
        .with_reg(x(0), u64::from(func))
        .with_reg(x(1), args[0])
        .with_reg(x(2), args[1])
        .with_reg(x(3), args[2])
}

/// Values the run loop wrote to `x0` of `vcpu`, in order.
fn x0_writes(mock: &MockBackend, vcpu: u32) -> Vec<u64> {
    mock.events()
        .into_iter()
        .filter_map(|e| match e {
            Event::SetReg {
                vcpu: v,
                reg,
                value,
            } if v == vcpu && reg == x(0) => Some(value),
            _ => None,
        })
        .collect()
}

#[test]
fn power_state_calls_from_the_boot_vcpu() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            psci(AFFINITY_INFO_64, [1, 0, 0]),
            psci(CPU_ON_64, [1, ENTRY, 5]),
            psci(AFFINITY_INFO_64, [1, 0, 0]),
            psci(CPU_ON_64, [1, ENTRY, 5]),
            psci(CPU_ON_64, [0, ENTRY, 5]),
            psci(CPU_ON_64, [2, ENTRY, 5]),
            psci(SYSTEM_OFF, [0; 3]),
        ],
    );
    let mut vcpu = mock.create_vcpu(0).unwrap();
    vcpu.set_reg(Reg::PC, PC).unwrap();
    let machine = Machine::new(Vec::new(), None, 2);
    assert_eq!(
        runner::run(&mock, &mut vcpu, &machine).unwrap(),
        Stop::PowerOff
    );
    let expect: Vec<u64> = [
        affinity::OFF,
        ret::SUCCESS,
        affinity::ON_PENDING,
        ret::ON_PENDING,
        ret::ALREADY_ON,
        ret::INVALID_PARAMETERS,
    ]
    .iter()
    .map(|&v| v as u64)
    .collect();
    assert_eq!(x0_writes(&mock, 0), expect);
    assert_eq!(
        machine.power(),
        vec![
            Power::On,
            Power::OnPending {
                entry: ENTRY,
                context: 5
            }
        ]
    );
}

#[test]
fn cpu_on_starts_the_secondary_at_its_entry_point() {
    let mock = MockBackend::new();
    // vCPU 0 starts vCPU 1, then idles until something kicks it.
    mock.script(
        0,
        [psci(CPU_ON_64, [1, ENTRY, 0xc0ffee]), Step::until_kicked()],
    );
    // vCPU 1 powers the machine off.
    mock.script(1, [psci(SYSTEM_OFF, [0; 3])]);
    let machine = Machine::new(Vec::new(), None, 2);

    let (first, second) = std::thread::scope(|scope| {
        let secondary = scope.spawn(|| {
            let mut vcpu = mock.create_vcpu(1).unwrap();
            runner::run_secondary(&mock, &mut vcpu, 1, &machine).unwrap()
        });
        let mut vcpu = mock.create_vcpu(0).unwrap();
        let first = runner::run(&mock, &mut vcpu, &machine).unwrap();
        (first, secondary.join().unwrap())
    });
    assert_eq!(first, Stop::PowerOff, "vCPU 0 stopped with the machine");
    assert_eq!(second, Stop::PowerOff);

    let set = |reg: Reg, value: u64| Event::SetReg {
        vcpu: 1,
        reg,
        value,
    };
    let events = mock.events();
    assert!(
        events.contains(&set(Reg::PC, ENTRY)),
        "PC at the entry point"
    );
    assert!(
        events.contains(&set(x(0), 0xc0ffee)),
        "x0 holds the context"
    );
    assert!(events.contains(&set(Reg::CPSR, 0x3c5)), "EL1h, masked");
    let first_run = events
        .iter()
        .position(|e| *e == Event::Run { vcpu: 1 })
        .unwrap();
    let pc_set = events
        .iter()
        .position(|e| *e == set(Reg::PC, ENTRY))
        .unwrap();
    assert!(pc_set < first_run, "set up before it runs");
}

#[test]
fn cpu_off_lets_the_secondary_be_started_again() {
    let mock = MockBackend::new();
    // vCPU 1 turns itself off, and once restarted, powers off the machine.
    mock.script(1, [psci(CPU_OFF, [0; 3]), psci(SYSTEM_OFF, [0; 3])]);
    let machine = Machine::new(Vec::new(), None, 2);
    // Start vCPU 1 from here, as vCPU 0's CPU_ON would.
    machine.set_power(&[
        Power::On,
        Power::OnPending {
            entry: ENTRY,
            context: 1,
        },
    ]);

    std::thread::scope(|scope| {
        let secondary = scope.spawn(|| {
            let mut vcpu = mock.create_vcpu(1).unwrap();
            runner::run_secondary(&mock, &mut vcpu, 1, &machine).unwrap()
        });
        // Wait for vCPU 1 to go off, then start it again.
        while machine.power()[1] != Power::Off || !mock.events().contains(&Event::Run { vcpu: 1 }) {
            std::thread::yield_now();
        }
        let mut restarted = machine.power();
        restarted[1] = Power::OnPending {
            entry: ENTRY + 8,
            context: 2,
        };
        machine.set_power(&restarted);
        assert_eq!(secondary.join().unwrap(), Stop::PowerOff);

        let mut vcpu = mock.create_vcpu(0).unwrap();
        assert_eq!(
            runner::run(&mock, &mut vcpu, &machine).unwrap(),
            Stop::PowerOff
        );
    });
    assert!(mock.events().contains(&Event::SetReg {
        vcpu: 1,
        reg: Reg::PC,
        value: ENTRY + 8
    }));
}

#[test]
fn stop_request_ends_running_and_waiting_vcpus() {
    let mock = MockBackend::new();
    mock.script(0, [Step::until_kicked()]);
    let machine = Machine::new(Vec::new(), None, 3);

    let stops = std::thread::scope(|scope| {
        let waiting: Vec<_> = [1, 2]
            .into_iter()
            .map(|index| {
                let (mock, machine) = (&mock, &machine);
                scope.spawn(move || {
                    let mut vcpu = mock.create_vcpu(index).unwrap();
                    runner::run_secondary(mock, &mut vcpu, index, machine).unwrap()
                })
            })
            .collect();
        let boot = scope.spawn(|| {
            let mut vcpu = mock.create_vcpu(0).unwrap();
            runner::run(&mock, &mut vcpu, &machine).unwrap()
        });
        while !mock.events().contains(&Event::Run { vcpu: 0 }) {
            std::thread::yield_now();
        }
        machine.request_stop();
        let mut stops = vec![boot.join().unwrap()];
        stops.extend(waiting.into_iter().map(|t| t.join().unwrap()));
        stops
    });
    assert_eq!(stops, [Stop::Canceled; 3]);
    assert!(
        !mock.events().contains(&Event::Run { vcpu: 1 }),
        "never started"
    );
}

#[test]
fn restored_secondary_that_was_on_runs_without_cpu_on() {
    let mock = MockBackend::new();
    mock.script(1, [psci(SYSTEM_OFF, [0; 3])]);
    let machine = Machine::new(Vec::new(), None, 2);
    machine.set_power(&[Power::On, Power::On]);
    let mut vcpu = mock.create_vcpu(1).unwrap();
    assert_eq!(
        runner::run_secondary(&mock, &mut vcpu, 1, &machine).unwrap(),
        Stop::PowerOff
    );
    assert!(
        !mock.events().iter().any(|e| matches!(
            e,
            Event::SetReg {
                vcpu: 1,
                reg: Reg::PC,
                ..
            }
        )),
        "restored registers are left alone"
    );
}

/// Waits up to five seconds for `done`.
fn wait_until(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn runs(mock: &MockBackend, vcpu: u32) -> usize {
    mock.events()
        .iter()
        .filter(|e| **e == Event::Run { vcpu })
        .count()
}

#[test]
fn pause_saves_every_vcpu_and_resume_lets_them_run_on() {
    let mock = MockBackend::new();
    mock.set_now(1_000);
    // vCPU 0 idles in the guest until the pause kicks it, then until the
    // stop does. vCPU 1 stays off.
    mock.script(
        0,
        [
            Step::until_kicked().with_reg(x(7), 77),
            Step::until_kicked(),
        ],
    );
    let machine = Machine::new(Vec::new(), None, 2);
    std::thread::scope(|s| {
        let boot = s.spawn(|| {
            let mut vcpu = mock.create_vcpu(0).unwrap();
            vcpu.set_reg(Reg::PC, PC).unwrap();
            runner::run(&mock, &mut vcpu, &machine)
        });
        let secondary = s.spawn(|| {
            let mut vcpu = mock.create_vcpu(1).unwrap();
            runner::run_secondary(&mock, &mut vcpu, 1, &machine)
        });
        wait_until(|| runs(&mock, 0) == 1);

        let states = machine.pause(&mock).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!((states[0].power, states[1].power), (Power::On, Power::Off));
        assert!(states[0].regs.contains(&(x(7), 77)), "{:?}", states[0].regs);
        assert!(states[0].regs.contains(&(Reg::PC, PC)));
        assert_eq!(states[0].guest_counter, 1_000);
        assert!(machine.pause(&mock).is_err(), "already paused");
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(runs(&mock, 0), 1, "paused vCPUs do not run");

        machine.resume();
        wait_until(|| runs(&mock, 0) == 2);
        machine.request_stop();
        assert_eq!(boot.join().unwrap().unwrap(), Stop::Canceled);
        assert_eq!(secondary.join().unwrap().unwrap(), Stop::Canceled);
    });
}

#[test]
fn a_pause_waits_until_no_vcpu_is_in_an_interrupt_handler() {
    use cave_vm_vmm::backend::IccReg;

    let mock = MockBackend::new();
    // Paused first inside a handler, then again once it has finished.
    mock.script(
        0,
        [
            Step::until_kicked().with_icc_reg(IccReg::AP1R0_EL1, 1 << 24),
            Step::until_kicked().with_icc_reg(IccReg::AP1R0_EL1, 0),
            Step::until_kicked(),
        ],
    );
    let machine = Machine::new(Vec::new(), None, 1);
    std::thread::scope(|s| {
        let boot = s.spawn(|| {
            let mut vcpu = mock.create_vcpu(0).unwrap();
            runner::run(&mock, &mut vcpu, &machine)
        });
        wait_until(|| runs(&mock, 0) == 1);
        let states = machine.pause(&mock).unwrap();
        assert!(!states[0].in_interrupt());
        assert_eq!(runs(&mock, 0), 2, "ran on once to leave the handler");
        machine.resume();
        machine.request_stop();
        assert_eq!(boot.join().unwrap().unwrap(), Stop::Canceled);
    });
}

#[test]
fn a_stop_ends_a_pause() {
    let mock = MockBackend::new();
    mock.script(0, [Step::until_kicked()]);
    let machine = Machine::new(Vec::new(), None, 1);
    std::thread::scope(|s| {
        let boot = s.spawn(|| {
            let mut vcpu = mock.create_vcpu(0).unwrap();
            runner::run(&mock, &mut vcpu, &machine)
        });
        wait_until(|| runs(&mock, 0) == 1);
        machine.pause(&mock).unwrap();
        machine.request_stop();
        assert_eq!(boot.join().unwrap().unwrap(), Stop::Canceled);
    });
    let err = machine.pause(&mock).unwrap_err().to_string();
    assert!(err.contains("stopping"), "{err}");
}
