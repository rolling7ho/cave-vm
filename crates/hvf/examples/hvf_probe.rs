//! Creates a VM and a vCPU, runs one instruction, and reports the result.
//! Used by the P2.8 signing check: build it, sign it different ways, install
//! it different ways, and see whether it can still create a VM.
//!
//! ```sh
//! cargo build -p cave-vm-hvf --example hvf_probe
//! ```

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use cave_vm_hvf::{Exit, Reg, Trap, Vm, VmConfig};

    let vm = match Vm::new(&VmConfig::default()) {
        Ok(vm) => vm,
        Err(e) => {
            println!("vm-create: FAILED ({e})");
            std::process::exit(2);
        }
    };
    println!("vm-create: ok");
    let mut vcpu = vm.create_vcpu().expect("create vCPU");
    // With nothing mapped, the first instruction fetch traps to the host.
    vcpu.set_reg(Reg::PC, 0x1000).expect("set PC");
    vcpu.set_reg(Reg::CPSR, 0x3c5).expect("set CPSR");
    match vcpu.run() {
        Ok(Exit::Exception(e)) if matches!(e.decode(), Trap::InstructionAbort { .. }) => {
            println!("vcpu-run: ok (instruction abort at {:#x})", e.physical_address)
        }
        other => {
            println!("vcpu-run: unexpected {other:?}");
            std::process::exit(3);
        }
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    println!("hvf_probe needs an Apple Silicon Mac");
}
