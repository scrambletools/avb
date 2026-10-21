//! The AVB Lite profile: AVB's streams between endpoints with no AVB
//! bridge between them, as on a switch that knows nothing of AVB.
//!
//! - [`fallback`]: whether a bridge reserves streams, from the Pdelay
//!   exchange, and falling back to AVB Lite when none does (profile 2.2).
//! - [`cvu`]: CVU SRP's messages, MSRP declarations carried as ATDECC
//!   vendor unique commands (profile 6).
//! - [`participant`]: CVU SRP's participant for a port, declaring the
//!   talkers and listeners here and keeping what the others declare,
//!   which admits each talker's listeners with [`admission`].
//!
//! Like `atdecc` and `avb-mrp`, it does no I/O, keeps no clock and starts
//! no threads: the caller hands it what it heard and the time, and takes
//! back what to send and the next deadline. AVB Lite times its streams by
//! the PTP hardware clock as AVB does, so it is for interfaces that have
//! one; the caller sees to that.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod admission;
pub mod cvu;
pub mod fallback;
pub mod participant;

pub use participant::{Event, Participant};
