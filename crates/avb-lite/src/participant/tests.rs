use alloc::vec;

use super::*;
use crate::admission::FAILURE_RESOURCES;

const STREAM: u64 = 0xa010_a3a5_1726_7000;
const GROUP: MacAddress = MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x63, 0x26]);

fn mac(last: u8) -> MacAddress {
    MacAddress([0x02, 0, 0, 0, 0, last])
}

fn entity(last: u8) -> EntityId {
    EntityId(0x0200_00ff_0000_0000 | u64::from(last))
}

/// A stream as AVB declares it: to its multicast address, in VLAN 2 at
/// class A's priority.
fn stream() -> TalkerDeclaration {
    TalkerDeclaration {
        stream_id: STREAM,
        destination: GROUP,
        vlan_id: 2,
        max_frame_size: 72,
        max_interval_frames: 1,
        priority: 3,
        rank: true,
        accumulated_latency: 125_000,
        failure: None,
        event: MrpEvent::New,
    }
}

/// Participants on one switch, each with its address, handed each other's
/// commands as they send them: to all, or to one alone.
struct Link {
    now: Duration,
    ends: Vec<(MacAddress, Participant, Vec<Event>)>,
    /// Ends whose commands go nowhere, as a computer gone from the link.
    cut: Vec<usize>,
}

impl Link {
    fn new(count: u8) -> Self {
        Link {
            now: Duration::ZERO,
            ends: (1..=count)
                .map(|last| {
                    (
                        mac(last),
                        Participant::new(mac(last), Duration::ZERO),
                        Vec::new(),
                    )
                })
                .collect(),
            cut: Vec::new(),
        }
    }

    fn end(&mut self, index: usize) -> &mut Participant {
        &mut self.ends[index].1
    }

    /// Runs everyone for `millis`, in steps of 10 ms.
    fn run(&mut self, millis: u64) {
        let until = self.now + Duration::from_millis(millis);
        while self.now < until {
            self.now += Duration::from_millis(10);
            let now = self.now;
            for (_, participant, _) in &mut self.ends {
                if participant.poll_timeout() <= now {
                    participant.handle_timeout(now);
                }
            }
            self.deliver();
        }
    }

    /// Hands every command sent to those it goes to, and keeps the events.
    fn deliver(&mut self) {
        let now = self.now;
        loop {
            let mut sent = Vec::new();
            for (index, (source, participant, events)) in self.ends.iter_mut().enumerate() {
                while let Some((destination, pdu)) = participant.poll_transmit() {
                    if !self.cut.contains(&index) {
                        sent.push((index, *source, destination, pdu));
                    }
                }
                events.extend(core::iter::from_fn(|| participant.poll_event()));
            }
            if sent.is_empty() {
                return;
            }
            for (from, source, destination, pdu) in sent {
                for (index, (address, participant, _)) in self.ends.iter_mut().enumerate() {
                    if index == from {
                        continue;
                    }
                    if destination == BROADCAST {
                        participant.handle_pdu(now, source, true, &pdu);
                    } else if destination == *address {
                        participant.handle_pdu(now, source, false, &pdu);
                    }
                }
            }
        }
    }

    /// The events `index` told since last asked.
    fn events(&mut self, index: usize) -> Vec<Event> {
        core::mem::take(&mut self.ends[index].2)
    }

    /// The last Copies `index` told of the stream.
    fn copies(&mut self, index: usize) -> Option<(Vec<MacAddress>, usize, Option<ListenerState>)> {
        self.events(index)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                Event::Copies {
                    stream_id: STREAM,
                    copies,
                    ready,
                    state,
                } => Some((copies, ready, state)),
                _ => None,
            })
    }

    /// End `index` listens to the stream, as its listener would once told
    /// of its talker, and the link runs a moment.
    fn listen(&mut self, index: usize) -> Option<TalkerDeclaration> {
        let now = self.now;
        let heard = self
            .end(index)
            .listen(now, STREAM, Some(entity(index as u8)));
        self.deliver();
        self.run(50);
        heard
    }
}

/// A listener hears the talker declare, unicast, in the media VLAN at
/// AVB Lite's priority, and asks: the talker sends it a copy.
#[test]
fn a_listener_finds_its_talker_and_gets_a_copy() {
    let mut link = Link::new(2);
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.deliver();
    assert_eq!(link.copies(0), Some((vec![], 0, None)));
    assert!(link.events(1).contains(&Event::Talker(STREAM)));
    let heard = link.listen(1).expect("the talker declared");
    assert_eq!(heard.destination, UNICAST);
    assert_eq!((heard.vlan_id, heard.priority), (MEDIA_VLAN, PRIORITY));
    assert_eq!(
        link.copies(0),
        Some((vec![mac(2)], 1, Some(ListenerState::Ready)))
    );
    assert_eq!(link.end(0).copies(STREAM), Some(&[mac(2)][..]));
    assert_eq!(link.end(0).committed(), stream().bandwidth(8000));
    // It stops listening: LeaveTime later its copy goes.
    let now = link.now;
    assert_eq!(link.end(1).listen(now, STREAM, None), Some(heard));
    link.deliver();
    link.run(LEAVE_TIME.as_millis() as u64 + 50);
    assert_eq!(link.copies(0), Some((vec![], 0, None)));
}

/// Beyond the fan-out the newest listener is refused, by a Talker Failed
/// sent to it alone, which outranks the talker's declaration to all; it
/// is served once a place frees, and the refusal is withdrawn.
#[test]
fn beyond_the_fan_out_a_listener_is_refused_alone() {
    let mut link = Link::new(3);
    let now = link.now;
    link.end(0).set_fanout(now, 1);
    link.end(0).declare_talker(now, entity(1), stream());
    link.deliver();
    link.listen(1);
    let heard = link.listen(2).expect("declared");
    assert!(heard.failure.is_none());
    assert_eq!(
        link.copies(0),
        Some((vec![mac(2)], 1, Some(ListenerState::ReadyFailed)))
    );
    let (refused, from) = link.end(2).talker(STREAM).expect("refused");
    assert_eq!(from, mac(1));
    let failure = refused.failure.expect("Talker Failed");
    assert_eq!(failure.code, FAILURE_RESOURCES);
    assert_eq!(failure.bridge_id, 0x0000_0200_0000_0001);
    assert!(link.events(2).contains(&Event::Talker(STREAM)));
    // Asked again, it asks and fails.
    link.listen(2);
    // The first goes: LeaveTime later the second has its place, and the
    // refusal is withdrawn, which lasts LeaveTime more.
    let now = link.now;
    link.end(1).listen(now, STREAM, None);
    link.deliver();
    link.run(LEAVE_TIME.as_millis() as u64 * 2 + 100);
    assert!(
        link.end(2)
            .talker(STREAM)
            .is_some_and(|(declared, _)| declared.failure.is_none())
    );
    link.listen(2);
    assert_eq!(
        link.copies(0),
        Some((vec![mac(3)], 1, Some(ListenerState::Ready)))
    );
}

/// Where escalation is allowed, more listeners than the fan-out take the
/// stream's multicast address: its declaration says so at once, and its
/// frames go there JoinTime later.
#[test]
fn beyond_the_fan_out_the_stream_escalates_where_allowed() {
    let mut link = Link::new(3);
    let now = link.now;
    link.end(0).set_fanout(now, 1);
    link.end(0).set_escalation(now, true);
    link.end(0).declare_talker(now, entity(1), stream());
    link.deliver();
    link.listen(1);
    link.listen(2);
    assert_eq!(
        link.end(2)
            .talker(STREAM)
            .map(|(declared, _)| declared.destination),
        Some(GROUP)
    );
    link.run(JOIN_TIME.as_millis() as u64);
    assert_eq!(link.end(0).copies(STREAM), Some(&[GROUP][..]));
    // Disallowed again, it goes back to one copy, its frames first.
    let now = link.now;
    link.end(0).set_escalation(now, false);
    assert_eq!(link.end(0).copies(STREAM), Some(&[mac(2)][..]));
    link.deliver();
    assert_eq!(
        link.end(1)
            .talker(STREAM)
            .map(|(declared, _)| declared.destination),
        Some(UNICAST)
    );
}

/// A talker heard from no more is gone 16 s after its last declaration,
/// though it refreshes every 10 to 15 s while it is there.
#[test]
fn a_talker_gone_quiet_ages_out() {
    let mut link = Link::new(2);
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.deliver();
    link.run(40_000);
    assert!(link.end(1).talker(STREAM).is_some(), "refreshed");
    link.events(1);
    link.cut.push(0);
    link.run(AGE_OUT.as_millis() as u64 + 1000);
    assert!(link.end(1).talker(STREAM).is_none());
    assert!(link.events(1).contains(&Event::Talker(STREAM)));
}

/// A listener here takes a talker here's stream as one on the link would:
/// a copy to this computer's own address, whose frames the listener takes
/// as they go out.
#[test]
fn a_listener_here_gets_a_copy_to_this_computer() {
    let mut link = Link::new(1);
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.run(50);
    assert_eq!(
        link.end(0).talker(STREAM).map(|(_, from)| from),
        Some(mac(1))
    );
    link.listen(0);
    assert_eq!(
        link.copies(0),
        Some((vec![mac(1)], 1, Some(ListenerState::Ready)))
    );
}

/// Each declaration goes out twice, JoinTime apart, a newer one of the
/// same superseding the second send, and again on each refresh.
#[test]
fn declarations_go_twice_and_again_on_refresh() {
    let mut participant = Participant::new(mac(1), Duration::ZERO);
    participant.declare_talker(Duration::ZERO, entity(1), stream());
    let sent =
        |participant: &mut Participant| core::iter::from_fn(|| participant.poll_transmit()).count();
    assert_eq!(sent(&mut participant), 1);
    participant.handle_timeout(JOIN_TIME);
    assert_eq!(sent(&mut participant), 1);
    // It takes what it sent to all itself, at once.
    assert_eq!(participant.poll_timeout(), Duration::ZERO);
    participant.handle_timeout(JOIN_TIME);
    let refresh = participant.poll_timeout();
    assert!(refresh >= LEAVE_ALL_TIME && refresh < LEAVE_ALL_TIME * 3 / 2);
    participant.handle_timeout(refresh);
    assert_eq!(sent(&mut participant), 1);
    // Withdrawn before its second send, the Lv alone goes.
    participant.withdraw_talker(refresh, STREAM);
    assert_eq!(sent(&mut participant), 1);
    participant.handle_timeout(refresh + JOIN_TIME);
    assert_eq!(sent(&mut participant), 0);
}

/// The link's room bounds the copies: past 75% of it a listener is
/// refused for bandwidth; and this computer's own link refuses a stream
/// it has no room for beside the others it takes.
#[test]
fn the_links_room_bounds_the_copies_both_ways() {
    let mut link = Link::new(3);
    let now = link.now;
    let per_copy = stream().bandwidth(8000);
    // Room for one copy of 7.3 Mb/s at 10 Mb/s, 7.5 of it AVB Lite's.
    link.end(0).set_link_speed(now, 10);
    link.end(0).declare_talker(now, entity(1), stream());
    link.deliver();
    assert!(per_copy < 7_500_000 && per_copy * 2 > 7_500_000);
    link.listen(1);
    link.listen(2);
    let (refused, _) = link.end(2).talker(STREAM).expect("refused");
    assert_eq!(
        refused.failure.map(|failure| failure.code),
        Some(FAILURE_BANDWIDTH)
    );

    // A listener on a 10 Mb/s link takes one such stream, not two.
    let mut link = Link::new(3);
    let mut other = stream();
    other.stream_id = STREAM + 1;
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.end(1).declare_talker(Duration::ZERO, entity(2), other);
    link.deliver();
    link.end(2).set_link_speed(Duration::ZERO, 10);
    link.listen(2);
    let now = link.now;
    let full = link.end(2).listen(now, other.stream_id, Some(entity(3)));
    assert_eq!(
        full.and_then(|declared| declared.failure)
            .map(|failure| failure.code),
        Some(FAILURE_BANDWIDTH)
    );
}

/// Leaving AVB Lite withdraws the talkers and listeners here: the others
/// let them go LeaveTime later.
#[test]
fn leaving_withdraws_everything() {
    let mut link = Link::new(2);
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.deliver();
    link.listen(1);
    link.copies(0);
    link.end(1).leave();
    link.deliver();
    link.run(LEAVE_TIME.as_millis() as u64 + 50);
    assert_eq!(link.copies(0), Some((vec![], 0, None)));
    link.end(0).leave();
    link.deliver();
    link.run(LEAVE_TIME.as_millis() as u64 + 50);
    assert!(link.end(1).talker(STREAM).is_none());
}

/// What comes back from the wire with this computer's own address is its
/// own: a refusal it sent another listener does not refuse its own.
#[test]
fn its_own_commands_heard_back_are_its_own() {
    let mut link = Link::new(3);
    let now = link.now;
    link.end(0).set_fanout(now, 1);
    link.end(0).declare_talker(now, entity(1), stream());
    link.deliver();
    link.listen(1);
    link.listen(2);
    // The talker's refusal of the second, as its own socket hears it go.
    let declared = link.end(0).declared(STREAM).unwrap();
    let refusal = link.end(0).refused(declared, FAILURE_RESOURCES);
    let pdu =
        cvu::cvu_command(entity(1), 99, &cvu::talker_message(&refusal, MrpEvent::New)).unwrap();
    let now = link.now;
    link.end(0).handle_pdu(now, mac(1), false, &pdu);
    assert!(
        link.end(0)
            .talker(STREAM)
            .is_some_and(|(declared, _)| declared.failure.is_none())
    );
}

/// A talker here withdrawn and declared again, as its format changes,
/// keeps its listeners, whose Lv never came: its copies come back at
/// once.
#[test]
fn a_talker_declared_again_keeps_its_listeners() {
    let mut link = Link::new(2);
    link.end(0)
        .declare_talker(Duration::ZERO, entity(1), stream());
    link.deliver();
    link.listen(1);
    link.copies(0);
    let now = link.now;
    link.end(0).withdraw_talker(now, STREAM);
    let mut changed = stream();
    changed.max_frame_size = 224;
    link.end(0).declare_talker(now, entity(1), changed);
    assert_eq!(link.end(0).copies(STREAM), Some(&[mac(2)][..]));
    // Declared again as it is, with another event, it is no change.
    link.deliver();
    link.events(0);
    let joined = TalkerDeclaration {
        event: MrpEvent::JoinIn,
        ..changed
    };
    link.end(0).declare_talker(now, entity(1), joined);
    assert!(link.end(0).poll_transmit().is_none());
    assert!(link.events(0).is_empty());
}
