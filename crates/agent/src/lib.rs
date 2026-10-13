//! The guest agent: serves the agent protocol (`docs/protocol.md`) on each
//! connection the host opens over vsock.
//!
//! [`session::serve`] runs one connection: the handshake, then one
//! operation. The operations are plain std code over a [`Stream`], so they
//! are tested on the host over Unix socket pairs; only [`sys`] is
//! platform-specific.

#![deny(unsafe_code)]

pub mod control;
pub mod exec;
pub mod files;
pub mod forward;
pub mod pty;
pub mod session;
pub mod stream;
pub mod sys;
pub mod user;

pub use session::{Config, serve};
pub use stream::Stream;
pub use user::User;
