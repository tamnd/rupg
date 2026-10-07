//! The WebAssembly module and its storage on the OPFS.
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M9. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]
