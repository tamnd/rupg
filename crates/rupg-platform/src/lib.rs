//! The traits `Io`, `Clock`, `Net`, `Tasks` and `Entropy`, their operating system and simulation implementations, memory accounting (`MemoryPool`), reservations of address space (`Region`), CPU feature detection.
//!
//! The engine reaches the outside world only through these traits (spec/21 section 21.10). The operating system implementations are in [`os`]. The simulation implementations are in [`sim`]. They are deterministic for a given seed, and the simulated disk can build the file that a power cut leaves (spec/21 section 21.9.1).
//!
//! This crate may contain `unsafe`. Each `unsafe` block must have a `// SAFETY:` comment that states the invariant. `cargo xtask style` checks this.
//!
//! This crate first ships in milestone M1. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![deny(unsafe_op_in_unsafe_fn)]

mod clock;
mod cpu;
mod entropy;
mod io;
mod memory;
mod net;
pub mod os;
mod region;
pub mod sim;
mod tasks;

pub use clock::Clock;
pub use cpu::{CpuFeatures, CpuLevel, physical_memory};
pub use entropy::Entropy;
pub use io::{File, FileMode, Io, OpenMode};
pub use memory::{MemoryPool, Reclaimer, Reservation};
pub use net::{Listener, Net, Stream};
pub use region::Region;
pub use tasks::{Join, Task, TaskHandle, Tasks};
