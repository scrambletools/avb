//! Raw Ethernet for AVB protocols: raw-ether's MAC addresses, interfaces,
//! sockets, frames with hardware time stamps, PTP hardware clocks and
//! captures, under the paths AVB crates use, and whole frames for AVTP
//! media streams. This is the only platform code the protocol crates
//! depend on.
//!
//! Without the `std` feature only the address types remain, so protocol
//! crates built on them also run on microcontrollers.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

pub use raw_ether::{MacAddress, ParseError};

#[cfg(feature = "std")]
pub use raw_ether::{capture, clock, interfaces, socket, stamped};

#[cfg(all(feature = "std", target_os = "linux"))]
pub use raw_ether::broker;

#[cfg(feature = "std")]
pub mod stream;

#[cfg(feature = "std")]
pub use raw_ether::{Blocked, Interface, LinkPower, Received, Socket, check_access};
