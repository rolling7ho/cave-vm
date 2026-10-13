//! Snapshot save and restore against the mock backend.

#![cfg(target_os = "macos")]

use cave_vm_vmm::backend::mock::MockBackend;
use cave_vm_vmm::backend::{Backend, BackendVcpu, IccReg, RedistReg, Reg, SysReg};
use cave_vm_vmm::devices::pl011::Pl011State;
use cave_vm_vmm::layout::RAM_BASE;
use cave_vm_vmm::memory::{GuestMemory, Region};
use cave_vm_vmm::snapshot::{self, State};

#[test]
fn vcpu_state_moves_to_a_new_vcpu_and_the_counter_continues() {
    let old = MockBackend::new();
    let mut a = old.create_vcpu(0).unwrap();
    a.set_reg(Reg::PC, 0x4000_1234).unwrap();
    a.set_reg(Reg::x(5).unwrap(), 55).unwrap();
    a.set_simd_reg(7, 0xdead_beef_u128 << 64).unwrap();
    a.set_sys_reg(SysReg::TTBR1_EL1, 0x8000).unwrap();
    a.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000).unwrap();
    a.set_icc_reg(IccReg::PMR_EL1, 0xf0).unwrap();
    // SGIs 0 to 7 and the virtual timer enabled, the timer pending.
    a.set_redist_reg(RedistReg::ISENABLER0, 0x0800_00ff)
        .unwrap();
    a.set_redist_reg(RedistReg::ISPENDR0, 1 << 27).unwrap();
    a.set_vtimer_offset(1_000).unwrap();
    old.set_now(11_000);
    let saved = snapshot::save_vcpu(&a, old.now_ticks()).unwrap();
    assert_eq!(saved.guest_counter, 10_000);

    let new = MockBackend::new();
    new.set_now(500_000);
    let mut b = [new.create_vcpu(0).unwrap()];
    // A stray enable on the new vCPU is cleared by the restore.
    b[0].set_redist_reg(RedistReg::ISENABLER0, 1 << 30).unwrap();
    let state = State {
        memory_size: 0,
        irqchip: vec![9, 9],
        uart: Pl011State::default(),
        vcpus: vec![saved.clone()],
    };
    snapshot::restore(&new, &mut b, &state).unwrap();
    assert_eq!(snapshot::save_vcpu(&b[0], new.now_ticks()).unwrap(), saved);
    assert_eq!(new.irqchip_state().unwrap(), vec![9, 9]);
    assert_eq!(b[0].vtimer_offset().unwrap(), 490_000);
    assert_eq!(b[0].redist_reg(RedistReg::ISENABLER0).unwrap(), 0x0800_00ff);
    assert_eq!(b[0].redist_reg(RedistReg::ISPENDR0).unwrap(), 1 << 27);
}

#[test]
fn memory_round_trips_through_a_sparse_file() {
    let dir = std::env::temp_dir().join(format!("cave-snapshot-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let size = 0x10_0000;
    let memory = GuestMemory::new(vec![Region::anonymous(RAM_BASE, size).unwrap()]).unwrap();
    memory.write(RAM_BASE + 0x4321, b"hello").unwrap();
    memory.write(RAM_BASE + size - 1, b"!").unwrap();
    let state = State {
        memory_size: size,
        irqchip: Vec::new(),
        uart: Pl011State::default(),
        vcpus: Vec::new(),
    };
    snapshot::save(&dir, &state, &memory).unwrap();
    assert_eq!(snapshot::load_state(&dir).unwrap(), state);

    let restored = snapshot::load_memory(&dir, size).unwrap();
    let mut buf = [0u8; 5];
    restored.read(RAM_BASE + 0x4321, &mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    restored.read(RAM_BASE + size - 1, &mut buf[..1]).unwrap();
    assert_eq!(buf[0], b'!');
    // Writes after restore stay out of the snapshot file.
    restored.write(RAM_BASE, b"x").unwrap();
    let again = snapshot::load_memory(&dir, size).unwrap();
    again.read(RAM_BASE, &mut buf[..1]).unwrap();
    assert_eq!(buf[0], 0);
    std::fs::remove_dir_all(&dir).unwrap();
}
