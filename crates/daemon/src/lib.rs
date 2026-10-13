//! The daemon: supervises one VMM process per VM and owns policy, the audit
//! log, sync and the shared image cache.

#![deny(unsafe_code)]

pub mod image;
