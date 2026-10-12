//! The only wrapper around Apple's Hypervisor.framework.
//!
//! This crate is one of the two places `unsafe` is allowed (the other is
//! the `memory` module in `vmm`). Every `unsafe` block needs a `// SAFETY:`
//! comment, which clippy enforces below.

#![deny(clippy::undocumented_unsafe_blocks)]
