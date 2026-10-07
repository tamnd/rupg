//! The traits `Io`, `Clock`, `Net`, `Tasks` and `Entropy`, their operating system and simulation implementations, memory accounting (`MemoryPool`), CPU feature detection.
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]
