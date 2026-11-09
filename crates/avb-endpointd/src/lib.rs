//! This computer's AVB entities, for any app: one daemon per interface,
//! `avb-endpointd@<interface>`, runs every entity on the computer, each an
//! ATDECC entity with streams any controller can connect, its audio from
//! and to a PipeWire device apps play into or record from.
//!
//! Without features the crate is what apps share with the daemon, on
//! every platform: the entities to run ([`config`]), what the daemon is
//! doing ([`status`]), how loud its streams are ([`meters`]), where those
//! files are ([`store`]), and how an entity's ID is made
//! ([`entity_id`]). The `daemon` feature adds the daemon, for Linux.

use atdecc::EntityId;
use avb_net::MacAddress;

pub mod config;
pub mod meters;
pub mod status;
pub mod store;

#[cfg(feature = "daemon")]
pub mod gptp;
#[cfg(feature = "daemon")]
pub mod lite;
#[cfg(feature = "daemon")]
pub mod port;
#[cfg(feature = "daemon")]
pub mod profile;
#[cfg(feature = "daemon")]
pub mod runtime;
#[cfg(feature = "daemon")]
pub mod sharing;
#[cfg(feature = "daemon")]
pub mod signal;

/// The entity models of the daemon's entities, under Scramble Tools' MA-S
/// `8C-1F-64-36-C`: this, and a hash of their streams. triib 1.0's
/// talkers and listeners had `0x8c1f_6436_c000_0002` and `_0004`.
pub const DEVICE_MODELS: u64 = 0x8c1f_6436_c001_0000;

/// An entity ID from the interface's address and the entity's instance,
/// apart from triib's controller (FF-FE), triib-cli's (FF-FD) and plaa's
/// (FF-FC).
pub fn entity_id(mac: MacAddress, instance: u8) -> EntityId {
    let [a, b, c, d, e, f] = mac.0;
    EntityId(u64::from_be_bytes([a, b, c, 0xff, instance, d, e, f]))
}

/// Whether an entity model is one of the daemon's entities', telling them
/// apart before the status names them: the block's high bits and the bit
/// their hash always carries, which the apps' controllers lack.
pub fn is_device_model(model: atdecc::EntityModelId) -> bool {
    model.0 & 0xffff_ffff_ff01_0000 == DEVICE_MODELS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_id_takes_the_address_and_the_instance() {
        let mac = MacAddress([0xf0, 0xa7, 0x31, 0xf4, 0x0f, 0x14]);
        assert_eq!(entity_id(mac, 0), EntityId(0xf0a7_31ff_00f4_0f14));
        assert_eq!(entity_id(mac, 0x70), EntityId(0xf0a7_31ff_70f4_0f14));
        assert!(is_device_model(atdecc::EntityModelId(
            DEVICE_MODELS | 0x01_2345
        )));
        assert!(!is_device_model(atdecc::EntityModelId(
            0x8c1f_6436_c020_0005
        )));
    }
}
