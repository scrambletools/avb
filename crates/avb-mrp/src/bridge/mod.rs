//! A bridge's MRP applications (IEEE 802.1Q-2022): MSRP, propagating
//! talker and listener attributes from port to port and admitting each
//! stream on each port's bandwidth (35.2.4), and MVRP, propagating VLAN
//! registrations (10.3, 11.2).
//!
//! Like [`Participant`], it does no I/O and keeps no clock: the caller
//! hands it each port's received MRPDUs, how each port stands and the
//! time, and takes back MRPDUs to send, the changes to make to its
//! forwarding and its queues' idle slopes, and the next deadline. A
//! program's own endpoint can sit on the bridge as one more port, its
//! participant and the bridge's handing MRPDUs to each other in memory.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::time::Duration;

use crate::mrpdu::{Event, Format};
use crate::msrp::{self, Domain, ListenerState, TalkerDeclaration};
use crate::participant::{Participant, Registration};
use crate::{MacAddress, mvrp};

mod streams;
#[cfg(test)]
mod tests;
mod vlans;

/// A bridge port, numbered as the caller likes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortId(pub u32);

/// The MRP applications a bridge runs on each port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Msrp,
    Mvrp,
}

impl Application {
    /// The Ethertype of its MRPDUs.
    pub fn ethertype(self) -> u16 {
        match self {
            Application::Msrp => crate::ETHERTYPE_MSRP,
            Application::Mvrp => crate::ETHERTYPE_MVRP,
        }
    }

    /// Where its MRPDUs go.
    pub fn destination(self) -> MacAddress {
        match self {
            Application::Msrp => crate::MSRP_DESTINATION,
            Application::Mvrp => crate::MVRP_DESTINATION,
        }
    }

    fn format(self) -> Format {
        match self {
            Application::Msrp => msrp::FORMAT,
            Application::Mvrp => mvrp::FORMAT,
        }
    }
}

/// An SR class the bridge supports (34.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SrClass {
    /// Its class ID, priority and SR_PVID, as each port declares them.
    pub domain: Domain,
    /// Class measurement intervals a second (34.3).
    pub intervals_per_second: u32,
    /// deltaBandwidth (34.3.1): the percentage of a link it may reserve
    /// beyond what the classes above it may.
    pub delta_bandwidth: u8,
}

impl SrClass {
    /// Class A, which may reserve 75% of a link.
    pub const A: SrClass = SrClass {
        domain: Domain::CLASS_A,
        intervals_per_second: 8000,
        delta_bandwidth: 75,
    };

    /// Class B, which may take what class A leaves of those 75%.
    pub const B: SrClass = SrClass {
        domain: Domain::CLASS_B,
        intervals_per_second: 4000,
        delta_bandwidth: 0,
    };
}

/// How a bridge runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The Bridge Identifier (13.26.2) its Talker Failed declarations
    /// name.
    pub bridge_id: u64,
    /// The SR classes it supports, highest priority first.
    pub classes: Vec<SrClass>,
    /// The largest MaxFrameSize its media carry; a talker above it fails
    /// with code 14.
    pub max_frame_size: u16,
    /// Whether it runs MVRP.
    pub mvrp: bool,
    /// VLANs every port is a member of, as if registered on each: MVRP
    /// declares them on every port.
    pub static_vlans: Vec<u16>,
}

impl Config {
    /// Classes A and B, Ethernet's largest frame, and MVRP with the
    /// default SR_PVID, 2, on every port.
    pub fn new(bridge_id: u64) -> Self {
        Config {
            bridge_id,
            classes: Vec::from([SrClass::A, SrClass::B]),
            max_frame_size: 1500,
            mvrp: true,
            static_vlans: Vec::from([Domain::CLASS_A.vlan_id]),
        }
    }
}

/// msrpLatencyMaxFrameSize (35.2.1.4), in octets.
const LATENCY_MAX_FRAME_SIZE: u32 = 2000;
/// Wire propagation time where none is known (35.2.2.8.6), in ns.
const WIRE_LATENCY: u32 = 500;
/// How long a port that has just come up waits, before it declares
/// talkers, for its neighbor's Domain declarations and for gPTP to count
/// the link asCapable: without either the port is a domain boundary, and
/// a Talker Failed sent in the moments before they come would stop a
/// stream the neighbor's side is listening to. The LeaveAll the port
/// starts with has the Domains come within the neighbor's JoinTime; gPTP
/// takes a few Pdelay exchanges, a second apart.
const SETTLE: Duration = Duration::from_secs(6);

/// How a port stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PortStatus {
    /// Its link is up and it forwards.
    pub enabled: bool,
    /// The link's speed in Mb/s, 0 where not known: no stream is admitted
    /// on it then.
    pub link_speed: u32,
    /// gPTP's asCapable on the link; without it the port is an SRP domain
    /// boundary.
    pub as_capable: bool,
    /// portTcMaxLatency (35.2.1.4) in nanoseconds: the most a frame of an
    /// SR class takes through this port to the next device.
    pub latency: u32,
}

impl PortStatus {
    /// A port whose link is up at `link_speed` Mb/s. Its latency counts
    /// one interfering frame of msrpLatencyMaxFrameSize and the wire
    /// (35.2.2.8.6); add the bridge's own time through it.
    pub fn up(link_speed: u32, as_capable: bool) -> Self {
        let interfering = (LATENCY_MAX_FRAME_SIZE * 8 * 1000)
            .checked_div(link_speed)
            .unwrap_or(0);
        PortStatus {
            enabled: true,
            link_speed,
            as_capable,
            latency: interfering + WIRE_LATENCY,
        }
    }

    /// A port whose link is down.
    pub fn down() -> Self {
        PortStatus {
            enabled: false,
            link_speed: 0,
            as_capable: false,
            latency: 0,
        }
    }
}

/// A change to make to the bridge's forwarding or queues, in the order
/// to make it: a stream stops going out a port before its bandwidth is
/// taken off the port's queue, and the queue gets it before the stream
/// starts (35.2.4.4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// A stream's frames start or stop going out a port: its Dynamic
    /// Reservation Entry (8.8.7) forwards or filters.
    Forwarding {
        port: PortId,
        stream_id: u64,
        destination: MacAddress,
        vlan_id: u16,
        forwarding: bool,
    },
    /// What an SR class reserves on a port, its operIdleSlope (34.3), in
    /// bits a second.
    IdleSlope {
        port: PortId,
        class_id: u8,
        bits_per_second: u64,
    },
    /// A port becomes an SRP domain boundary for an SR class, or stops
    /// being one.
    Boundary {
        port: PortId,
        class_id: u8,
        boundary: bool,
    },
    /// A VLAN registers on a port through MVRP, or leaves it.
    Vlan {
        port: PortId,
        vlan_id: u16,
        registered: bool,
    },
}

/// An MRPDU to send out a port, from its ProtocolVersion on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Transmit {
    pub port: PortId,
    pub application: Application,
    pub pdu: Vec<u8>,
}

/// A stream forwarded out a port, with the bandwidth it reserves there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reservation {
    pub port: PortId,
    pub stream_id: u64,
    pub destination: MacAddress,
    pub vlan_id: u16,
    pub class_id: u8,
    /// Bits a second, with the media's overhead.
    pub bandwidth: u64,
}

/// A talker a peer declares on a port, and since when.
#[derive(Debug, Clone)]
struct Talker {
    declaration: TalkerDeclaration,
    since: Duration,
    /// The peer declares back the talker the bridge declares to it, as
    /// some endpoints do with another latency or destination: no talker
    /// of its own.
    echo: bool,
}

impl Port {
    /// Whether the bridge declares a talker for `stream_id` on the port.
    fn declares_talker(&self, stream_id: u64) -> bool {
        let name = stream_id.to_be_bytes().to_vec();
        [
            msrp::attribute::TALKER_ADVERTISE,
            msrp::attribute::TALKER_FAILED,
        ]
        .into_iter()
        .any(|attribute_type| {
            self.declared_msrp
                .contains_key(&(attribute_type, name.clone()))
        })
    }

    /// Whether a peer on the port declares a talker of its own for
    /// `stream_id`.
    fn talks(&self, stream_id: u64) -> bool {
        self.talkers
            .get(&stream_id)
            .is_some_and(|talker| !talker.echo)
    }
}

/// What a port declares for one application: each attribute's value and
/// four-packed event, by attribute type and key.
type Declarations = BTreeMap<(u8, Vec<u8>), (Vec<u8>, Option<u8>)>;

struct Port {
    status: PortStatus,
    /// The port's participants, while it is enabled.
    msrp: Option<Participant>,
    mvrp: Option<Participant>,
    /// What peers declare on it.
    talkers: BTreeMap<u64, Talker>,
    listeners: BTreeMap<u64, ListenerState>,
    domains: BTreeMap<u8, Domain>,
    vlans: BTreeSet<u16>,
    /// What the bridge declares on it.
    declared_msrp: Declarations,
    declared_mvrp: Declarations,
    /// Until when, having just come up, it waits for the neighbor's
    /// Domain declarations before declaring talkers.
    settling_until: Option<Duration>,
}

impl Port {
    fn new(status: PortStatus) -> Self {
        Port {
            status,
            msrp: None,
            mvrp: None,
            talkers: BTreeMap::new(),
            listeners: BTreeMap::new(),
            domains: BTreeMap::new(),
            vlans: BTreeSet::new(),
            declared_msrp: Declarations::new(),
            declared_mvrp: Declarations::new(),
            settling_until: None,
        }
    }

    /// Whether it still waits at `now`, having just come up, for the
    /// neighbor's Domain declarations or for asCapable.
    fn settling(&self, now: Duration) -> bool {
        (self.domains.is_empty() || !self.status.as_capable)
            && self.settling_until.is_some_and(|until| now < until)
    }

    /// Forgets all registrations and declarations, as the port's
    /// participants start again or stop.
    fn clear(&mut self) {
        self.talkers.clear();
        self.listeners.clear();
        self.domains.clear();
        self.vlans.clear();
        self.declared_msrp.clear();
        self.declared_mvrp.clear();
    }
}

/// A stream forwarded out a port, and since when, as streamAge counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Forwarded {
    destination: MacAddress,
    vlan_id: u16,
    class_id: u8,
    bandwidth: u64,
    since: Duration,
}

/// A bridge's MSRP and MVRP on all its ports. Feed it MRPDUs received
/// on each port, each port's status and the time; take back MRPDUs to
/// send, changes to its forwarding and queues, and the next deadline.
/// Times are durations from any start the caller keeps.
pub struct Bridge {
    config: Config,
    seed: u64,
    ports: BTreeMap<PortId, Port>,
    forwarded: BTreeMap<(PortId, u64), Forwarded>,
    idle_slopes: BTreeMap<(PortId, u8), u64>,
    boundaries: BTreeSet<(PortId, u8)>,
    /// Registrations or ports changed since attributes last propagated.
    stale: bool,
    outgoing: VecDeque<Transmit>,
    changes: VecDeque<Change>,
}

impl Bridge {
    /// A bridge with no ports yet; `seed` spreads its participants' timers
    /// from others'.
    pub fn new(config: Config, seed: u64) -> Self {
        Bridge {
            config,
            seed,
            ports: BTreeMap::new(),
            forwarded: BTreeMap::new(),
            idle_slopes: BTreeMap::new(),
            boundaries: BTreeSet::new(),
            stale: false,
            outgoing: VecDeque::new(),
            changes: VecDeque::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Adds `port`, or changes how it stands. A port whose link goes down
    /// loses its registrations at once (Flush!), and one that comes up
    /// declares all it should anew.
    pub fn set_port(&mut self, now: Duration, port: PortId, status: PortStatus) {
        let seed = self.seed;
        let mvrp = self.config.mvrp;
        let entry = self
            .ports
            .entry(port)
            .or_insert_with(|| Port::new(PortStatus::down()));
        let was_enabled = entry.msrp.is_some();
        entry.status = status;
        if status.enabled != was_enabled {
            for vlan_id in core::mem::take(&mut entry.vlans) {
                self.changes.push_back(Change::Vlan {
                    port,
                    vlan_id,
                    registered: false,
                });
            }
            entry.clear();
            if status.enabled {
                let port_seed = seed ^ u64::from(port.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                let mut msrp =
                    Participant::new(msrp::FORMAT, now, port_seed).with_twins(msrp::twin);
                let mut mvrp =
                    mvrp.then(|| Participant::new(mvrp::FORMAT, now, port_seed.rotate_left(17)));
                // A LeaveAll at once has the neighbor declare all again
                // within its JoinTime: one that kept registering what an
                // earlier participant declared, as across the daemon's
                // restart, would otherwise wait for its own LeaveAll, which
                // a switch that sends no periodic Joins takes up to 15 s to.
                msrp.leave_all_at(now);
                if let Some(mvrp) = mvrp.as_mut() {
                    mvrp.leave_all_at(now);
                }
                entry.msrp = Some(msrp);
                entry.mvrp = mvrp;
                entry.settling_until = Some(now + SETTLE);
            } else {
                entry.msrp = None;
                entry.mvrp = None;
                entry.settling_until = None;
            }
        }
        self.stale = true;
        self.propagate(now);
    }

    /// Takes `port` off the bridge, with all it registered.
    pub fn remove_port(&mut self, now: Duration, port: PortId) {
        if let Some(removed) = self.ports.remove(&port) {
            for vlan_id in removed.vlans {
                self.changes.push_back(Change::Vlan {
                    port,
                    vlan_id,
                    registered: false,
                });
            }
            self.stale = true;
            self.propagate(now);
        }
    }

    /// Handles an MRPDU's payload received on `port`, from its
    /// ProtocolVersion on.
    pub fn handle_pdu(
        &mut self,
        now: Duration,
        port: PortId,
        application: Application,
        pdu: &[u8],
    ) {
        let Some(entry) = self.ports.get_mut(&port) else {
            return;
        };
        let participant = match application {
            Application::Msrp => entry.msrp.as_mut(),
            Application::Mvrp => entry.mvrp.as_mut(),
        };
        let Some(participant) = participant else {
            return;
        };
        participant.handle_pdu(now, pdu);
        self.registrations(now);
        self.propagate(now);
    }

    /// Handles the timers due by `now`.
    pub fn handle_timeout(&mut self, now: Duration) {
        for port in self.ports.values_mut() {
            for participant in [port.msrp.as_mut(), port.mvrp.as_mut()]
                .into_iter()
                .flatten()
            {
                participant.handle_timeout(now);
            }
            // Done waiting for the neighbor's Domain declarations and
            // asCapable: what talkers it holds back go out now, as a
            // boundary's if either is still missing.
            if port.settling_until.is_some_and(|until| until <= now) {
                port.settling_until = None;
                self.stale = true;
            }
        }
        self.registrations(now);
        self.propagate(now);
    }

    /// The next MRPDU to send.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.outgoing.pop_front()
    }

    /// The next change to make to forwarding or queues.
    pub fn poll_change(&mut self) -> Option<Change> {
        self.changes.pop_front()
    }

    /// When `handle_timeout` should next be called; `Duration::MAX` with
    /// no port up.
    pub fn poll_timeout(&self) -> Duration {
        let settled = self.ports.values().filter_map(|port| port.settling_until);
        self.ports
            .values()
            .flat_map(|port| [port.msrp.as_ref(), port.mvrp.as_ref()])
            .flatten()
            .map(Participant::poll_timeout)
            .chain(settled)
            .min()
            .unwrap_or(Duration::MAX)
    }

    /// The streams forwarded out each port now.
    pub fn reservations(&self) -> impl Iterator<Item = Reservation> + '_ {
        self.forwarded
            .iter()
            .map(|((port, stream_id), forwarded)| Reservation {
                port: *port,
                stream_id: *stream_id,
                destination: forwarded.destination,
                vlan_id: forwarded.vlan_id,
                class_id: forwarded.class_id,
                bandwidth: forwarded.bandwidth,
            })
    }

    /// What `class_id` reserves on `port`, in bits a second.
    pub fn idle_slope(&self, port: PortId, class_id: u8) -> u64 {
        self.idle_slopes
            .get(&(port, class_id))
            .copied()
            .unwrap_or(0)
    }

    /// Whether `port` is an SRP domain boundary for `class_id`.
    pub fn boundary(&self, port: PortId, class_id: u8) -> bool {
        self.boundaries.contains(&(port, class_id))
    }

    /// The VLANs peers register on `port` through MVRP.
    pub fn vlans(&self, port: PortId) -> impl Iterator<Item = u16> + '_ {
        self.ports
            .get(&port)
            .into_iter()
            .flat_map(|port| port.vlans.iter().copied())
    }

    /// The talkers peers declare on each port, as registered, and whether
    /// each is an echo of what the bridge declares there.
    pub fn talkers(&self) -> impl Iterator<Item = (PortId, &TalkerDeclaration, bool)> + '_ {
        self.ports.iter().flat_map(|(port_id, port)| {
            port.talkers
                .values()
                .map(move |talker| (*port_id, &talker.declaration, talker.echo))
        })
    }

    /// The listeners peers declare on each port: stream ID and state.
    pub fn listeners(&self) -> impl Iterator<Item = (PortId, u64, ListenerState)> + '_ {
        self.ports.iter().flat_map(|(port_id, port)| {
            port.listeners
                .iter()
                .map(move |(stream_id, state)| (*port_id, *stream_id, *state))
        })
    }

    /// The SR class domains peers declare on `port`.
    pub fn domains(&self, port: PortId) -> impl Iterator<Item = Domain> + '_ {
        self.ports
            .get(&port)
            .into_iter()
            .flat_map(|port| port.domains.values().copied())
    }

    /// Takes in what each port's participants registered or took off.
    fn registrations(&mut self, now: Duration) {
        for (port_id, port) in self.ports.iter_mut() {
            while let Some(registration) = port.msrp.as_mut().and_then(Participant::poll_event) {
                self.stale |= stream_registration(port, now, registration);
            }
            while let Some(registration) = port.mvrp.as_mut().and_then(Participant::poll_event) {
                if let Some((vlan_id, registered)) = vlan_registration(port, registration) {
                    self.stale = true;
                    self.changes.push_back(Change::Vlan {
                        port: *port_id,
                        vlan_id,
                        registered,
                    });
                }
            }
        }
    }

    /// Runs MAP when anything it works from changed: declares on each
    /// port what it should, and changes forwarding and queues to match.
    fn propagate(&mut self, now: Duration) {
        if self.stale {
            self.stale = false;
            let plan = streams::plan(&self.config, &self.ports, &self.forwarded);
            let mut vlan_plan = vlans::plan(&self.config, &self.ports);
            let mut stream_plan = plan.declarations;
            for (port_id, port) in self.ports.iter_mut() {
                let settling = port.settling(now);
                if let Some(participant) = port.msrp.as_mut() {
                    let mut wanted = stream_plan.remove(port_id).unwrap_or_default();
                    if settling {
                        wanted
                            .retain(|(attribute_type, _), _| msrp::twin(*attribute_type).is_none());
                    }
                    declare(participant, &mut port.declared_msrp, wanted, now);
                }
                if let Some(participant) = port.mvrp.as_mut() {
                    let wanted = vlan_plan.remove(port_id).unwrap_or_default();
                    declare(participant, &mut port.declared_mvrp, wanted, now);
                }
            }
            self.reserve(now, plan.reserved, plan.idle_slopes);
            self.mark_boundaries(plan.boundaries);
        }
        for (port_id, port) in self.ports.iter_mut() {
            for (application, participant) in [
                (Application::Msrp, port.msrp.as_mut()),
                (Application::Mvrp, port.mvrp.as_mut()),
            ] {
                let Some(participant) = participant else {
                    continue;
                };
                while let Some(pdu) = participant.poll_transmit() {
                    self.outgoing.push_back(Transmit {
                        port: *port_id,
                        application,
                        pdu,
                    });
                }
            }
        }
    }

    /// Changes forwarding and idle slopes to `wanted` and `idle_slopes`:
    /// streams that stop first, then the slopes, then streams that start.
    fn reserve(
        &mut self,
        now: Duration,
        wanted: BTreeMap<(PortId, u64), streams::Wanted>,
        idle_slopes: BTreeMap<(PortId, u8), u64>,
    ) {
        let stopping: Vec<(PortId, u64)> = self
            .forwarded
            .iter()
            .filter(|(key, forwarded)| {
                wanted.get(key).is_none_or(|stream| {
                    stream.destination != forwarded.destination
                        || stream.vlan_id != forwarded.vlan_id
                })
            })
            .map(|(key, _)| *key)
            .collect();
        for key in stopping {
            if let Some(forwarded) = self.forwarded.remove(&key) {
                self.changes.push_back(Change::Forwarding {
                    port: key.0,
                    stream_id: key.1,
                    destination: forwarded.destination,
                    vlan_id: forwarded.vlan_id,
                    forwarding: false,
                });
            }
        }
        let classes: BTreeSet<(PortId, u8)> = self
            .idle_slopes
            .keys()
            .chain(idle_slopes.keys())
            .copied()
            .collect();
        for key in classes {
            let bits_per_second = idle_slopes.get(&key).copied().unwrap_or(0);
            if self.idle_slopes.get(&key).copied().unwrap_or(0) != bits_per_second {
                self.changes.push_back(Change::IdleSlope {
                    port: key.0,
                    class_id: key.1,
                    bits_per_second,
                });
            }
        }
        self.idle_slopes = idle_slopes;
        for (key, stream) in wanted {
            match self.forwarded.get_mut(&key) {
                Some(forwarded) => {
                    forwarded.class_id = stream.class_id;
                    forwarded.bandwidth = stream.bandwidth;
                }
                None => {
                    self.forwarded.insert(
                        key,
                        Forwarded {
                            destination: stream.destination,
                            vlan_id: stream.vlan_id,
                            class_id: stream.class_id,
                            bandwidth: stream.bandwidth,
                            since: now,
                        },
                    );
                    self.changes.push_back(Change::Forwarding {
                        port: key.0,
                        stream_id: key.1,
                        destination: stream.destination,
                        vlan_id: stream.vlan_id,
                        forwarding: true,
                    });
                }
            }
        }
    }

    fn mark_boundaries(&mut self, boundaries: BTreeSet<(PortId, u8)>) {
        for (port, class_id) in self.boundaries.difference(&boundaries) {
            self.changes.push_back(Change::Boundary {
                port: *port,
                class_id: *class_id,
                boundary: false,
            });
        }
        for (port, class_id) in boundaries.difference(&self.boundaries) {
            self.changes.push_back(Change::Boundary {
                port: *port,
                class_id: *class_id,
                boundary: true,
            });
        }
        self.boundaries = boundaries;
    }
}

/// Makes `participant` declare `wanted`, withdrawing what it declared
/// before and no longer should.
fn declare(
    participant: &mut Participant,
    declared: &mut Declarations,
    wanted: Declarations,
    now: Duration,
) {
    for (attribute_type, key) in declared.keys() {
        if !wanted.contains_key(&(*attribute_type, key.clone())) {
            participant.withdraw(now, *attribute_type, key);
        }
    }
    for ((attribute_type, key), value) in &wanted {
        if declared.get(&(*attribute_type, key.clone())) != Some(value) {
            participant.declare(now, *attribute_type, value.0.clone(), value.1);
        }
    }
    *declared = wanted;
}

/// The key a participant names an attribute by: the leading octets of
/// its value, as the application's format counts them.
fn key(application: Application, attribute_type: u8, value: &[u8]) -> Vec<u8> {
    let length = (application.format().key_length)(attribute_type).min(value.len());
    Vec::from(&value[..length])
}

/// The stream ID a talker or listener value starts with.
fn stream_of(value: &[u8]) -> Option<u64> {
    let octets: [u8; 8] = value.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(octets))
}

/// Keeps an MSRP registration change on `port`; whether anything changed.
fn stream_registration(port: &mut Port, now: Duration, registration: Registration) -> bool {
    match registration {
        Registration::Joined {
            attribute_type,
            value,
            four_packed,
        } => match attribute_type {
            msrp::attribute::TALKER_ADVERTISE | msrp::attribute::TALKER_FAILED => {
                let Some(declaration) = msrp::decode_talker(attribute_type, &value, Event::JoinIn)
                else {
                    return false;
                };
                let stream_id = declaration.stream_id;
                let before = port.talkers.get(&stream_id);
                let since = before.map_or(now, |talker| talker.since);
                // Once an echo, it stays one while it stays registered.
                let echo = before.map_or(port.declares_talker(stream_id), |talker| talker.echo);
                port.talkers.insert(
                    stream_id,
                    Talker {
                        declaration,
                        since,
                        echo,
                    },
                );
                true
            }
            msrp::attribute::LISTENER => {
                let Some(stream_id) = stream_of(&value) else {
                    return false;
                };
                let state = ListenerState::from_number(four_packed.unwrap_or(0));
                port.listeners.insert(stream_id, state) != Some(state)
            }
            msrp::attribute::DOMAIN => {
                let Some(domain) = Domain::decode(&value) else {
                    return false;
                };
                port.domains.insert(domain.class_id, domain) != Some(domain)
            }
            _ => false,
        },
        Registration::Left {
            attribute_type,
            value,
        } => match attribute_type {
            msrp::attribute::TALKER_ADVERTISE | msrp::attribute::TALKER_FAILED => {
                let Some(stream_id) = stream_of(&value) else {
                    return false;
                };
                // A talker turning Failed or back declares the new type
                // before the old one leaves.
                let failed = attribute_type == msrp::attribute::TALKER_FAILED;
                let current = port
                    .talkers
                    .get(&stream_id)
                    .is_some_and(|talker| talker.declaration.failure.is_some() == failed);
                current && port.talkers.remove(&stream_id).is_some()
            }
            msrp::attribute::LISTENER => stream_of(&value)
                .is_some_and(|stream_id| port.listeners.remove(&stream_id).is_some()),
            msrp::attribute::DOMAIN => value
                .first()
                .is_some_and(|class_id| port.domains.remove(class_id).is_some()),
            _ => false,
        },
    }
}

/// Keeps an MVRP registration change on `port`: the VLAN and whether it
/// is registered now, where that changed.
fn vlan_registration(port: &mut Port, registration: Registration) -> Option<(u16, bool)> {
    let (attribute_type, value, registered) = match registration {
        Registration::Joined {
            attribute_type,
            value,
            ..
        } => (attribute_type, value, true),
        Registration::Left {
            attribute_type,
            value,
        } => (attribute_type, value, false),
    };
    if attribute_type != mvrp::VID {
        return None;
    }
    let octets: [u8; 2] = value.get(..2)?.try_into().ok()?;
    let vlan_id = u16::from_be_bytes(octets) & 0x0fff;
    let changed = if registered {
        port.vlans.insert(vlan_id)
    } else {
        port.vlans.remove(&vlan_id)
    };
    changed.then_some((vlan_id, registered))
}
