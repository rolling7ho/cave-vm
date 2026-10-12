//! The per-VM process: device models and snapshots.

#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod backend;
#[cfg(target_os = "macos")]
pub mod boot;
#[cfg(target_os = "macos")]
pub mod devices;
#[cfg(target_os = "macos")]
pub mod disk;
pub mod fdt;
#[cfg(target_os = "macos")]
pub mod host;
pub mod kernel;
pub mod layout;
pub mod memory;
pub mod psci;
#[cfg(target_os = "macos")]
pub mod runner;
#[cfg(target_os = "macos")]
pub mod snapshot;
