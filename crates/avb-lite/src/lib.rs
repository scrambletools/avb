//! The AVB Lite profile: AVB's streams between endpoints with no AVB
//! bridge between them, as on a switch that knows nothing of AVB.
//!
//! - [`cvu`]: CVU SRP's messages, MSRP declarations carried as ATDECC
//!   vendor unique commands (profile 6).
//! - [`participant`]: CVU SRP's participant for a port, declaring the
//!   talkers and listeners here and keeping what the others declare,
//!   which admits each talker's listeners with [`admission`].
//!
//! Whether a port falls back to AVB Lite (profile 2.2) is PTP's to say:
//! gptp's `lite` module weighs it, and gptpd runs it.
//!
//! Like `atdecc` and `avb-mrp`, it does no I/O, keeps no clock and starts
//! no threads: the caller hands it what it heard and the time, and takes
//! back what to send and the next deadline. AVB Lite times its streams by
//! gPTP time as AVB does, so it is for interfaces with a PTP hardware
//! clock; the caller sees to that.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod admission;
pub mod cvu;
pub mod participant;

pub use participant::{Event, Participant};
