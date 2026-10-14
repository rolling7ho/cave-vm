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
        devices: Vec::new(),
        guest_flows: Vec::new(),
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
        devices: Vec::new(),
        guest_flows: Vec::new(),
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

#[test]
fn a_snapshot_of_changed_pages_builds_on_its_base() {
    let dir = std::env::temp_dir().join(format!("cave-snapshot-changed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (first, second) = (dir.join("first"), dir.join("second"));
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let size = 0x10_0000;
    let page = 16 * 1024;
    let state = State {
        memory_size: size,
        irqchip: Vec::new(),
        uart: Pl011State::default(),
        devices: Vec::new(),
        guest_flows: Vec::new(),
        vcpus: Vec::new(),
    };
    let memory = GuestMemory::new(vec![Region::anonymous(RAM_BASE, size).unwrap()]).unwrap();
    memory.write(RAM_BASE + 0x10, b"one").unwrap();
    // Marked but still zero: stays a hole over a zero base.
    memory.dirty().mark(RAM_BASE + 5 * page, 1);
    let pages = memory.dirty().take();
    snapshot::save_changed(&first, &state, &memory, &snapshot::Base::Zero, &pages).unwrap();

    // The base stays usable after its snapshot is deleted.
    let base = snapshot::Base::of_snapshot(&first).unwrap();
    std::fs::remove_dir_all(&first).unwrap();
    memory.write(RAM_BASE + 2 * page, b"two").unwrap();
    let pages = memory.dirty().take();
    assert_eq!(pages, [RAM_BASE + 2 * page]);
    snapshot::save_changed(&second, &state, &memory, &base, &pages).unwrap();

    assert_eq!(snapshot::load_state(&second).unwrap(), state);
    let restored = snapshot::load_memory(&second, size).unwrap();
    let mut buf = [0u8; 3];
    restored.read(RAM_BASE + 0x10, &mut buf).unwrap();
    assert_eq!(&buf, b"one", "from the base");
    restored.read(RAM_BASE + 2 * page, &mut buf).unwrap();
    assert_eq!(&buf, b"two", "written since");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_running_machine_is_snapshotted_with_its_disk_and_runs_on() {
    use cave_vm_vmm::backend::mock::{Event, Step};
    use cave_vm_vmm::runner::{self, Machine, Stop};

    let dir = std::env::temp_dir().join(format!("cave-snapshot-take-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("disk.raw");
    std::fs::write(&disk, b"disk contents").unwrap();
    let snap = dir.join("snap");

    let mut mock = MockBackend::new();
    mock.create_irqchip().unwrap();
    let mock = mock;
    mock.set_now(5_000);
    mock.script(0, [Step::until_kicked(), Step::until_kicked()]);
    let size = 0x10_0000;
    let memory = GuestMemory::new(vec![Region::anonymous(RAM_BASE, size).unwrap()]).unwrap();
    memory.write(RAM_BASE + 0x100, b"guest").unwrap();
    let machine = Machine::new(Vec::new(), None, 1);
    let runs = || {
        mock.events()
            .iter()
            .filter(|e| **e == Event::Run { vcpu: 0 })
            .count()
    };
    std::thread::scope(|s| {
        let vcpu = s.spawn(|| {
            let mut vcpu = mock.create_vcpu(0).unwrap();
            vcpu.set_reg(Reg::PC, 0x4000_0040).unwrap();
            runner::run(&mock, &mut vcpu, &machine)
        });
        while runs() == 0 {
            std::thread::yield_now();
        }
        snapshot::take(&snap, &machine, &mock, &memory, &disk, None, None).unwrap();
        // It runs on after the snapshot.
        while runs() < 2 {
            std::thread::yield_now();
        }
        machine.request_stop();
        assert_eq!(vcpu.join().unwrap().unwrap(), Stop::Canceled);
    });

    let state = snapshot::load_state(&snap).unwrap();
    assert_eq!(state.memory_size, size);
    assert_eq!(state.vcpus.len(), 1);
    assert!(state.vcpus[0].regs.contains(&(Reg::PC, 0x4000_0040)));
    assert_eq!(state.vcpus[0].guest_counter, 5_000);
    let restored = snapshot::load_memory(&snap, size).unwrap();
    let mut buf = [0u8; 5];
    restored.read(RAM_BASE + 0x100, &mut buf).unwrap();
    assert_eq!(&buf, b"guest");
    assert_eq!(
        std::fs::read(snap.join(snapshot::DISK_FILE)).unwrap(),
        b"disk contents"
    );

    // A second snapshot into the same folder replaces the files, and a VM
    // still mapping the old memory keeps its copy.
    memory.write(RAM_BASE + 0x100, b"again").unwrap();
    let paused = Machine::new(Vec::new(), None, 1);
    paused.request_stop();
    assert!(snapshot::take(&snap, &paused, &mock, &memory, &disk, None, None).is_err());
    let state = snapshot::load_state(&snap).unwrap();
    snapshot::save(&snap, &state, &memory).unwrap();
    restored.read(RAM_BASE + 0x100, &mut buf).unwrap();
    assert_eq!(&buf, b"guest");
    let fresh = snapshot::load_memory(&snap, size).unwrap();
    fresh.read(RAM_BASE + 0x100, &mut buf).unwrap();
    assert_eq!(&buf, b"again");
    std::fs::remove_dir_all(&dir).unwrap();
}
