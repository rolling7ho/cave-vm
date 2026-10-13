//! Hardware test for guest memory: a real guest writes RAM the host then
//! reads. `#[ignore]`d like the `hvf` hardware tests; run with
//! `cargo test -p cave-vm-vmm -- --ignored`.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use cave_vm_hvf::{Exit, Reg, Trap, Vm, VmConfig};
use cave_vm_vmm::memory::{FileMode, GuestMemory, PAGE_SIZE, Region};

const RAM: u64 = 0x4000_0000;

// A64: str x1, [x2]; hvc #0
const PROGRAM: [u32; 2] = [0xf900_0041, 0xd400_0002];

fn program_bytes() -> Vec<u8> {
    PROGRAM.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A process can hold only one VM, and tests run in parallel.
static ONE_VM: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_store(memory: &GuestMemory, value: u64, target: u64) {
    let _one = ONE_VM.lock().unwrap_or_else(|e| e.into_inner());
    let vm = Vm::new(&VmConfig::default()).unwrap();
    let mapping = memory.map_into(&vm).unwrap();
    let mut vcpu = vm.create_vcpu().unwrap();
    vcpu.set_reg(Reg::PC, RAM).unwrap();
    vcpu.set_reg(Reg::CPSR, 0x3c5).unwrap();
    vcpu.set_reg(Reg::x(1).unwrap(), value).unwrap();
    vcpu.set_reg(Reg::x(2).unwrap(), target).unwrap();
    let Exit::Exception(e) = vcpu.run().unwrap() else {
        panic!("no exception exit")
    };
    assert_eq!(
        e.decode(),
        Trap::Hvc { imm: 0 },
        "the store hit RAM, not MMIO"
    );
    drop(vcpu);
    drop(mapping);
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn guest_writes_are_visible_to_the_host() {
    let memory = GuestMemory::new(vec![Region::anonymous(RAM, 64 * PAGE_SIZE).unwrap()]).unwrap();
    memory.write(RAM, &program_bytes()).unwrap();
    let target = RAM + 10 * PAGE_SIZE + 8;
    run_store(&memory, 0x1122_3344_5566_7788, target);

    let mut buf = [0u8; 8];
    memory.read(target, &mut buf).unwrap();
    assert_eq!(u64::from_le_bytes(buf), 0x1122_3344_5566_7788);
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn copy_on_write_file_memory_runs_a_guest() {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("cave-vm-hw-cow-{}", std::process::id()));
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let mut image = program_bytes();
    image.resize(4 * PAGE_SIZE as usize, 0);
    file.write_all(&image).unwrap();

    let memory = GuestMemory::new(vec![
        Region::from_file(&file, RAM, 4 * PAGE_SIZE, FileMode::CopyOnWrite).unwrap(),
    ])
    .unwrap();
    run_store(&memory, 0xabcd, RAM + PAGE_SIZE);
    let mut buf = [0u8; 8];
    memory.read(RAM + PAGE_SIZE, &mut buf).unwrap();
    assert_eq!(u64::from_le_bytes(buf), 0xabcd);
}
