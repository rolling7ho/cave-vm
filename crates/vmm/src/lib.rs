//! The per-VM process: device models and snapshots.

#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod backend;
#[cfg(target_os = "macos")]
pub mod boot;
#[cfg(target_os = "macos")]
pub mod devices;
pub mod fdt;
pub mod kernel;
pub mod layout;
pub mod memory;
pub mod psci;
#[cfg(target_os = "macos")]
pub mod runner;
