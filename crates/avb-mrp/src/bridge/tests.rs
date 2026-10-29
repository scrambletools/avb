//! Bridges running against end stations' participants and each other.

use alloc::vec::Vec;
use core::time::Duration;

use super::streams::failure;
use super::*;
use crate::msrp::TalkerFailure;

const BRIDGE_ID: u64 = 0x8000_fc06_8cff_fe14;
const OTHER_BRIDGE_ID: u64 = 0x8000_fc06_8cff_fe20;
const STREAM: u64 = 0x9c6b_0030_9a2b_0001;
const CLASS_A: u8 = 6;
const CLASS_B: u8 = 5;

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn gigabit() -> PortStatus {
    PortStatus::up(1000, true)
}

fn bridge_with(ports: &[(u32, PortStatus)]) -> Bridge {
    bridge_named(BRIDGE_ID, ports)
}

fn bridge_named(bridge_id: u64, ports: &[(u32, PortStatus)]) -> Bridge {
    let mut bridge = Bridge::new(Config::new(bridge_id), bridge_id);
    for (port, status) in ports {
        bridge.set_port(ms(0), PortId(*port), *status);
    }
    bridge
}

/// A class A stream of one frame of `max_frame_size` octets each
/// interval.
fn advertise(stream_id: u64, max_frame_size: u16) -> TalkerDeclaration {
    TalkerDeclaration {
        stream_id,
        destination: MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x12, stream_id as u8]),
        vlan_id: 2,
        max_frame_size,
        max_interval_frames: 1,
        priority: 3,
        rank: true,
        accumulated_latency: 1000,
        failure: None,
        event: Event::New,
    }
}

fn class_a_bandwidth(max_frame_size: u64) -> u64 {
    (max_frame_size + 43) * 8 * 8000
}

/// An end station on a bridge port, declaring both SR classes as
/// endpoints do.
struct Station {
    port: PortId,
    msrp: Participant,
    mvrp: Participant,
}

impl Station {
    fn new(port: u32, seed: u64) -> Self {
        let mut msrp = Participant::new(msrp::FORMAT, ms(0), seed).with_twins(msrp::twin);
        for domain in [Domain::CLASS_A, Domain::CLASS_B] {
            msrp.declare(ms(0), msrp::attribute::DOMAIN, domain.value(), None);
        }
        Station {
            port: PortId(port),
            msrp,
            mvrp: Participant::new(mvrp::FORMAT, ms(0), seed.rotate_left(9)),
        }
    }

    fn talk(&mut self, now: u64, declaration: &TalkerDeclaration) {
        self.msrp.declare(
            ms(now),
            msrp::attribute::TALKER_ADVERTISE,
            msrp::talker_value(declaration),
            None,
        );
    }

    fn listen(&mut self, now: u64, stream_id: u64, state: ListenerState) {
        self.msrp.declare(
            ms(now),
            msrp::attribute::LISTENER,
            stream_id.to_be_bytes().to_vec(),
            Some(state.number()),
        );
    }

    /// The talker it registers for `stream_id`, Advertise or Failed.
    fn talker(&self, stream_id: u64) -> Option<TalkerDeclaration> {
        self.msrp
            .registered()
            .filter(|(attribute_type, value, _)| {
                matches!(
                    *attribute_type,
                    msrp::attribute::TALKER_ADVERTISE | msrp::attribute::TALKER_FAILED
                ) && stream_of(value) == Some(stream_id)
            })
            .find_map(|(attribute_type, value, _)| {
                msrp::decode_talker(attribute_type, value, Event::JoinIn)
            })
    }

    /// The listener it registers for `stream_id`.
    fn listener(&self, stream_id: u64) -> Option<ListenerState> {
        self.msrp
            .registered()
            .find(|(attribute_type, value, _)| {
                *attribute_type == msrp::attribute::LISTENER && stream_of(value) == Some(stream_id)
            })
            .map(|(_, _, four_packed)| ListenerState::from_number(four_packed.unwrap_or(0)))
    }

    fn vlans(&self) -> Vec<u16> {
        self.mvrp
            .registered()
            .map(|(_, value, _)| u16::from_be_bytes([value[0], value[1]]))
            .collect()
    }

    fn tick(&mut self, now: Duration) {
        self.msrp.handle_timeout(now);
        self.mvrp.handle_timeout(now);
        while self.msrp.poll_event().is_some() {}
        while self.mvrp.poll_event().is_some() {}
    }

    fn hear(&mut self, now: Duration, transmit: &Transmit) {
        match transmit.application {
            Application::Msrp => self.msrp.handle_pdu(now, &transmit.pdu),
            Application::Mvrp => self.mvrp.handle_pdu(now, &transmit.pdu),
        }
    }

    fn send(&mut self, now: Duration, bridge: &mut Bridge) {
        while let Some(pdu) = self.msrp.poll_transmit() {
            bridge.handle_pdu(now, self.port, Application::Msrp, &pdu);
        }
        while let Some(pdu) = self.mvrp.poll_transmit() {
            bridge.handle_pdu(now, self.port, Application::Mvrp, &pdu);
        }
    }
}

/// Runs the bridge and its stations from `from` to `to` ms, handing
/// MRPDUs between them, and returns the bridge's changes.
fn run(bridge: &mut Bridge, stations: &mut [&mut Station], from: u64, to: u64) -> Vec<Change> {
    let mut changes = Vec::new();
    for now in (from..=to).step_by(10).map(ms) {
        bridge.handle_timeout(now);
        for station in stations.iter_mut() {
            station.tick(now);
        }
        while let Some(transmit) = bridge.poll_transmit() {
            if let Some(station) = stations
                .iter_mut()
                .find(|station| station.port == transmit.port)
            {
                station.hear(now, &transmit);
            }
        }
        for station in stations.iter_mut() {
            station.send(now, bridge);
        }
        changes.extend(core::iter::from_fn(|| bridge.poll_change()));
    }
    changes
}

/// The changes to forwarding and idle slopes, leaving out the others.
fn reservation_changes(changes: Vec<Change>) -> Vec<Change> {
    changes
        .into_iter()
        .filter(|change| matches!(change, Change::Forwarding { .. } | Change::IdleSlope { .. }))
        .collect()
}

fn forwarding(port: u32, declaration: &TalkerDeclaration, forwarding: bool) -> Change {
    Change::Forwarding {
        port: PortId(port),
        stream_id: declaration.stream_id,
        destination: declaration.destination,
        vlan_id: declaration.vlan_id,
        forwarding,
    }
}

fn idle_slope(port: u32, bits_per_second: u64) -> Change {
    Change::IdleSlope {
        port: PortId(port),
        class_id: CLASS_A,
        bits_per_second,
    }
}

/// A talker on port 1 and a listener on port 2, reserved.
fn reserved() -> (Bridge, Station, Station, TalkerDeclaration) {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut talker = Station::new(1, 1);
    let mut listener = Station::new(2, 2);
    let declaration = advertise(STREAM, 224);
    talker.talk(0, &declaration);
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 1000);
    listener.listen(1000, STREAM, ListenerState::Ready);
    run(&mut bridge, &mut [&mut talker, &mut listener], 1000, 2000);
    (bridge, talker, listener, declaration)
}

#[test]
fn a_stream_reserves_through_the_bridge() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut talker = Station::new(1, 1);
    let mut listener = Station::new(2, 2);
    let declaration = advertise(STREAM, 224);
    talker.talk(0, &declaration);
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 1000);
    let heard = listener.talker(STREAM).expect("the talker");
    assert_eq!(heard.failure, None);
    assert_eq!(heard.accumulated_latency, 1000 + gigabit().latency);
    assert!(!bridge.boundary(PortId(2), CLASS_A));

    listener.listen(1000, STREAM, ListenerState::Ready);
    let changes = run(&mut bridge, &mut [&mut talker, &mut listener], 1000, 2000);
    assert_eq!(talker.listener(STREAM), Some(ListenerState::Ready));
    let bandwidth = class_a_bandwidth(224);
    // The queue gets the bandwidth before the stream goes out.
    assert_eq!(
        reservation_changes(changes),
        [idle_slope(2, bandwidth), forwarding(2, &declaration, true)]
    );
    assert_eq!(bridge.idle_slope(PortId(2), CLASS_A), bandwidth);
    assert_eq!(bridge.reservations().count(), 1);
    let talkers: Vec<(PortId, u64, bool)> = bridge
        .talkers()
        .map(|(port, heard, echo)| (port, heard.stream_id, echo))
        .collect();
    assert_eq!(talkers, [(PortId(1), STREAM, false)]);
    let listeners: Vec<(PortId, u64, ListenerState)> = bridge.listeners().collect();
    assert_eq!(listeners, [(PortId(2), STREAM, ListenerState::Ready)]);
    assert_eq!(bridge.domains(PortId(2)).count(), 2);
}

#[test]
fn a_port_whose_neighbor_is_not_as_capable_fails_the_talker() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, PortStatus::up(1000, false))]);
    let mut talker = Station::new(1, 3);
    let mut listener = Station::new(2, 4);
    talker.talk(0, &advertise(STREAM, 224));
    // Past the wait for asCapable a port that has just come up makes.
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 7000);
    assert!(bridge.boundary(PortId(2), CLASS_A));
    let heard = listener.talker(STREAM).expect("the talker");
    assert_eq!(
        heard.failure,
        Some(TalkerFailure {
            bridge_id: BRIDGE_ID,
            code: failure::NOT_AVB_CAPABLE,
        })
    );
    // A listener ready all the same reaches the talker as failed.
    listener.listen(7000, STREAM, ListenerState::Ready);
    run(&mut bridge, &mut [&mut talker, &mut listener], 7000, 8000);
    assert_eq!(talker.listener(STREAM), Some(ListenerState::AskingFailed));
    assert_eq!(bridge.reservations().count(), 0);
}

/// A port that has just come up, whose link gPTP counts asCapable a few
/// seconds on, declares the talker to its neighbor only then, not as a
/// Talker Failed first.
#[test]
fn a_port_coming_up_fails_no_talker_before_it_is_as_capable() {
    let mut bridge = bridge_with(&[(1, gigabit())]);
    let mut talker = Station::new(1, 25);
    talker.talk(0, &advertise(STREAM, 224));
    run(&mut bridge, &mut [&mut talker], 0, 1000);
    bridge.set_port(ms(1000), PortId(2), PortStatus::up(1000, false));
    let mut listener = Station::new(2, 26);
    let mut heard = Vec::new();
    for at in (1000..6000).step_by(10) {
        if at == 4000 {
            bridge.set_port(ms(at), PortId(2), gigabit());
        }
        run(&mut bridge, &mut [&mut talker, &mut listener], at, at);
        heard.push(
            listener
                .talker(STREAM)
                .map(|talker| talker.failure.is_some()),
        );
    }
    assert!(!heard.contains(&Some(true)), "a Talker Failed came first");
    assert_eq!(heard.last(), Some(&Some(false)));
}

#[test]
fn a_neighbor_with_another_priority_is_a_boundary() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut talker = Station::new(1, 5);
    let mut listener = Station::new(2, 6);
    let other = Domain {
        class_id: CLASS_A,
        priority: 4,
        vlan_id: 2,
    };
    listener
        .msrp
        .declare(ms(0), msrp::attribute::DOMAIN, other.value(), None);
    talker.talk(0, &advertise(STREAM, 224));
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 2000);
    assert!(bridge.boundary(PortId(2), CLASS_A));
    assert!(!bridge.boundary(PortId(2), CLASS_B));
    let code = listener
        .talker(STREAM)
        .and_then(|heard| heard.failure)
        .map(|failure| failure.code);
    assert_eq!(code, Some(failure::NOT_AVB_CAPABLE));
}

#[test]
fn a_stream_beyond_the_bandwidth_fails_and_the_older_keeps_its_place() {
    let slow = PortStatus::up(100, true);
    let mut bridge = bridge_with(&[(1, gigabit()), (2, slow)]);
    let mut talker = Station::new(1, 7);
    let mut listener = Station::new(2, 8);
    // Each takes about 67 Mb/s: one fits in 75% of 100 Mb/s, two do not.
    let older = advertise(STREAM + 1, 1000);
    let newer = advertise(STREAM, 1000);
    talker.talk(0, &older);
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 1000);
    listener.listen(1000, older.stream_id, ListenerState::Ready);
    run(&mut bridge, &mut [&mut talker, &mut listener], 1000, 2000);
    // The newer stream's lower stream ID does not take the place over.
    talker.talk(2000, &newer);
    run(&mut bridge, &mut [&mut talker, &mut listener], 2000, 3000);
    listener.listen(3000, newer.stream_id, ListenerState::Ready);
    run(&mut bridge, &mut [&mut talker, &mut listener], 3000, 4000);

    let reserved: Vec<u64> = bridge
        .reservations()
        .map(|reservation| reservation.stream_id)
        .collect();
    assert_eq!(reserved, [older.stream_id]);
    let code = listener
        .talker(newer.stream_id)
        .and_then(|heard| heard.failure)
        .map(|failure| failure.code);
    assert_eq!(code, Some(failure::INSUFFICIENT_BANDWIDTH));
    assert_eq!(
        talker.listener(newer.stream_id),
        Some(ListenerState::AskingFailed)
    );
    assert_eq!(talker.listener(older.stream_id), Some(ListenerState::Ready));
    assert_eq!(
        bridge.idle_slope(PortId(2), CLASS_A),
        class_a_bandwidth(1000)
    );
}

#[test]
fn listeners_on_two_ports_merge_toward_the_talker() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit()), (3, gigabit())]);
    let mut talker = Station::new(1, 9);
    let mut ready = Station::new(2, 10);
    let mut asking = Station::new(3, 11);
    talker.talk(0, &advertise(STREAM, 224));
    run(
        &mut bridge,
        &mut [&mut talker, &mut ready, &mut asking],
        0,
        1000,
    );
    ready.listen(1000, STREAM, ListenerState::Ready);
    asking.listen(1000, STREAM, ListenerState::AskingFailed);
    run(
        &mut bridge,
        &mut [&mut talker, &mut ready, &mut asking],
        1000,
        2000,
    );
    assert_eq!(talker.listener(STREAM), Some(ListenerState::ReadyFailed));
    let ports: Vec<PortId> = bridge
        .reservations()
        .map(|reservation| reservation.port)
        .collect();
    assert_eq!(ports, [PortId(2)]);
}

#[test]
fn a_listener_port_going_down_stops_the_stream_before_its_bandwidth() {
    let (mut bridge, mut talker, mut listener, declaration) = reserved();
    bridge.set_port(ms(2000), PortId(2), PortStatus::down());
    let changes: Vec<Change> = core::iter::from_fn(|| bridge.poll_change()).collect();
    assert_eq!(
        reservation_changes(changes),
        [forwarding(2, &declaration, false), idle_slope(2, 0)]
    );
    // The talker loses its listener within a leave time.
    run(&mut bridge, &mut [&mut talker, &mut listener], 2000, 3500);
    assert_eq!(talker.listener(STREAM), None);
}

#[test]
fn a_port_coming_back_up_reserves_again() {
    let (mut bridge, mut talker, mut listener, declaration) = reserved();
    bridge.set_port(ms(2000), PortId(2), PortStatus::down());
    run(&mut bridge, &mut [&mut talker], 2000, 2500);
    bridge.set_port(ms(2500), PortId(2), gigabit());
    // The listener's station hears nothing for a while, as a link
    // coming up again; its LeaveAll or periodic join brings it back.
    let changes = run(&mut bridge, &mut [&mut talker, &mut listener], 2500, 20_000);
    assert!(
        reservation_changes(changes).contains(&forwarding(2, &declaration, true)),
        "the stream forwards again"
    );
    assert_eq!(talker.listener(STREAM), Some(ListenerState::Ready));
}

#[test]
fn a_talker_leaving_takes_its_stream_down() {
    let (mut bridge, mut talker, mut listener, declaration) = reserved();
    talker.msrp.withdraw(
        ms(2000),
        msrp::attribute::TALKER_ADVERTISE,
        &STREAM.to_be_bytes(),
    );
    let changes = run(&mut bridge, &mut [&mut talker, &mut listener], 2000, 4000);
    assert_eq!(
        reservation_changes(changes),
        [forwarding(2, &declaration, false), idle_slope(2, 0)]
    );
    assert_eq!(listener.talker(STREAM), None);
    assert_eq!(bridge.reservations().count(), 0);
}

/// A neighboring bridge fails a stream while the talker's port comes up
/// and back once it has, its Advertise never withdrawn between, as a
/// PreSonus switch does for a talker that has just booted: the stream
/// comes back here, rather than staying lost behind the stale Advertise
/// registration.
#[test]
fn a_stream_failed_and_back_upstream_comes_back() {
    let (mut bridge, mut upstream, mut listener, declaration) = reserved();
    let mut failed = declaration;
    failed.failure = Some(TalkerFailure {
        bridge_id: OTHER_BRIDGE_ID,
        code: failure::NOT_AVB_CAPABLE,
    });
    upstream.msrp.declare(
        ms(2000),
        msrp::attribute::TALKER_FAILED,
        msrp::talker_value(&failed),
        None,
    );
    run(&mut bridge, &mut [&mut upstream, &mut listener], 2000, 3000);
    let heard = listener.talker(STREAM).expect("the talker");
    assert_eq!(
        heard.failure.map(|failure| failure.code),
        Some(failure::NOT_AVB_CAPABLE)
    );
    assert_eq!(bridge.reservations().count(), 0);

    upstream.msrp.withdraw(
        ms(3000),
        msrp::attribute::TALKER_FAILED,
        &STREAM.to_be_bytes(),
    );
    run(&mut bridge, &mut [&mut upstream, &mut listener], 3000, 6000);
    let talkers: Vec<(PortId, Option<TalkerFailure>)> = bridge
        .talkers()
        .map(|(port, heard, _)| (port, heard.failure))
        .collect();
    assert_eq!(talkers, [(PortId(1), None)]);
    assert_eq!(
        listener.talker(STREAM).map(|heard| heard.failure),
        Some(None)
    );
    assert_eq!(bridge.reservations().count(), 1);
}

/// A port that comes up sends LeaveAll with its first declarations, so a
/// neighbor still registering an earlier participant's, as when the
/// bridge's daemon restarts, declares its own again at once.
#[test]
fn a_port_coming_up_asks_its_neighbor_to_declare_again_at_once() {
    for application in [Application::Msrp, Application::Mvrp] {
        let mut bridge = bridge_with(&[(1, gigabit())]);
        bridge.handle_timeout(ms(250));
        let first = core::iter::from_fn(|| bridge.poll_transmit())
            .find(|transmit| transmit.application == application)
            .expect("a first MRPDU within JoinTime");
        let format = match application {
            Application::Msrp => msrp::FORMAT,
            Application::Mvrp => mvrp::FORMAT,
        };
        assert!(
            !crate::mrpdu::decode(&first.pdu, &format)
                .leave_all
                .is_empty(),
            "{application:?}"
        );
    }
}

/// A port that comes up declares no talker to its neighbor before the
/// neighbor's Domain declarations could arrive, rather than a Talker
/// Failed for the boundary it is in that moment, which would stop a
/// stream the neighbor's side listens to.
#[test]
fn a_port_coming_up_fails_no_talker_before_its_neighbors_domains() {
    let mut bridge = bridge_with(&[(1, gigabit())]);
    let mut talker = Station::new(1, 21);
    talker.talk(0, &advertise(STREAM, 224));
    run(&mut bridge, &mut [&mut talker], 0, 1000);
    bridge.set_port(ms(1000), PortId(2), gigabit());
    let mut listener = Station::new(2, 22);
    let mut heard = Vec::new();
    for at in (1000..3000).step_by(10) {
        run(&mut bridge, &mut [&mut talker, &mut listener], at, at);
        heard.push(
            listener
                .talker(STREAM)
                .map(|talker| talker.failure.is_some()),
        );
    }
    assert!(!heard.contains(&Some(true)), "a Talker Failed came first");
    assert_eq!(heard.last(), Some(&Some(false)));
}

/// A neighbor that declares no Domain at all is a boundary once the wait
/// for it is over, as it was without the wait.
#[test]
fn a_port_whose_neighbor_declares_no_domain_fails_the_talker_after_the_wait() {
    let mut bridge = bridge_with(&[(1, gigabit())]);
    let mut talker = Station::new(1, 23);
    talker.talk(0, &advertise(STREAM, 224));
    run(&mut bridge, &mut [&mut talker], 0, 1000);
    bridge.set_port(ms(1000), PortId(2), gigabit());
    let mut listener = Station::new(2, 24);
    for class in [CLASS_A, CLASS_B] {
        listener
            .msrp
            .withdraw(ms(1000), msrp::attribute::DOMAIN, &[class]);
    }
    run(&mut bridge, &mut [&mut talker, &mut listener], 1000, 8000);
    let heard = listener.talker(STREAM).expect("the talker, failed");
    assert_eq!(
        heard.failure.map(|failure| failure.code),
        Some(failure::NOT_AVB_CAPABLE)
    );
}

#[test]
fn a_listener_echoing_the_talker_back_is_no_second_talker() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut talker = Station::new(1, 18);
    let mut listener = Station::new(2, 19);
    let declaration = advertise(STREAM, 224);
    talker.talk(0, &declaration);
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 1000);
    // It declares back what it registered, with its own latency and
    // destination, as some endpoints do.
    let mut echo = listener.talker(STREAM).expect("the talker");
    echo.accumulated_latency += 5000;
    echo.destination = MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x99, 0x99]);
    listener.talk(1000, &echo);
    listener.listen(1000, STREAM, ListenerState::Ready);
    let changes = run(&mut bridge, &mut [&mut talker, &mut listener], 1000, 2000);
    assert_eq!(
        listener.talker(STREAM).and_then(|heard| heard.failure),
        None
    );
    assert_eq!(talker.listener(STREAM), Some(ListenerState::Ready));
    assert_eq!(talker.talker(STREAM), None, "the echo goes no further");
    assert!(reservation_changes(changes).contains(&forwarding(2, &declaration, true)));
    // With the talker gone, an echo still declared does not take its
    // place.
    talker.msrp.withdraw(
        ms(2000),
        msrp::attribute::TALKER_ADVERTISE,
        &STREAM.to_be_bytes(),
    );
    run(&mut bridge, &mut [&mut talker, &mut listener], 2000, 4000);
    assert_eq!(talker.talker(STREAM), None);
    assert_eq!(bridge.reservations().count(), 0);
}

#[test]
fn a_priority_outside_the_sr_classes_fails_the_talker() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut talker = Station::new(1, 12);
    let mut listener = Station::new(2, 13);
    let mut declaration = advertise(STREAM, 224);
    declaration.priority = 1;
    talker.talk(0, &declaration);
    run(&mut bridge, &mut [&mut talker, &mut listener], 0, 1000);
    let code = listener
        .talker(STREAM)
        .and_then(|heard| heard.failure)
        .map(|failure| failure.code);
    assert_eq!(code, Some(failure::NOT_SR_CLASS_PRIORITY));
}

#[test]
fn latency_accumulates_through_two_bridges() {
    let near_port = PortStatus::up(1000, true);
    let mut far_port = PortStatus::up(100, true);
    far_port.latency = 150_000;
    let mut near = bridge_named(BRIDGE_ID, &[(1, gigabit()), (2, near_port)]);
    let mut far = bridge_named(OTHER_BRIDGE_ID, &[(1, gigabit()), (2, far_port)]);
    let mut talker = Station::new(1, 14);
    let mut listener = Station::new(2, 15);
    let declaration = advertise(STREAM, 224);
    talker.talk(0, &declaration);
    let mut step = |from: u64, to: u64, talker: &mut Station, listener: &mut Station| {
        for now in (from..=to).step_by(10).map(ms) {
            near.handle_timeout(now);
            far.handle_timeout(now);
            talker.tick(now);
            listener.tick(now);
            while let Some(transmit) = near.poll_transmit() {
                match transmit.port {
                    PortId(1) => talker.hear(now, &transmit),
                    _ => far.handle_pdu(now, PortId(1), transmit.application, &transmit.pdu),
                }
            }
            while let Some(transmit) = far.poll_transmit() {
                match transmit.port {
                    PortId(2) => listener.hear(now, &transmit),
                    _ => near.handle_pdu(now, PortId(2), transmit.application, &transmit.pdu),
                }
            }
            talker.send(now, &mut near);
            listener.send(now, &mut far);
        }
    };
    step(0, 1500, &mut talker, &mut listener);
    let heard = listener.talker(STREAM).expect("the talker");
    assert_eq!(heard.failure, None);
    assert_eq!(
        heard.accumulated_latency,
        1000 + near_port.latency + far_port.latency
    );
    listener.listen(1500, STREAM, ListenerState::Ready);
    step(1500, 3000, &mut talker, &mut listener);
    assert_eq!(talker.listener(STREAM), Some(ListenerState::Ready));
    let near_ports: Vec<PortId> = near
        .reservations()
        .map(|reservation| reservation.port)
        .collect();
    let far_ports: Vec<PortId> = far
        .reservations()
        .map(|reservation| reservation.port)
        .collect();
    assert_eq!((near_ports, far_ports), (vec![PortId(2)], vec![PortId(2)]));
}

#[test]
fn vlans_propagate_and_static_ones_are_declared_everywhere() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    let mut member = Station::new(1, 16);
    let mut other = Station::new(2, 17);
    member.mvrp.declare(ms(0), mvrp::VID, mvrp::value(10), None);
    let changes = run(&mut bridge, &mut [&mut member, &mut other], 0, 1000);
    assert!(changes.contains(&Change::Vlan {
        port: PortId(1),
        vlan_id: 10,
        registered: true,
    }));
    assert_eq!(other.vlans(), [2, 10]);
    assert_eq!(member.vlans(), [2]);
    assert_eq!(bridge.vlans(PortId(1)).collect::<Vec<u16>>(), [10]);

    member.mvrp.withdraw(ms(1000), mvrp::VID, &mvrp::value(10));
    let changes = run(&mut bridge, &mut [&mut member, &mut other], 1000, 3000);
    assert!(changes.contains(&Change::Vlan {
        port: PortId(1),
        vlan_id: 10,
        registered: false,
    }));
    assert_eq!(other.vlans(), [2]);
}

#[test]
fn malformed_mrpdus_change_nothing() {
    let mut bridge = bridge_with(&[(1, gigabit()), (2, gigabit())]);
    for pdu in [
        &[][..],
        &[0],
        &[0, 1, 25, 0xff, 0xff],
        &[0, 3, 8, 0, 4, 0, 1],
        &[0, 1, 25, 0, 9, 0, 1, 1, 2, 3],
    ] {
        bridge.handle_pdu(ms(0), PortId(1), Application::Msrp, pdu);
        bridge.handle_pdu(ms(0), PortId(1), Application::Mvrp, pdu);
        bridge.handle_pdu(ms(0), PortId(9), Application::Msrp, pdu);
    }
    assert_eq!(bridge.reservations().count(), 0);
}
