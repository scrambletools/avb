//! MSRP's attribute propagation (IEEE 802.1Q-2022, 35.2.4): from what
//! peers declare on each port, what the bridge declares on each, which
//! streams each port forwards and what each SR class reserves there.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::time::Duration;

use super::{Application, Config, Declarations, Forwarded, Port, PortId, SrClass, Talker, key};
use crate::MacAddress;
use crate::msrp::{self, ListenerState, TalkerDeclaration, TalkerFailure};

/// Failure codes (Table 46-15) the bridge declares.
pub(super) mod failure {
    pub const INSUFFICIENT_BANDWIDTH: u8 = 1;
    pub const STREAM_ID_IN_USE: u8 = 4;
    pub const NOT_AVB_CAPABLE: u8 = 8;
    pub const NOT_SR_CLASS_PRIORITY: u8 = 13;
    pub const FRAME_TOO_LARGE: u8 = 14;
}

/// Octets each frame takes on Ethernet beyond its payload: preamble and
/// start of frame, header with a VLAN tag, frame check and interframe
/// gap (34.4), and one for the class measurement intervals of neighbors
/// differing (35.2.4.2).
const FRAME_OVERHEAD: u64 = 8 + 18 + 4 + 12 + 1;

/// A stream a port should forward, and what it reserves there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Wanted {
    pub destination: MacAddress,
    pub vlan_id: u16,
    pub class_id: u8,
    pub bandwidth: u64,
}

/// What the bridge should declare and forward.
#[derive(Default)]
pub(super) struct Plan {
    pub declarations: BTreeMap<PortId, Declarations>,
    pub reserved: BTreeMap<(PortId, u64), Wanted>,
    pub idle_slopes: BTreeMap<(PortId, u8), u64>,
    pub boundaries: BTreeSet<(PortId, u8)>,
}

/// Works out the plan from what peers declare on the enabled ports, with
/// the streams `forwarded` now keeping their place as the oldest.
pub(super) fn plan(
    config: &Config,
    ports: &BTreeMap<PortId, Port>,
    forwarded: &BTreeMap<(PortId, u64), Forwarded>,
) -> Plan {
    let enabled: Vec<(PortId, &Port)> = ports
        .iter()
        .filter(|(_, port)| port.msrp.is_some())
        .map(|(id, port)| (*id, port))
        .collect();
    let mut plan = Plan::default();
    // Every port declares the SR classes it supports, whatever others
    // register, and is a boundary where its neighbor differs (35.2.4).
    for (port_id, port) in &enabled {
        let declarations = plan.declarations.entry(*port_id).or_default();
        for class in &config.classes {
            let value = class.domain.value();
            let name = key(Application::Msrp, msrp::attribute::DOMAIN, &value);
            declarations.insert((msrp::attribute::DOMAIN, name), (value, None));
            if boundary(port, class) {
                plan.boundaries.insert((*port_id, class.domain.class_id));
            }
        }
    }
    let talkers = talkers(&enabled);
    let admitted = admit(config, &enabled, &talkers, &plan.boundaries, forwarded);
    for (stream_id, (talker_port, talker)) in &talkers {
        let registered = &talker.declaration;
        let class = class_of(config, registered.priority);
        let name = stream_id.to_be_bytes().to_vec();
        let mut listeners: Option<ListenerState> = None;
        for (port_id, port) in &enabled {
            if port_id == talker_port {
                continue;
            }
            let code = if port.talks(*stream_id) {
                Some(failure::STREAM_ID_IN_USE)
            } else {
                match class {
                    None => Some(failure::NOT_SR_CLASS_PRIORITY),
                    Some(_) if registered.max_frame_size > config.max_frame_size => {
                        Some(failure::FRAME_TOO_LARGE)
                    }
                    Some(class) if plan.boundaries.contains(&(*port_id, class.domain.class_id)) => {
                        Some(failure::NOT_AVB_CAPABLE)
                    }
                    Some(_)
                        if port
                            .listeners
                            .get(stream_id)
                            .is_some_and(|state| state.ready())
                            && !admitted.contains_key(&(*port_id, *stream_id)) =>
                    {
                        Some(failure::INSUFFICIENT_BANDWIDTH)
                    }
                    Some(_) => None,
                }
            };
            let mut declared = *registered;
            declared.accumulated_latency = registered
                .accumulated_latency
                .saturating_add(port.status.latency);
            // A talker that failed before this bridge keeps its failure.
            declared.failure = registered.failure.or(code.map(|code| TalkerFailure {
                bridge_id: config.bridge_id,
                code,
            }));
            let attribute_type = if declared.failure.is_some() {
                msrp::attribute::TALKER_FAILED
            } else {
                msrp::attribute::TALKER_ADVERTISE
            };
            plan.declarations.entry(*port_id).or_default().insert(
                (attribute_type, name.clone()),
                (msrp::talker_value(&declared), None),
            );
            // Its listeners, as the talker it hears stands (Table 35-12),
            // and forwarding where they are ready for it (Table 35-13).
            let Some(state) = port
                .listeners
                .get(stream_id)
                .filter(|state| **state != ListenerState::Ignore)
            else {
                continue;
            };
            let passed = if declared.failure.is_some() {
                ListenerState::AskingFailed
            } else {
                *state
            };
            listeners = Some(merge(listeners, passed));
            if declared.failure.is_none()
                && state.ready()
                && let Some((class_id, bandwidth)) = admitted.get(&(*port_id, *stream_id))
            {
                plan.reserved.insert(
                    (*port_id, *stream_id),
                    Wanted {
                        destination: registered.destination,
                        vlan_id: registered.vlan_id,
                        class_id: *class_id,
                        bandwidth: *bandwidth,
                    },
                );
                *plan.idle_slopes.entry((*port_id, *class_id)).or_default() += bandwidth;
            }
        }
        // The listeners merged into one declaration toward the talker
        // (35.2.4.4.3).
        if let Some(state) = listeners {
            plan.declarations.entry(*talker_port).or_default().insert(
                (msrp::attribute::LISTENER, name.clone()),
                (name, Some(state.number())),
            );
        }
    }
    plan
}

/// Whether `port` is an SRP domain boundary for `class` (35.2.1.4 h):
/// its neighbor is not AVB capable, declares no domain for the class or
/// declares it with another priority.
fn boundary(port: &Port, class: &SrClass) -> bool {
    !port.status.as_capable
        || port
            .domains
            .get(&class.domain.class_id)
            .is_none_or(|domain| domain.priority != class.domain.priority)
}

/// Each stream's talker: of the ports a peer declares it on as its own,
/// the one it was declared on first, then the lowest.
fn talkers<'a>(enabled: &[(PortId, &'a Port)]) -> BTreeMap<u64, (PortId, &'a Talker)> {
    let mut talkers: BTreeMap<u64, (PortId, &'a Talker)> = BTreeMap::new();
    for (port_id, port) in enabled {
        for (stream_id, talker) in &port.talkers {
            if talker.echo {
                continue;
            }
            let first = talkers
                .get(stream_id)
                .is_none_or(|(_, before)| talker.since < before.since);
            if first {
                talkers.insert(*stream_id, (*port_id, talker));
            }
        }
    }
    talkers
}

/// The SR class whose priority `priority` is.
fn class_of(config: &Config, priority: u8) -> Option<&SrClass> {
    config
        .classes
        .iter()
        .find(|class| class.domain.priority == priority)
}

/// The bandwidth a stream takes on Ethernet, in bits a second (34.4,
/// 35.2.4.2).
fn bandwidth(declaration: &TalkerDeclaration, class: &SrClass) -> u64 {
    (u64::from(declaration.max_frame_size) + FRAME_OVERHEAD)
        * 8
        * u64::from(declaration.max_interval_frames)
        * u64::from(class.intervals_per_second)
}

/// A stream asking for bandwidth on a port.
struct Candidate {
    stream_id: u64,
    class_index: usize,
    class_id: u8,
    bandwidth: u64,
    emergency: bool,
    /// Since when the port forwards it, where it does.
    since: Option<Duration>,
}

/// The streams each port admits, with their SR class and bandwidth: of
/// those its listeners are ready for and that could pass, the most
/// important first (35.2.4.1), each while its class and those below it
/// stay within their deltaBandwidth (34.3.1).
fn admit(
    config: &Config,
    enabled: &[(PortId, &Port)],
    talkers: &BTreeMap<u64, (PortId, &Talker)>,
    boundaries: &BTreeSet<(PortId, u8)>,
    forwarded: &BTreeMap<(PortId, u64), Forwarded>,
) -> BTreeMap<(PortId, u64), (u8, u64)> {
    let mut admitted = BTreeMap::new();
    for (port_id, port) in enabled {
        let mut candidates: Vec<Candidate> = Vec::new();
        for (stream_id, state) in &port.listeners {
            let Some((talker_port, talker)) = talkers.get(stream_id) else {
                continue;
            };
            let declaration = &talker.declaration;
            let Some(class_index) = config
                .classes
                .iter()
                .position(|class| class.domain.priority == declaration.priority)
            else {
                continue;
            };
            let class = &config.classes[class_index];
            if !state.ready()
                || talker_port == port_id
                || port.talks(*stream_id)
                || declaration.failure.is_some()
                || declaration.max_frame_size > config.max_frame_size
                || boundaries.contains(&(*port_id, class.domain.class_id))
            {
                continue;
            }
            candidates.push(Candidate {
                stream_id: *stream_id,
                class_index,
                class_id: class.domain.class_id,
                bandwidth: bandwidth(declaration, class),
                emergency: !declaration.rank,
                since: forwarded
                    .get(&(*port_id, *stream_id))
                    .map(|forwarded| forwarded.since),
            });
        }
        candidates.sort_by(|one, other| {
            other
                .emergency
                .cmp(&one.emergency)
                .then_with(|| older(one.since, other.since))
                .then_with(|| one.stream_id.cmp(&other.stream_id))
        });
        let link = u64::from(port.status.link_speed) * 1_000_000;
        // What each class and those above it may reserve together.
        let mut allowed = 0u64;
        let limits: Vec<u64> = config
            .classes
            .iter()
            .map(|class| {
                allowed += u64::from(class.delta_bandwidth);
                link * allowed / 100
            })
            .collect();
        let mut reserved = alloc::vec![0u64; config.classes.len()];
        for candidate in candidates {
            let fits = (candidate.class_index..config.classes.len()).all(|below| {
                let together: u64 = reserved[..=below].iter().sum();
                together + candidate.bandwidth <= limits[below]
            });
            if fits {
                reserved[candidate.class_index] += candidate.bandwidth;
                admitted.insert(
                    (*port_id, candidate.stream_id),
                    (candidate.class_id, candidate.bandwidth),
                );
            }
        }
    }
    admitted
}

/// Orders streams forwarded longer first, then those not forwarded yet.
fn older(one: Option<Duration>, other: Option<Duration>) -> Ordering {
    match (one, other) {
        (Some(one), Some(other)) => one.cmp(&other),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Two listener declaration types merged into one (Table 35-15).
fn merge(merged: Option<ListenerState>, next: ListenerState) -> ListenerState {
    use ListenerState::*;
    match (merged, next) {
        (None, state) | (Some(Ignore), state) | (Some(state), Ignore) => state,
        (Some(Ready), Ready) => Ready,
        (Some(AskingFailed), AskingFailed) => AskingFailed,
        _ => ReadyFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ListenerState::*;

    #[test]
    fn listener_declarations_merge_as_the_table_says() {
        for (first, second, merged) in [
            (Ready, Ready, Ready),
            (Ready, ReadyFailed, ReadyFailed),
            (Ready, AskingFailed, ReadyFailed),
            (ReadyFailed, Ready, ReadyFailed),
            (ReadyFailed, AskingFailed, ReadyFailed),
            (AskingFailed, Ready, ReadyFailed),
            (AskingFailed, ReadyFailed, ReadyFailed),
            (AskingFailed, AskingFailed, AskingFailed),
        ] {
            assert_eq!(merge(Some(first), second), merged, "{first:?} {second:?}");
        }
        assert_eq!(merge(None, AskingFailed), AskingFailed);
    }
}
