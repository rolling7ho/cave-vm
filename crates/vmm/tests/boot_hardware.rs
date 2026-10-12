//! Boots a tiny stand-in "kernel" through the real boot path: GIC, RAM, the
//! arm64 Image header, the device tree, PSCI and the UART. `#[ignore]`d;
//! run with `cargo test -p cave-vm-vmm -- --ignored`.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use cave_vm_hvf::{Vm, VmConfig};
use cave_vm_vmm::backend::Backend;
use cave_vm_vmm::backend::hvf::HvfBackend;
use cave_vm_vmm::boot::{self, BootConfig};
use cave_vm_vmm::runner::{self, Machine, Stop};

/// An arm64 Image whose code prints `text` on the UART at 0x0900_0000 and
/// then calls PSCI SYSTEM_OFF.
fn fake_kernel(text: &[u8]) -> Vec<u8> {
    let mut code: Vec<u32> = vec![0xd2a1_2001]; // movz x1, #0x0900, lsl #16
    for &c in text {
        code.push(0x5280_0002 | (u32::from(c) << 5)); // movz w2, #c
        code.push(0xb900_0022); // str w2, [x1]
    }
    code.push(0xd280_0100); // movz x0, #0x0008
    code.push(0xf2b0_8000); // movk x0, #0x8400, lsl #16
    code.push(0xd400_0002); // hvc #0

    let mut image = vec![0u8; 64];
    image[0..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // b +64
    image[0x10..0x18].copy_from_slice(&(1u64 << 20).to_le_bytes()); // image_size
    image[0x38..0x3c].copy_from_slice(b"ARM\x64");
    for word in code {
        image.extend_from_slice(&word.to_le_bytes());
    }
    image
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn fake_kernel_prints_and_powers_off() {
    let config = BootConfig {
        kernel: fake_kernel(b"hello from the guest\n"),
        initrd: Some(vec![0xab; 4096]),
        cmdline: "console=ttyAMA0".into(),
        memory_size: 64 << 20,
        cpus: 1,
        virtio_slots: 0,
    };
    let vm = Vm::new(&VmConfig::default()).unwrap();
    let mut backend = HvfBackend::new(&vm);
    let prepared = boot::prepare(&mut backend, &config).unwrap();
    let mut vcpu = backend.create_vcpu(0).unwrap();
    boot::setup_vcpu(&mut vcpu, 0, &prepared.layout).unwrap();

    let mut out = Vec::new();
    let machine = Machine::new(&mut out, None, 1);
    let stop = runner::run(&backend, &mut vcpu, &machine).unwrap();
    drop(machine);

    assert_eq!(stop, Stop::PowerOff);
    assert_eq!(String::from_utf8(out).unwrap(), "hello from the guest\n");

    let mut dtb_magic = [0u8; 4];
    prepared
        .memory
        .read(prepared.layout.dtb, &mut dtb_magic)
        .unwrap();
    assert_eq!(
        dtb_magic,
        [0xd0, 0x0d, 0xfe, 0xed],
        "device tree in guest RAM"
    );
}
