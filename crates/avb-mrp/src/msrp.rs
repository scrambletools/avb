//! MSRP (IEEE 802.1Q-2022, 35): its attributes as an MRP participant
//! declares and registers them, and the talker declarations in a message,
//! as an MRPDU or a CVU SRP message carries them.

use alloc::vec::Vec;
use core::time::Duration;

use avb_net::MacAddress;

use crate::Participant;
use crate::mrpdu::{Format, add};

/// MSRP attribute types (35.2.2.4).
pub mod attribute {
    pub const TALKER_ADVERTISE: u8 = 1;
    pub const TALKER_FAILED: u8 = 2;
    pub const LISTENER: u8 = 3;
    pub const DOMAIN: u8 = 4;
}

pub use crate::mrpdu::Event;

/// Class measurement intervals a second, by SR class: how often a
/// class's talkers may send their MaxIntervalFrames.
pub mod intervals {
    /// Class A's, one each 125 us.
    pub const CLASS_A: u32 = 8000;
    /// Class B's, one each 250 us.
    pub const CLASS_B: u32 = 4000;
    /// Class C's by default, one each 500 us. A network may set its own,
    /// as to its control cycle, but MSRP does not carry it: every bridge
    /// and talker in the SRP domain sets the same, and a talker counts its
    /// MaxIntervalFrames against it.
    pub const CLASS_C: u32 = 2000;
}

/// The format of MSRP's MRPDUs, for a [`crate::Participant`].
pub const FORMAT: Format = Format {
    list_length: true,
    first_value_length,
    key_length,
    four_packed: |attribute_type| attribute_type == attribute::LISTENER,
    nth_value,
};

fn first_value_length(attribute_type: u8) -> Option<usize> {
    match attribute_type {
        attribute::TALKER_ADVERTISE => Some(25),
        attribute::TALKER_FAILED => Some(34),
        attribute::LISTENER => Some(8),
        attribute::DOMAIN => Some(4),
        _ => None,
    }
}

/// Talkers and listeners are named by their stream ID, domains by their
/// SR class.
fn key_length(attribute_type: u8) -> usize {
    match attribute_type {
        attribute::DOMAIN => 1,
        _ => 8,
    }
}

/// Values after the first count on its stream ID, and a talker's
/// destination too (35.2.2.8.x); a domain's class and priority, its VLAN
/// staying the same (35.2.2.9).
fn nth_value(attribute_type: u8, first: &[u8], index: u16) -> Vec<u8> {
    let mut value = Vec::from(first);
    match attribute_type {
        attribute::DOMAIN if value.len() >= 4 => {
            value[0] = value[0].wrapping_add(index as u8);
            value[1] = value[1].wrapping_add(index as u8);
        }
        attribute::TALKER_ADVERTISE | attribute::TALKER_FAILED if value.len() >= 14 => {
            add(&mut value[..8], index);
            add(&mut value[8..14], index);
        }
        _ if value.len() >= 8 => add(&mut value[..8], index),
        _ => {}
    }
    value
}

/// A Listener declaration's four-packed event (35.2.2.8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerState {
    Ignore,
    AskingFailed,
    Ready,
    ReadyFailed,
}

impl ListenerState {
    pub fn from_number(number: u8) -> Self {
        match number & 0x3 {
            0 => ListenerState::Ignore,
            1 => ListenerState::AskingFailed,
            2 => ListenerState::Ready,
            _ => ListenerState::ReadyFailed,
        }
    }

    pub fn number(self) -> u8 {
        match self {
            ListenerState::Ignore => 0,
            ListenerState::AskingFailed => 1,
            ListenerState::Ready => 2,
            ListenerState::ReadyFailed => 3,
        }
    }

    /// Whether at least one listener behind it can receive the stream.
    pub fn ready(self) -> bool {
        matches!(self, ListenerState::Ready | ListenerState::ReadyFailed)
    }
}

/// An SR class's domain: its class ID, priority and VLAN (35.2.2.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Domain {
    /// 6 for class A, 5 for class B, 4 for class C.
    pub class_id: u8,
    pub priority: u8,
    pub vlan_id: u16,
}

impl Domain {
    /// Class A on its default priority and VLAN.
    pub const CLASS_A: Domain = Domain {
        class_id: 6,
        priority: 3,
        vlan_id: 2,
    };

    /// Class B on its default priority and VLAN.
    pub const CLASS_B: Domain = Domain {
        class_id: 5,
        priority: 2,
        vlan_id: 2,
    };

    /// Class C on its default priority and VLAN, for control streams.
    /// Opt-in: no port declares it unless asked, and a talker in it can
    /// fall back to Class A where it cannot be reserved ([`fall_back`]).
    /// A network may give it another priority and VLAN, the same across
    /// its SRP domain, and a talker takes them from its port's Domain
    /// declaration ([`Domain::registered`]), these until one comes.
    pub const CLASS_C: Domain = Domain {
        class_id: 4,
        priority: 6,
        vlan_id: 2,
    };

    /// This class's domain as a port's neighbor declares it among
    /// `registered`, by class ID whatever its priority and VLAN, or this
    /// one until the neighbor declares it. A talker in the class takes its
    /// priority and VLAN from it (35.2.2.9.3, 35.2.2.9.4).
    pub fn registered(&self, registered: impl IntoIterator<Item = Domain>) -> Domain {
        registered
            .into_iter()
            .find(|domain| domain.class_id == self.class_id)
            .unwrap_or(*self)
    }

    /// Whether a port whose neighbor declares the domains `registered` is
    /// a boundary of this class's SRP domain (35.2.1.4 h): none of them
    /// has its class ID with its priority. This is a bridge's rule, for
    /// a priority it is configured with; a talker, which takes its class's
    /// priority from the neighbor, asks [`fallback`].
    pub fn boundary(&self, registered: impl IntoIterator<Item = Domain>) -> bool {
        !registered
            .into_iter()
            .any(|domain| domain.class_id == self.class_id && domain.priority == self.priority)
    }

    pub fn value(&self) -> Vec<u8> {
        let vlan = self.vlan_id.to_be_bytes();
        Vec::from([self.class_id, self.priority, vlan[0], vlan[1]])
    }

    pub fn decode(value: &[u8]) -> Option<Self> {
        let [class_id, priority, high, low, ..] = *value else {
            return None;
        };
        Some(Domain {
            class_id,
            priority,
            vlan_id: u16::from_be_bytes([high, low]) & 0x0fff,
        })
    }
}

/// The value of a Talker Advertise for `declaration`, or of a Talker
/// Failed when it says why.
pub fn talker_value(declaration: &TalkerDeclaration) -> Vec<u8> {
    let mut value = Vec::with_capacity(34);
    value.extend_from_slice(&declaration.stream_id.to_be_bytes());
    value.extend_from_slice(&declaration.destination.0);
    value.extend_from_slice(&declaration.vlan_id.to_be_bytes());
    value.extend_from_slice(&declaration.max_frame_size.to_be_bytes());
    value.extend_from_slice(&declaration.max_interval_frames.to_be_bytes());
    value.push((declaration.priority << 5) | if declaration.rank { 0x10 } else { 0 });
    value.extend_from_slice(&declaration.accumulated_latency.to_be_bytes());
    if let Some(failure) = declaration.failure {
        value.extend_from_slice(&failure.bridge_id.to_be_bytes());
        value.push(failure.code);
    }
    value
}

/// The other talker attribute type: a stream's Talker Advertise and
/// Talker Failed displace each other at a port (802.1Q-2022 35.2.6), for
/// [`crate::Participant::with_twins`].
pub fn twin(attribute_type: u8) -> Option<u8> {
    match attribute_type {
        attribute::TALKER_ADVERTISE => Some(attribute::TALKER_FAILED),
        attribute::TALKER_FAILED => Some(attribute::TALKER_ADVERTISE),
        _ => None,
    }
}

/// The talker declaration a Talker Advertise or Talker Failed value
/// holds.
pub fn decode_talker(attribute_type: u8, value: &[u8], event: Event) -> Option<TalkerDeclaration> {
    declaration(value, attribute_type == attribute::TALKER_FAILED, 0, event)
}

/// Why a talker's reservation failed, and where (35.2.2.8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TalkerFailure {
    pub bridge_id: u64,
    pub code: u8,
}

/// One stream a talker declares: Talker Advertise, or Talker Failed when
/// `failure` says why (35.2.2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TalkerDeclaration {
    pub stream_id: u64,
    pub destination: MacAddress,
    pub vlan_id: u16,
    /// The largest frame, in octets of its payload.
    pub max_frame_size: u16,
    /// The most frames in each class measurement interval.
    pub max_interval_frames: u16,
    pub priority: u8,
    /// Rank 1, a stream that is not an emergency one.
    pub rank: bool,
    /// Nanoseconds.
    pub accumulated_latency: u32,
    pub failure: Option<TalkerFailure>,
    pub event: Event,
}

/// Octets each frame takes on the wire beyond its payload: preamble and
/// start of frame, header with a VLAN tag, frame check and interframe gap.
const FRAME_OVERHEAD: u64 = 8 + 18 + 4 + 12;

impl TalkerDeclaration {
    /// The bandwidth the stream takes in bits per second, at
    /// `intervals_per_second` class measurement intervals: 8000 for class
    /// A, 4000 for class B, 2000 for class C by default ([`intervals`]).
    pub fn bandwidth(&self, intervals_per_second: u32) -> u64 {
        (u64::from(self.max_frame_size) + FRAME_OVERHEAD)
            * 8
            * u64::from(self.max_interval_frames)
            * u64::from(intervals_per_second)
    }

    /// The declaration in `domain`'s class: its priority and VLAN, as a
    /// talker takes them from its port's Domain declaration (35.2.2.9.3,
    /// 35.2.2.9.4). All else stays.
    pub fn in_domain(&self, domain: Domain) -> TalkerDeclaration {
        TalkerDeclaration {
            priority: domain.priority,
            vlan_id: domain.vlan_id,
            ..*self
        }
    }

    /// The declaration as its talker declares it again in Class A, from
    /// Class C measured `class_c_intervals` times a second: `class_a`'s
    /// priority and VLAN, Class A's domain as its port registers it
    /// ([`Domain::registered`]), and its frames per interval counted again
    /// for Class A's 125 us ([`interval_frames`]). Its stream, destination,
    /// frame size, rank and latency stay.
    pub fn in_class_a(&self, class_a: Domain, class_c_intervals: u32) -> TalkerDeclaration {
        TalkerDeclaration {
            max_interval_frames: interval_frames(
                self.max_interval_frames,
                class_c_intervals,
                intervals::CLASS_A,
            ),
            failure: None,
            ..self.in_domain(class_a)
        }
    }
}

/// The frames in each interval of a class measured `to` times a second
/// that carry a stream sending `frames` in each of `from`'s: as many a
/// second, rounded up to a whole frame, and at least one.
///
/// From Class C's 2000 to Class A's 8000 that is frames / 4 rounded up,
/// the least that carries the stream in whole frames: one frame each
/// 500 us reserves four times its rate in Class A, one each 125 us. The
/// talker then spreads its frames, no more than that many in any one of
/// the new intervals; one that keeps sending a Class C interval's frames
/// together needs `frames` unchanged.
pub fn interval_frames(frames: u16, from: u32, to: u32) -> u16 {
    let per_second = u64::from(frames) * u64::from(from);
    let frames = per_second.div_ceil(u64::from(to.max(1)));
    u16::try_from(frames).unwrap_or(u16::MAX).max(1)
}

/// Why a talker in Class C declares its stream again: in Class A, or in
/// Class C as its port now has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Its port is the edge of Class C's SRP domain: the neighbor
    /// declares domains, but not Class C at any priority. It falls back
    /// to Class A.
    Boundary,
    /// Class C is on its port, but at another priority or VLAN than the
    /// declaration's, as where the talker declared before the neighbor's
    /// Domain came. It stays in Class C, in this domain.
    Domain(Domain),
    /// Its stream's listeners answer only AskingFailed: none of them can
    /// reserve it in Class C, as where a bridge on the way fails it. It
    /// falls back to Class A.
    AskingFailed,
}

/// Whether a talker that declares `declaration` in Class C should declare
/// its stream again, and why: `domains` are the domains its port
/// registers, `listener` the listener state registered for its stream.
/// Opt-in: nothing asks unless a talker does, and a declaration at Class
/// A's or Class B's priority, as the port registers them or by default,
/// never falls back. Any other is taken for Class C's, so a talker asks
/// only of its Class C streams.
///
/// A port that registers no domain at all, as before its neighbor first
/// declares, is no boundary yet. The neighbor's domains come one at a
/// time, so ask about the boundary once they have settled, as a bridge
/// holds a new port's talkers for its neighbor's. The listeners' answer
/// may come at any time, but one to a declaration the talker has since
/// replaced may stay registered a while: ask about it once they have
/// answered the new one.
pub fn fallback(
    declaration: &TalkerDeclaration,
    domains: impl IntoIterator<Item = Domain>,
    listener: Option<ListenerState>,
) -> Option<Fallback> {
    let domains: Vec<Domain> = domains.into_iter().collect();
    let registered = |class: Domain| class.registered(domains.iter().copied());
    let a_or_b = [Domain::CLASS_A, Domain::CLASS_B].map(|class| registered(class).priority);
    if a_or_b.contains(&declaration.priority) {
        return None;
    }
    let class_c = registered(Domain::CLASS_C);
    let declared = domains
        .iter()
        .any(|domain| domain.class_id == class_c.class_id);
    if !domains.is_empty() && !declared {
        Some(Fallback::Boundary)
    } else if declared
        && (class_c.priority, class_c.vlan_id) != (declaration.priority, declaration.vlan_id)
    {
        Some(Fallback::Domain(class_c))
    } else if listener == Some(ListenerState::AskingFailed) {
        Some(Fallback::AskingFailed)
    } else {
        None
    }
}

/// The domains `participant` registers, and the listener state it
/// registers for `declaration`'s stream.
fn registrations(
    participant: &Participant,
    declaration: &TalkerDeclaration,
) -> (Vec<Domain>, Option<ListenerState>) {
    let stream = declaration.stream_id.to_be_bytes();
    let mut domains = Vec::new();
    let mut listener = None;
    for (attribute_type, value, four_packed) in participant.registered() {
        match attribute_type {
            attribute::DOMAIN => domains.extend(Domain::decode(value)),
            attribute::LISTENER if value.get(..8) == Some(&stream[..]) => {
                listener = four_packed.map(ListenerState::from_number);
            }
            _ => {}
        }
    }
    (domains, listener)
}

/// [`fallback`], from what `participant`, the talker's MSRP participant
/// on its port, registers: the port's domains and the listener state for
/// `declaration`'s stream.
pub fn registered_fallback(
    participant: &Participant,
    declaration: &TalkerDeclaration,
) -> Option<Fallback> {
    let (domains, listener) = registrations(participant, declaration);
    fallback(declaration, domains, listener)
}

/// Declares a Class C talker's stream again on `participant` when
/// [`registered_fallback`] says it should. Returns why, and the
/// declaration the talker now sends its stream by:
/// - for [`Fallback::Domain`], in Class C's domain as the port registers
///   it ([`TalkerDeclaration::in_domain`]);
/// - otherwise in Class A, as the port registers Class A's domain, its
///   frames per interval counted again from Class C's `class_c_intervals`
///   a second ([`TalkerDeclaration::in_class_a`]).
///
/// MSRP names a talker by its stream ID, so the new declaration
/// withdraws the old one as it goes out, a New in its place, with no
/// Leave between that would take the stream from a listener. The talker
/// then sends its frames at the new priority and VLAN. One fallen back
/// stays in Class A: to try Class C again, declare the stream in it
/// again.
pub fn fall_back(
    participant: &mut Participant,
    now: Duration,
    declaration: &TalkerDeclaration,
    class_c_intervals: u32,
) -> Option<(Fallback, TalkerDeclaration)> {
    let (domains, listener) = registrations(participant, declaration);
    let why = fallback(declaration, domains.iter().copied(), listener)?;
    let again = match why {
        Fallback::Domain(class_c) => TalkerDeclaration {
            failure: None,
            ..declaration.in_domain(class_c)
        },
        Fallback::Boundary | Fallback::AskingFailed => {
            declaration.in_class_a(Domain::CLASS_A.registered(domains), class_c_intervals)
        }
    };
    participant.declare(now, attribute::TALKER_ADVERTISE, talker_value(&again), None);
    Some((why, again))
}

/// The talker declarations in an MSRP message, from its AttributeType on.
pub fn talker_declarations(message: &[u8]) -> TalkerDeclarations<'_> {
    let (attribute_type, length, list) = match message {
        [attribute_type, length, list_length @ ..] if list_length.len() >= 2 => {
            let declared = usize::from(u16::from_be_bytes([list_length[0], list_length[1]]));
            let list = &list_length[2..];
            (
                *attribute_type,
                usize::from(*length),
                &list[..declared.min(list.len())],
            )
        }
        _ => (0, 0, &[][..]),
    };
    let talker = matches!(
        attribute_type,
        attribute::TALKER_ADVERTISE | attribute::TALKER_FAILED
    );
    TalkerDeclarations {
        failed: attribute_type == attribute::TALKER_FAILED,
        length,
        list: if talker { list } else { &[] },
        vector: None,
    }
}

/// An iterator over the talker declarations of an MSRP message.
pub struct TalkerDeclarations<'a> {
    failed: bool,
    /// The octets of a first value.
    length: usize,
    /// The vector attributes still to read.
    list: &'a [u8],
    /// The vector being read: its first value, its packed events, how many
    /// values it has and the next one.
    vector: Option<(&'a [u8], &'a [u8], usize, usize)>,
}

impl Iterator for TalkerDeclarations<'_> {
    type Item = TalkerDeclaration;

    fn next(&mut self) -> Option<TalkerDeclaration> {
        loop {
            if let Some((first, events, count, place)) = self.vector {
                if place < count {
                    self.vector = Some((first, events, count, place + 1));
                    let event = events
                        .get(place / 3)
                        .and_then(|&packed| {
                            let shift = [36, 6, 1][place % 3];
                            Event::from_number(packed / shift % 6)
                        })
                        .unwrap_or(Event::Mt);
                    return declaration(first, self.failed, place as u64, event);
                }
                self.vector = None;
            }
            // A vector attribute: its header, its first value, then its
            // events packed three to an octet; an EndMark ends the list.
            let header = self.list.get(..2)?;
            let count = usize::from(u16::from_be_bytes([header[0], header[1]]) & 0x1fff);
            if count == 0 {
                self.list = &[];
                return None;
            }
            let events = count.div_ceil(3);
            let first = self.list.get(2..2 + self.length)?;
            let packed = self.list.get(2 + self.length..2 + self.length + events)?;
            self.list = &self.list[2 + self.length + events..];
            self.vector = Some((first, packed, count, 0));
        }
    }
}

/// The `place`th value of a vector whose first value is `first`: its
/// stream ID and destination counted on from the first's.
fn declaration(first: &[u8], failed: bool, place: u64, event: Event) -> Option<TalkerDeclaration> {
    let needed = if failed { 34 } else { 25 };
    if first.len() < needed {
        return None;
    }
    let read_u64 = |at: usize| {
        let mut octets = [0; 8];
        octets.copy_from_slice(&first[at..at + 8]);
        u64::from_be_bytes(octets)
    };
    let mut mac = [0; 8];
    mac[2..].copy_from_slice(&first[8..14]);
    let mac = (u64::from_be_bytes(mac) + place) & 0xffff_ffff_ffff;
    let mut destination = [0; 6];
    destination.copy_from_slice(&mac.to_be_bytes()[2..]);
    let priority_and_rank = first[20];
    Some(TalkerDeclaration {
        stream_id: read_u64(0).wrapping_add(place),
        destination: MacAddress(destination),
        vlan_id: u16::from_be_bytes([first[14], first[15]]) & 0x0fff,
        max_frame_size: u16::from_be_bytes([first[16], first[17]]),
        max_interval_frames: u16::from_be_bytes([first[18], first[19]]),
        priority: priority_and_rank >> 5,
        rank: priority_and_rank & 0x10 != 0,
        accumulated_latency: u32::from_be_bytes([first[21], first[22], first[23], first[24]]),
        failure: failed.then(|| TalkerFailure {
            bridge_id: read_u64(25),
            code: first[33],
        }),
        event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Talker Advertise message for two streams, the first joining and
    /// the second leaving, as a CVU SRP talker message carries it.
    fn advertise() -> Vec<u8> {
        let mut message = vec![attribute::TALKER_ADVERTISE, 25, 0, 0];
        message.extend_from_slice(&[0x00, 0x02]); // two values
        message.extend_from_slice(&0xe8f6_0ae0_9220_0000u64.to_be_bytes());
        message.extend_from_slice(&[0x91, 0xe0, 0xf0, 0x00, 0x6a, 0x20]);
        message.extend_from_slice(&2u16.to_be_bytes()); // VLAN
        message.extend_from_slice(&224u16.to_be_bytes()); // frame size
        message.extend_from_slice(&1u16.to_be_bytes()); // frames per interval
        message.push(0xb0); // priority 5, rank 1
        message.extend_from_slice(&500_000u32.to_be_bytes());
        message.push(36 + 5 * 6); // JoinIn, Lv, New
        message.extend_from_slice(&[0, 0]); // EndMark
        let list = (message.len() - 4) as u16;
        message[2..4].copy_from_slice(&list.to_be_bytes());
        message
    }

    #[test]
    fn talker_advertisements_decode_value_by_value() {
        let declarations: Vec<TalkerDeclaration> = talker_declarations(&advertise()).collect();
        assert_eq!(declarations.len(), 2);
        let first = declarations[0];
        assert_eq!(first.stream_id, 0xe8f6_0ae0_9220_0000);
        assert_eq!(
            first.destination,
            MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x6a, 0x20])
        );
        assert_eq!((first.vlan_id, first.priority, first.rank), (2, 5, true));
        assert_eq!(first.accumulated_latency, 500_000);
        assert_eq!(first.event, Event::JoinIn);
        assert!(first.event.declares() && first.failure.is_none());
        let second = declarations[1];
        assert_eq!(second.stream_id, 0xe8f6_0ae0_9220_0001);
        assert_eq!(second.destination.0[5], 0x21);
        assert_eq!(second.event, Event::Lv);
        assert!(!second.event.declares());
        // 224 octets of payload, one frame each 125 us.
        assert_eq!(first.bandwidth(8000), (224 + 42) * 8 * 8000);
    }

    #[test]
    fn other_attributes_and_short_messages_hold_no_talkers() {
        let mut listener = advertise();
        listener[0] = attribute::LISTENER;
        assert_eq!(talker_declarations(&listener).count(), 0);
        assert_eq!(talker_declarations(&advertise()[..20]).count(), 0);
        assert_eq!(talker_declarations(&[]).count(), 0);
    }

    #[test]
    fn class_c_is_class_4_at_priority_6_on_vlan_2() {
        let c = Domain::CLASS_C;
        assert_eq!((c.class_id, c.priority, c.vlan_id), (4, 6, 2));
        assert_eq!(c.value(), [4, 6, 0, 2]);
        assert_eq!(Domain::decode(&c.value()), Some(c));
        assert_eq!(intervals::CLASS_C, 2000);
        // Classes A and B stay as they were.
        assert_eq!(Domain::CLASS_A.value(), [6, 3, 0, 2]);
        assert_eq!(Domain::CLASS_B.value(), [5, 2, 0, 2]);
        assert_eq!((intervals::CLASS_A, intervals::CLASS_B), (8000, 4000));
    }

    /// One frame of 64 octets each 500 us, in Class C.
    fn control() -> TalkerDeclaration {
        TalkerDeclaration {
            stream_id: 0xa010_a3ff_70a8_1140,
            destination: MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x70, 0x01]),
            vlan_id: 2,
            max_frame_size: 64,
            max_interval_frames: 1,
            priority: Domain::CLASS_C.priority,
            rank: true,
            accumulated_latency: 50_000,
            failure: None,
            event: Event::New,
        }
    }

    const A_AND_B: [Domain; 2] = [Domain::CLASS_A, Domain::CLASS_B];
    const A_B_AND_C: [Domain; 3] = [Domain::CLASS_A, Domain::CLASS_B, Domain::CLASS_C];

    #[test]
    fn a_port_without_class_c_is_its_boundary() {
        let c = Domain::CLASS_C;
        assert!(c.boundary(A_AND_B));
        assert!(!c.boundary(A_B_AND_C));
        // For a bridge, Class C's ID at another priority is a boundary too.
        assert!(c.boundary([Domain { priority: 5, ..c }]));
        assert_eq!(
            fallback(&control(), A_AND_B, None),
            Some(Fallback::Boundary)
        );
        assert_eq!(
            fallback(&control(), A_AND_B, Some(ListenerState::Ready)),
            Some(Fallback::Boundary)
        );
        assert_eq!(fallback(&control(), A_B_AND_C, None), None);
        // No domain registered yet is no boundary yet.
        assert_eq!(fallback(&control(), [], None), None);
        // A talker that took another priority falls back where Class C
        // goes, as where it moves to a neighbor without it.
        let at_5 = TalkerDeclaration {
            priority: 5,
            ..control()
        };
        assert_eq!(fallback(&at_5, A_AND_B, None), Some(Fallback::Boundary));
    }

    /// Class C where a network gives it priority 5.
    const C_AT_5: Domain = Domain {
        priority: 5,
        ..Domain::CLASS_C
    };

    #[test]
    fn class_c_takes_its_priority_and_vlan_from_the_domain() {
        // As the neighbor declares it, or the default until it does.
        assert_eq!(
            Domain::CLASS_C.registered([Domain::CLASS_A, C_AT_5]),
            C_AT_5
        );
        assert_eq!(Domain::CLASS_C.registered(A_AND_B), Domain::CLASS_C);
        assert_eq!(Domain::CLASS_A.registered(A_B_AND_C), Domain::CLASS_A);
        let on_vlan_3 = Domain {
            vlan_id: 3,
            ..C_AT_5
        };
        assert_eq!(
            control().in_domain(on_vlan_3),
            TalkerDeclaration {
                priority: 5,
                vlan_id: 3,
                ..control()
            }
        );

        // Class C at another priority is no boundary: the talker declares
        // again in it, before it looks at its listeners, as a bridge fails
        // a stream at a priority it has no class for.
        let with_c_at_5 = [Domain::CLASS_A, Domain::CLASS_B, C_AT_5];
        let asking = Some(ListenerState::AskingFailed);
        for listener in [None, asking] {
            assert_eq!(
                fallback(&control(), with_c_at_5, listener),
                Some(Fallback::Domain(C_AT_5))
            );
        }
        let at_5 = control().in_domain(C_AT_5);
        assert_eq!(fallback(&at_5, with_c_at_5, None), None);
        assert_eq!(
            fallback(&at_5, with_c_at_5, asking),
            Some(Fallback::AskingFailed)
        );
        // Another VLAN alone declares it again too.
        assert_eq!(
            fallback(&at_5, [Domain::CLASS_A, on_vlan_3], None),
            Some(Fallback::Domain(on_vlan_3))
        );
    }

    #[test]
    fn listeners_asking_failed_only_fall_class_c_back() {
        let asking = Some(ListenerState::AskingFailed);
        assert_eq!(
            fallback(&control(), A_B_AND_C, asking),
            Some(Fallback::AskingFailed)
        );
        assert_eq!(
            fallback(&control(), [], asking),
            Some(Fallback::AskingFailed)
        );
        for state in [
            ListenerState::Ignore,
            ListenerState::Ready,
            ListenerState::ReadyFailed,
        ] {
            assert_eq!(fallback(&control(), A_B_AND_C, Some(state)), None);
        }
    }

    #[test]
    fn class_a_and_b_talkers_never_fall_back() {
        for priority in [Domain::CLASS_A.priority, Domain::CLASS_B.priority] {
            let talker = TalkerDeclaration {
                priority,
                ..control()
            };
            let asking = Some(ListenerState::AskingFailed);
            assert_eq!(fallback(&talker, A_AND_B, asking), None);
            assert_eq!(fallback(&talker, [], asking), None);
            let with_c_at_5 = [Domain::CLASS_A, Domain::CLASS_B, C_AT_5];
            assert_eq!(fallback(&talker, with_c_at_5, asking), None);
        }
    }

    #[test]
    fn frames_per_interval_carry_the_same_frames_a_second() {
        let (c, a) = (intervals::CLASS_C, intervals::CLASS_A);
        // At least one each 125 us, then a quarter, rounded up.
        for (frames, expected) in [(0, 1), (1, 1), (4, 1), (5, 2), (8, 2), (9, 3)] {
            assert_eq!(interval_frames(frames, c, a), expected, "{frames}");
        }
        // A profile's own Class C interval, 250 us.
        assert_eq!(interval_frames(3, 4000, a), 2);
        // The other way round, and no overflow or division by zero.
        assert_eq!(interval_frames(3, a, c), 12);
        assert_eq!(interval_frames(u16::MAX, a, c), u16::MAX);
        assert_eq!(interval_frames(2, c, 0), 4000);

        let talker = TalkerDeclaration {
            max_interval_frames: 6,
            ..control()
        };
        let class_a = talker.in_class_a(Domain::CLASS_A, c);
        assert_eq!(
            class_a,
            TalkerDeclaration {
                priority: 3,
                max_interval_frames: 2,
                ..talker
            }
        );
        assert!(class_a.bandwidth(a) >= talker.bandwidth(c));
        // Class A's priority and VLAN as its port has them.
        let class_a_on_vlan_3 = Domain {
            priority: 4,
            vlan_id: 3,
            ..Domain::CLASS_A
        };
        let class_a = talker.in_class_a(class_a_on_vlan_3, c);
        assert_eq!((class_a.priority, class_a.vlan_id), (4, 3));
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Runs a talker's participant and its neighbor's from `from` to `to`
    /// ms, handing each one's MRPDUs to the other; returns what the
    /// neighbor registered as it changed.
    fn run(
        talker: &mut Participant,
        neighbor: &mut Participant,
        from: u64,
        to: u64,
    ) -> Vec<crate::Registration> {
        let mut heard = Vec::new();
        for now in (from..=to).step_by(10).map(ms) {
            talker.handle_timeout(now);
            neighbor.handle_timeout(now);
            while let Some(pdu) = talker.poll_transmit() {
                neighbor.handle_pdu(now, &pdu);
            }
            while let Some(pdu) = neighbor.poll_transmit() {
                talker.handle_pdu(now, &pdu);
            }
            while talker.poll_event().is_some() {}
            heard.extend(core::iter::from_fn(|| neighbor.poll_event()));
        }
        heard
    }

    /// A Class C talker on a port whose neighbor declares `domains`, and,
    /// when `listener` says, a listener state for its stream.
    fn talker_beside(
        domains: &[Domain],
        listener: Option<ListenerState>,
    ) -> (Participant, Participant) {
        let mut talker = Participant::new(FORMAT, ms(0), 11).with_twins(twin);
        let mut neighbor = Participant::new(FORMAT, ms(0), 12).with_twins(twin);
        for domain in domains {
            neighbor.declare(ms(0), attribute::DOMAIN, domain.value(), None);
        }
        let stream = control().stream_id.to_be_bytes().to_vec();
        if let Some(state) = listener {
            neighbor.declare(ms(0), attribute::LISTENER, stream, Some(state.number()));
        }
        let value = talker_value(&control());
        talker.declare(ms(0), attribute::TALKER_ADVERTISE, value, None);
        run(&mut talker, &mut neighbor, 0, 1000);
        (talker, neighbor)
    }

    /// The stream's talker declarations the neighbor registered, and
    /// whether it took the stream off at any point.
    fn talkers(heard: &[crate::Registration]) -> (Vec<TalkerDeclaration>, bool) {
        let mut declarations = Vec::new();
        let mut left = false;
        for registration in heard {
            match registration {
                crate::Registration::Joined {
                    attribute_type: attribute::TALKER_ADVERTISE,
                    value,
                    ..
                } => declarations.extend(decode_talker(
                    attribute::TALKER_ADVERTISE,
                    value,
                    Event::JoinIn,
                )),
                crate::Registration::Left {
                    attribute_type: attribute::TALKER_ADVERTISE,
                    ..
                } => left = true,
                _ => {}
            }
        }
        (declarations, left)
    }

    #[test]
    fn a_talker_beside_a_neighbor_without_class_c_falls_back_to_a() {
        let (mut talker, mut neighbor) = talker_beside(&A_AND_B, None);
        assert_eq!(
            registered_fallback(&talker, &control()),
            Some(Fallback::Boundary)
        );
        let (why, class_a) =
            fall_back(&mut talker, ms(1000), &control(), intervals::CLASS_C).expect("falls back");
        assert_eq!(why, Fallback::Boundary);
        assert_eq!((class_a.priority, class_a.max_interval_frames), (3, 1));
        let (declarations, left) = talkers(&run(&mut talker, &mut neighbor, 1000, 3000));
        assert_eq!(declarations.last().map(|d| d.priority), Some(3));
        assert!(!left, "the stream went off between classes");
        // In Class A it falls back no further.
        assert_eq!(registered_fallback(&talker, &class_a), None);
    }

    #[test]
    fn a_talker_whose_listeners_ask_failed_falls_back_to_a() {
        let asking = Some(ListenerState::AskingFailed);
        let (mut talker, mut neighbor) = talker_beside(&A_B_AND_C, asking);
        let (why, class_a) =
            fall_back(&mut talker, ms(1000), &control(), intervals::CLASS_C).expect("falls back");
        assert_eq!(why, Fallback::AskingFailed);
        let (declarations, left) = talkers(&run(&mut talker, &mut neighbor, 1000, 3000));
        let last = declarations.last().expect("declared again");
        assert_eq!(talker_value(last), talker_value(&class_a));
        assert!(!left);
    }

    #[test]
    fn a_talker_beside_a_neighbor_with_class_c_at_5_moves_to_it() {
        let with_c_at_5 = [Domain::CLASS_A, Domain::CLASS_B, C_AT_5];
        let (mut talker, mut neighbor) = talker_beside(&with_c_at_5, None);
        let (why, moved) =
            fall_back(&mut talker, ms(1000), &control(), intervals::CLASS_C).expect("moves");
        assert_eq!(why, Fallback::Domain(C_AT_5));
        assert_eq!(moved, control().in_domain(C_AT_5));
        let (declarations, left) = talkers(&run(&mut talker, &mut neighbor, 1000, 3000));
        let last = declarations.last().expect("declared again");
        assert_eq!(talker_value(last), talker_value(&moved));
        assert!(!left, "the stream went off between priorities");
        // At 5 it stays in Class C.
        assert_eq!(registered_fallback(&talker, &moved), None);
    }

    #[test]
    fn a_talker_falls_back_to_class_a_as_its_port_has_it() {
        let class_a_on_vlan_3 = Domain {
            vlan_id: 3,
            ..Domain::CLASS_A
        };
        let (mut talker, _) = talker_beside(&[class_a_on_vlan_3, Domain::CLASS_B], None);
        let (why, class_a) =
            fall_back(&mut talker, ms(1000), &control(), intervals::CLASS_C).expect("falls back");
        assert_eq!(why, Fallback::Boundary);
        assert_eq!((class_a.priority, class_a.vlan_id), (3, 3));
        assert_eq!(registered_fallback(&talker, &class_a), None);
    }

    #[test]
    fn a_talker_with_class_c_and_a_ready_listener_stays() {
        let ready = Some(ListenerState::Ready);
        let (mut talker, _) = talker_beside(&A_B_AND_C, ready);
        assert_eq!(registered_fallback(&talker, &control()), None);
        assert_eq!(
            fall_back(&mut talker, ms(1000), &control(), intervals::CLASS_C),
            None
        );
    }
}
