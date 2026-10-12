//! Guest memory.
//!
//! The only module in `vmm` allowed to use `unsafe` (ROADMAP.md D5). Every
//! `unsafe` block needs a `// SAFETY:` comment.

#![allow(unsafe_code)]
