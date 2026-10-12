//! Hardware tests: these create real VMs, so they need an Apple Silicon Mac
//! and the hypervisor entitlement. They are `#[ignore]`d so CI (whose
//! runners probably cannot run guests, ROADMAP.md A7) skips them. Run them
//! on a Mac with:
//!
//! ```sh
//! cargo test -p cave-vm-hvf -- --ignored --test-threads=1
//! ```
//!
//! `.cargo/config.toml` signs the test binary with the entitlement first.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::sync::{Mutex, MutexGuard};

use cave_vm_hvf::{
    DataAccess, Error, Exit, GUEST_STATE_ICC_REGS, GUEST_STATE_SYS_REGS, GicConfig, GicInterrupt,
    GicParams, MemFlags, Reg, SysReg, Trap, Vm, VmConfig,
};

/// Host page size on Apple Silicon.
const PAGE: usize = 16 * 1024;
/// Where the test program is mapped in guest physical memory.
const CODE_IPA: u64 = 0x4000_0000;
/// An unmapped guest physical address, standing in for an MMIO device.
const MMIO_IPA: u64 = 0x0900_0000;
/// EL1h with interrupts masked, the state Linux expects at entry.
const PSTATE_EL1H_MASKED: u64 = 0x3c5;

/// One VM per process, so tests take turns.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A page of zeroed, page-aligned host memory holding guest code.
struct GuestPage {
    ptr: *mut u8,
    layout: Layout,
}

impl GuestPage {
    fn with_code(code: &[u32]) -> GuestPage {
        let layout = Layout::from_size_align(PAGE, PAGE).unwrap();
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        assert!(code.len() * 4 <= PAGE);
        // SAFETY: `ptr` is valid for PAGE bytes and does not overlap `code`.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr().cast::<u8>(), ptr, code.len() * 4) };
        GuestPage { ptr, layout }
    }

    fn map(&self, vm: &Vm) {
        // SAFETY: the page is PAGE bytes, PAGE-aligned, and outlives the VM
        // because every test declares the page before the VM, so the VM is
        // dropped first. The host does not touch it while a vCPU runs.
        unsafe { vm.map(self.ptr, CODE_IPA, PAGE, MemFlags::RWX) }.expect("map guest page");
    }
}

impl Drop for GuestPage {
    fn drop(&mut self) {
        // SAFETY: allocated in `with_code` with this layout, freed once.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

// A64 encodings used by the test programs.
const fn movz_x(rd: u32, imm16: u32) -> u32 {
    0xd280_0000 | (imm16 << 5) | rd
}
const fn hvc(imm16: u32) -> u32 {
    0xd400_0002 | (imm16 << 5)
}
const fn str_x(rt: u32, rn: u32) -> u32 {
    0xf900_0000 | (rn << 5) | rt
}
const fn ldr_w(rt: u32, rn: u32) -> u32 {
    0xb940_0000 | (rn << 5) | rt
}

fn x(n: u8) -> Reg {
    Reg::x(n).unwrap()
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn reports_host_limits() {
    let max = cave_vm_hvf::max_ipa_bits().unwrap();
    let default = cave_vm_hvf::default_ipa_bits().unwrap();
    let vcpus = cave_vm_hvf::max_vcpus().unwrap();
    println!("max IPA bits {max}, default IPA bits {default}, max vCPUs {vcpus}");
    assert!(max >= default);
    assert!(vcpus >= 1);
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn hvc_exits_with_immediate_and_registers() {
    let _serial = serial();
    let page = GuestPage::with_code(&[movz_x(0, 42), hvc(5)]);
    let vm = Vm::new(&VmConfig::default()).unwrap();
    page.map(&vm);
    let mut vcpu = vm.create_vcpu().unwrap();
    vcpu.set_reg(Reg::PC, CODE_IPA).unwrap();
    vcpu.set_reg(Reg::CPSR, PSTATE_EL1H_MASKED).unwrap();

    let exit = vcpu.run().unwrap();
    let Exit::Exception(e) = exit else {
        panic!("expected an exception exit, got {exit:?}");
    };
    assert_eq!(e.decode(), Trap::Hvc { imm: 5 });
    assert_eq!(vcpu.reg(x(0)).unwrap(), 42);
    // HVC's preferred return address is the next instruction.
    assert_eq!(vcpu.reg(Reg::PC).unwrap(), CODE_IPA + 8);
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn mmio_store_and_load_are_described() {
    let _serial = serial();
    let page = GuestPage::with_code(&[str_x(1, 2), ldr_w(3, 2), hvc(0)]);
    let vm = Vm::new(&VmConfig::default()).unwrap();
    page.map(&vm);
    let mut vcpu = vm.create_vcpu().unwrap();
    vcpu.set_reg(Reg::PC, CODE_IPA).unwrap();
    vcpu.set_reg(Reg::CPSR, PSTATE_EL1H_MASKED).unwrap();
    vcpu.set_reg(x(1), 0xdead_beef).unwrap();
    vcpu.set_reg(x(2), MMIO_IPA).unwrap();

    // The store traps. Emulate it and step past the instruction.
    let Exit::Exception(e) = vcpu.run().unwrap() else {
        panic!()
    };
    let store = Trap::DataAbort {
        ipa: MMIO_IPA,
        access: Some(DataAccess {
            write: true,
            size: 8,
            reg: 1,
            sign_extend: false,
            wide_reg: true,
        }),
    };
    assert_eq!(e.decode(), store);
    assert_eq!(
        vcpu.reg(Reg::PC).unwrap(),
        CODE_IPA,
        "PC still at the store"
    );
    vcpu.set_reg(Reg::PC, CODE_IPA + 4).unwrap();

    // The load traps. Supply the device's value and step past it.
    let Exit::Exception(e) = vcpu.run().unwrap() else {
        panic!()
    };
    let Trap::DataAbort {
        ipa,
        access: Some(load),
    } = e.decode()
    else {
        panic!("expected a described load, got {:?}", e.decode());
    };
    assert_eq!(ipa, MMIO_IPA);
    assert_eq!(
        (load.write, load.size, load.reg, load.wide_reg),
        (false, 4, 3, false)
    );
    vcpu.set_reg(x(3), 0x1234_5678).unwrap();
    vcpu.set_reg(Reg::PC, CODE_IPA + 8).unwrap();

    let Exit::Exception(e) = vcpu.run().unwrap() else {
        panic!()
    };
    assert_eq!(e.decode(), Trap::Hvc { imm: 0 });
    assert_eq!(vcpu.reg(x(3)).unwrap(), 0x1234_5678);
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn registers_and_timer_round_trip() {
    let _serial = serial();
    let vm = Vm::new(&VmConfig::default()).unwrap();
    let vcpu = vm.create_vcpu().unwrap();

    vcpu.set_reg(x(7), 0x0123_4567_89ab_cdef).unwrap();
    assert_eq!(vcpu.reg(x(7)).unwrap(), 0x0123_4567_89ab_cdef);

    vcpu.set_sys_reg(SysReg::TPIDR_EL1, 0xfeed).unwrap();
    assert_eq!(vcpu.sys_reg(SysReg::TPIDR_EL1).unwrap(), 0xfeed);

    // Exercises the asm shim that passes the vector in v0.
    let q = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeffu128;
    vcpu.set_simd_fp_reg(5, q).unwrap();
    assert_eq!(vcpu.simd_fp_reg(5).unwrap(), q);
    assert_eq!(vcpu.simd_fp_reg(6).unwrap(), 0);
    assert_eq!(vcpu.simd_fp_reg(32), Err(Error::BadArgument));

    vcpu.set_vtimer_offset(0x1000).unwrap();
    assert_eq!(vcpu.vtimer_offset().unwrap(), 0x1000);
    vcpu.set_vtimer_masked(true).unwrap();
    assert!(vcpu.vtimer_masked().unwrap());

    // Every register a snapshot saves can be read back and written.
    for (name, reg) in GUEST_STATE_SYS_REGS {
        let value = vcpu
            .sys_reg(*reg)
            .unwrap_or_else(|e| panic!("read {name}: {e}"));
        vcpu.set_sys_reg(*reg, value)
            .unwrap_or_else(|e| panic!("write {name}: {e}"));
    }
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn ipa_size_can_be_set_to_the_maximum() {
    let _serial = serial();
    let max = cave_vm_hvf::max_ipa_bits().unwrap();
    let vm = Vm::new(&VmConfig {
        ipa_bits: Some(max),
    })
    .unwrap();
    drop(vm);
    let too_big = Vm::new(&VmConfig {
        ipa_bits: Some(max + 1),
    });
    assert!(too_big.is_err(), "IPA size above the maximum was accepted");
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn gic_state_saves_and_restores() {
    let _serial = serial();
    let params = GicParams::query().unwrap();
    println!("{params:?}");
    let align = |v: u64, a: usize| v.next_multiple_of(a as u64);
    let distributor_base = align(0x0800_0000, params.distributor_alignment);
    let redistributor_base = align(
        distributor_base + params.distributor_size as u64,
        params.redistributor_alignment,
    );

    let vm = Vm::new(&VmConfig::default()).unwrap();
    vm.create_gic(&GicConfig {
        distributor_base,
        redistributor_base,
    })
    .unwrap();
    let vcpu = vm.create_vcpu().unwrap();
    // The redistributor is found by the vCPU's affinity, so set it first.
    vcpu.set_sys_reg(SysReg::MPIDR_EL1, 1 << 31).unwrap();
    assert_eq!(vcpu.redistributor_base().unwrap(), redistributor_base);

    let vtimer = GicInterrupt::El1VirtualTimer.intid().unwrap();
    println!("virtual timer intid {vtimer}");
    assert_eq!(vtimer, 27);

    let state = vm.gic_state().unwrap();
    println!("GIC state is {} bytes", state.len());
    assert!(!state.is_empty());
    vm.set_gic_state(&state).unwrap();
    assert_eq!(vm.gic_state().unwrap(), state);

    for (name, reg) in GUEST_STATE_ICC_REGS {
        let value = vcpu
            .icc_reg(*reg)
            .unwrap_or_else(|e| panic!("read ICC_{name}: {e}"));
        vcpu.set_icc_reg(*reg, value)
            .unwrap_or_else(|e| panic!("write ICC_{name}: {e}"));
    }

    // The physical timer registers exist only with a GIC.
    vcpu.sys_reg(SysReg::CNTP_CTL_EL0).unwrap();
    vm.set_spi(params.spi_base, true).unwrap();
}

#[test]
#[ignore = "needs a Mac with the hypervisor entitlement"]
fn second_vm_is_refused() {
    let _serial = serial();
    let _vm = Vm::new(&VmConfig::default()).unwrap();
    assert_eq!(Vm::new(&VmConfig::default()).err(), Some(Error::VmExists));
}
