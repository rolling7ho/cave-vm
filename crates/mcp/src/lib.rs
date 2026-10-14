//! The MCP server over the daemon's socket API (decision 0013): `cave mcp`
//! runs it on stdin and stdout for the project in the current folder.
//!
//! Messages are JSON-RPC 2.0, one per line (the MCP stdio transport). The
//! server answers `initialize`, `ping`, `tools/list` and `tools/call`, one
//! request at a time. The tools ([`tools`]) run through the Rust SDK and
//! resolve the VM from the project folder at each call, so a project set
//! up after the server started is found.

#![deny(unsafe_code)]

mod rpc;
pub mod tools;

pub use rpc::{PROTOCOL_VERSIONS, serve};
pub use tools::Tools;
