//! MVRP's attribute propagation (IEEE 802.1Q-2022, 10.3, 11.2): each
//! port declares the VLANs registered on the bridge's other ports, and
//! those every port is a member of.

use alloc::collections::{BTreeMap, BTreeSet};

use super::{Application, Config, Declarations, Port, PortId, key};
use crate::mvrp;

/// What each enabled port should declare.
pub(super) fn plan(
    config: &Config,
    ports: &BTreeMap<PortId, Port>,
) -> BTreeMap<PortId, Declarations> {
    let mut plan = BTreeMap::new();
    if !config.mvrp {
        return plan;
    }
    for (port_id, port) in ports {
        if port.mvrp.is_none() {
            continue;
        }
        let mut vlans: BTreeSet<u16> = config.static_vlans.iter().copied().collect();
        for (other_id, other) in ports {
            if other_id != port_id && other.mvrp.is_some() {
                vlans.extend(other.vlans.iter().copied());
            }
        }
        let declarations: &mut Declarations = plan.entry(*port_id).or_default();
        for vlan_id in vlans {
            let value = mvrp::value(vlan_id);
            let name = key(Application::Mvrp, mvrp::VID, &value);
            declarations.insert((mvrp::VID, name), (value, None));
        }
    }
    plan
}
