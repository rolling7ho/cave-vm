//! Raw declarations from the macOS SDK's Hypervisor.framework headers.
//!
//! Hand-written rather than generated, to avoid a build dependency. Only the
//! calls the wrapper uses are declared. Types follow the arm64 branch of the
//! headers.

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub type hv_return_t = i32;
pub type hv_vcpu_t = u64;
pub type hv_ipa_t = u64;
pub type hv_memory_flags_t = u64;
pub type hv_vm_config_t = *mut c_void;
pub type hv_vcpu_config_t = *mut c_void;
pub type hv_gic_config_t = *mut c_void;
pub type hv_gic_state_t = *mut c_void;

/// `hv_simd_fp_uchar16_t`: 16 bytes with the vector type's alignment.
#[repr(C, align(16))]
pub struct hv_simd_fp_uchar16_t(pub [u8; 16]);

#[repr(C)]
pub struct hv_vcpu_exit_exception_t {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: hv_ipa_t,
}

#[repr(C)]
pub struct hv_vcpu_exit_t {
    pub reason: u32,
    pub exception: hv_vcpu_exit_exception_t,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    // hv_vm_config.h
    pub fn hv_vm_config_create() -> hv_vm_config_t;
    pub fn hv_vm_config_get_max_ipa_size(ipa_bit_length: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_get_default_ipa_size(ipa_bit_length: *mut u32) -> hv_return_t;
    pub fn hv_vm_config_set_ipa_size(config: hv_vm_config_t, ipa_bit_length: u32) -> hv_return_t;

    // hv_vm.h
    pub fn hv_vm_get_max_vcpu_count(max_vcpu_count: *mut u32) -> hv_return_t;
    pub fn hv_vm_create(config: hv_vm_config_t) -> hv_return_t;
    pub fn hv_vm_destroy() -> hv_return_t;
    pub fn hv_vm_map(
        addr: *mut c_void,
        ipa: hv_ipa_t,
        size: usize,
        flags: hv_memory_flags_t,
    ) -> hv_return_t;
    pub fn hv_vm_unmap(ipa: hv_ipa_t, size: usize) -> hv_return_t;
    pub fn hv_vm_protect(ipa: hv_ipa_t, size: usize, flags: hv_memory_flags_t) -> hv_return_t;

    // hv_vcpu.h
    pub fn hv_vcpu_create(
        vcpu: *mut hv_vcpu_t,
        exit: *mut *const hv_vcpu_exit_t,
        config: hv_vcpu_config_t,
    ) -> hv_return_t;
    pub fn hv_vcpu_destroy(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpu_get_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_simd_fp_reg(
        vcpu: hv_vcpu_t,
        reg: u32,
        value: *mut hv_simd_fp_uchar16_t,
    ) -> hv_return_t;
    /// Takes `(hv_vcpu_t, hv_simd_fp_reg_t, hv_simd_fp_uchar16_t)` with the
    /// vector passed by value in `v0`. Rust cannot declare a SIMD argument in
    /// FFI on stable, so this is declared without arguments and called
    /// through [`crate::vcpu`]'s `asm!` shim.
    pub fn hv_vcpu_set_simd_fp_reg();
    pub fn hv_vcpu_get_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_sys_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;
    pub fn hv_vcpu_get_pending_interrupt(
        vcpu: hv_vcpu_t,
        kind: u32,
        pending: *mut bool,
    ) -> hv_return_t;
    pub fn hv_vcpu_set_pending_interrupt(vcpu: hv_vcpu_t, kind: u32, pending: bool) -> hv_return_t;
    pub fn hv_vcpu_run(vcpu: hv_vcpu_t) -> hv_return_t;
    pub fn hv_vcpus_exit(vcpus: *const hv_vcpu_t, vcpu_count: u32) -> hv_return_t;
    pub fn hv_vcpu_get_exec_time(vcpu: hv_vcpu_t, time: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_mask(vcpu: hv_vcpu_t, masked: *mut bool) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: hv_vcpu_t, masked: bool) -> hv_return_t;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: hv_vcpu_t, offset: *mut u64) -> hv_return_t;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: hv_vcpu_t, offset: u64) -> hv_return_t;

    // hv_gic_config.h, hv_gic_parameters.h, hv_gic.h, hv_gic_state.h (macOS 15+)
    pub fn hv_gic_config_create() -> hv_gic_config_t;
    pub fn hv_gic_config_set_distributor_base(
        config: hv_gic_config_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub fn hv_gic_config_set_redistributor_base(
        config: hv_gic_config_t,
        base: hv_ipa_t,
    ) -> hv_return_t;
    pub fn hv_gic_get_distributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_distributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_size(size: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base_alignment(alignment: *mut usize) -> hv_return_t;
    pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> hv_return_t;
    pub fn hv_gic_get_intid(interrupt: u16, intid: *mut u32) -> hv_return_t;
    pub fn hv_gic_create(config: hv_gic_config_t) -> hv_return_t;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
    pub fn hv_gic_get_redistributor_base(vcpu: hv_vcpu_t, base: *mut hv_ipa_t) -> hv_return_t;
    pub fn hv_gic_get_redistributor_reg(vcpu: hv_vcpu_t, reg: u32, value: *mut u64) -> hv_return_t;
    pub fn hv_gic_set_redistributor_reg(vcpu: hv_vcpu_t, reg: u32, value: u64) -> hv_return_t;
    pub fn hv_gic_get_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: *mut u64) -> hv_return_t;
    pub fn hv_gic_set_icc_reg(vcpu: hv_vcpu_t, reg: u16, value: u64) -> hv_return_t;
    pub fn hv_gic_set_state(data: *const c_void, size: usize) -> hv_return_t;
    pub fn hv_gic_state_create() -> hv_gic_state_t;
    pub fn hv_gic_state_get_size(state: hv_gic_state_t, size: *mut usize) -> hv_return_t;
    pub fn hv_gic_state_get_data(state: hv_gic_state_t, data: *mut c_void) -> hv_return_t;
}

/// `mach_timebase_info_data_t` from <mach/mach_time.h>.
#[repr(C)]
pub struct mach_timebase_info {
    pub numer: u32,
    pub denom: u32,
}

unsafe extern "C" {
    /// Host monotonic clock in ticks. The guest's virtual counter is this
    /// minus the vCPU's vtimer offset.
    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut mach_timebase_info) -> i32;

    /// Releases an OS object such as `hv_vm_config_t` (libSystem, os/object.h).
    pub fn os_release(object: *mut c_void);
}
