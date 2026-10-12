//! The only wrapper around Apple's Hypervisor.framework.
//!
//! This crate is one of the two places `unsafe` is allowed (the other is
//! the `memory` module in `vmm`). Every `unsafe` block needs a `// SAFETY:`
//! comment, which clippy enforces below.
//!
//! The framework only exists on Apple Silicon Macs, so everything that calls
//! it is compiled only there. Error codes, register tables and exit decoding
//! are pure logic and build and test on any host.
//!
//! A process can have one VM ([`Vm`]). Each [`Vcpu`] belongs to the thread
//! that created it. Running a VM needs the `com.apple.security.hypervisor`
//! entitlement; see `scripts/hypervisor-runner.sh` for running tests with it.

#![deny(clippy::undocumented_unsafe_blocks)]

mod error;
mod exit;
mod regs;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod ffi;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod time;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vcpu;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vm;

pub use error::{Error, RawReturn, Result};
pub use exit::{DataAccess, Exception, Exit, Trap};
pub use regs::{
    GUEST_STATE_ICC_REGS, GUEST_STATE_SYS_REGS, IccReg, RedistReg, Reg, SIMD_FP_REGS, SysReg,
};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use time::{host_ticks, ticks_to_duration};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use vcpu::{InterruptKind, Vcpu, VcpuHandle};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use vm::{
    GicConfig, GicInterrupt, GicParams, MemFlags, Vm, VmConfig, default_ipa_bits, max_ipa_bits,
    max_vcpus,
};
