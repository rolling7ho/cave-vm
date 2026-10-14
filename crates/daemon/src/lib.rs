//! The daemon: supervises one VMM process per VM and owns policy, the audit
//! log, sync and the shared image cache.

#![deny(unsafe_code)]

pub mod audit;
pub mod config;
pub mod credentials;
pub mod doctor;
pub mod forward;
pub mod gc;
pub mod home;
pub mod image;
pub mod launch;
pub mod link;
pub mod progress;
pub mod project;
pub mod relay;
pub mod server;
pub mod snapshot;
pub mod sync;
pub mod updates;
pub mod vms;
