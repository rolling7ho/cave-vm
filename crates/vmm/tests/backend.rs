//! The same small run loop driven by the mock backend (anywhere) and by
//! Hypervisor.framework (on a Mac, `#[ignore]`d).

#![cfg(target_os = "macos")]

use cave_vm_vmm::backend::mock::{Event, MockBackend, Step};
use cave_vm_vmm::backend::{Backend, BackendVcpu, DataAccess, Exception, Exit, Reg, Trap};
use cave_vm_vmm::memory::{GuestMemory, PAGE_SIZE, Region};

const RAM: u64 = 0x4000_0000;
const DEVICE: u64 = 0x0900_0000;

/// Runs vCPU 0 from `RAM` until an HVC, emulating MMIO stores by recording
/// them. Returns the stores seen and the HVC immediate.
fn run_until_hvc<B: Backend>(backend: &mut B, memory: &GuestMemory) -> (Vec<(u64, u64)>, u16) {
    backend.map_memory(memory).unwrap();
    let mut vcpu = backend.create_vcpu(0).unwrap();
    vcpu.set_reg(Reg::PC, RAM).unwrap();
    vcpu.set_reg(Reg::CPSR, 0x3c5).unwrap();
    let mut stores = Vec::new();
    loop {
        let Exit::Exception(e) = vcpu.run().unwrap() else {
            panic!("unexpected exit");
        };
        match e.decode() {
            Trap::DataAbort {
                ipa,
                access:
                    Some(DataAccess {
                        write: true, reg, ..
                    }),
            } => {
                stores.push((ipa, vcpu.reg(Reg::x(reg).unwrap()).unwrap()));
                let pc = vcpu.reg(Reg::PC).unwrap();
                vcpu.set_reg(Reg::PC, pc + 4).unwrap();
            }
            Trap::Hvc { imm } => return (stores, imm),
            other => panic!("unexpected trap {other:?}"),
        }
    }
}

fn data_abort_store(ipa: u64, reg: u32) -> Exit {
    let iss = (1 << 24) | (3 << 22) | (reg << 16) | (1 << 15) | (1 << 6);
    Exit::Exception(Exception {
        syndrome: (0x24u64 << 26) | (1 << 25) | iss as u64,
        virtual_address: ipa,
        physical_address: ipa,
    })
}

fn hvc(imm: u16) -> Exit {
    Exit::Exception(Exception {
        syndrome: (0x16u64 << 26) | (1 << 25) | imm as u64,
        virtual_address: 0,
        physical_address: 0,
    })
}

#[test]
fn mock_replays_a_scripted_exit_sequence() {
    let mock = MockBackend::new();
    mock.script(
        0,
        [
            Step::exit(data_abort_store(DEVICE, 1)).with_reg(Reg::x(1).unwrap(), 0xaa),
            Step::exit(data_abort_store(DEVICE + 8, 2)).with_reg(Reg::x(2).unwrap(), 0xbb),
            Step::exit(hvc(7)),
        ],
    );
    let memory = GuestMemory::new(vec![Region::anonymous(RAM, PAGE_SIZE).unwrap()]).unwrap();
    let (stores, imm) = run_until_hvc(&mut mock.clone(), &memory);

    assert_eq!(stores, vec![(DEVICE, 0xaa), (DEVICE + 8, 0xbb)]);
    assert_eq!(imm, 7);
    let events = mock.events();
    assert_eq!(
        events[0],
        Event::MapMemory {
            gpa: RAM,
            size: PAGE_SIZE as usize
        }
    );
    assert_eq!(events[1], Event::CreateVcpu { vcpu: 0 });
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Run { .. }))
            .count(),
        3
    );
    // The loop stepped PC past each store.
    assert!(events.contains(&Event::SetReg {
        vcpu: 0,
        reg: Reg::PC,
        value: RAM + 4
    }));
}

#[test]
fn exhausted_script_is_an_error() {
    let mock = MockBackend::new();
    let mut vcpu = mock.create_vcpu(3).unwrap();
    assert!(vcpu.run().is_err());
}

#[cfg(target_arch = "aarch64")]
#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn hvf_runs_the_same_loop() {
    use cave_vm_hvf::{Vm, VmConfig};
    use cave_vm_vmm::backend::hvf::HvfBackend;

    // movz x1, #0xaa; movz x2, #0xbb; movz x3, #0x0900, lsl #16;
    // str x1, [x3]; str x2, [x3, #8]; hvc #7
    let program: [u32; 6] = [
        0xd280_1541,
        0xd280_1762,
        0xd2a1_2003,
        0xf900_0061,
        0xf900_0462,
        0xd400_00e2,
    ];
    let memory = GuestMemory::new(vec![Region::anonymous(RAM, PAGE_SIZE).unwrap()]).unwrap();
    let bytes: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    memory.write(RAM, &bytes).unwrap();

    let vm = Vm::new(&VmConfig::default()).unwrap();
    let mut backend = HvfBackend::new(&vm);
    let (stores, imm) = run_until_hvc(&mut backend, &memory);
    assert_eq!(stores, vec![(DEVICE, 0xaa), (DEVICE + 8, 0xbb)]);
    assert_eq!(imm, 7);
}
