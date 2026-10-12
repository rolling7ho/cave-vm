//! The per-VM process: device models and snapshots.

#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod backend;
pub mod memory;
