//! Whom a talker serves in AVB Lite (profile 6): a unicast copy to each
//! listener ready for its stream, within its unicast fan-out and 75% of
//! the link, or, where escalation is allowed and they are more, its
//! multicast address once; the others it refuses, each alone.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use atdecc::MacAddress;
use avb_mrp::msrp::ListenerState;

/// MSRP's failure codes a talker refuses a listener with, in a Talker
/// Failed's FailureCode (IEEE 802.1Q-2022, 35.2.2.8.7): no room on the
/// link for another copy, and no place within the unicast fan-out.
pub const FAILURE_BANDWIDTH: u8 = 1;
pub const FAILURE_RESOURCES: u8 = 2;

/// What AVB Lite's streams may take of a link of `link_speed` Mb/s, in
/// bits per second: 75% (profile 6); none to go by where it is not known.
pub fn budget(link_speed: u32) -> Option<u64> {
    (link_speed > 0).then(|| u64::from(link_speed) * 750_000)
}

/// Whom a talker serves, as [`Talker::admit`] decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admitted {
    /// The listeners it sends to, each ready for its stream.
    pub ready: Vec<MacAddress>,
    /// The listeners it refuses, with the failure code each has.
    pub refused: BTreeMap<MacAddress, u8>,
    /// Whether it escalates to multicast for them.
    pub escalate: bool,
}

impl Admitted {
    /// How its listeners stand together, as one Listener declaration
    /// would say: Ready where it serves all that ask, Ready Failed where
    /// it refuses some, Asking Failed where it refuses them all; none
    /// where none asks.
    pub fn state(&self) -> Option<ListenerState> {
        match (self.ready.is_empty(), self.refused.is_empty()) {
            (false, true) => Some(ListenerState::Ready),
            (false, false) => Some(ListenerState::ReadyFailed),
            (true, false) => Some(ListenerState::AskingFailed),
            (true, true) => None,
        }
    }
}

/// A talker's stream in AVB Lite: where its copies go, whether it went
/// multicast and when its frames move there, and the listeners it
/// refuses, with the failure code each has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Talker {
    pub copies: Vec<MacAddress>,
    pub escalated: bool,
    pub escalate_at: Option<Duration>,
    pub refused: BTreeMap<MacAddress, u8>,
}

impl Talker {
    /// Whom it serves among `listeners`, those that declared for its
    /// stream, each copy taking `per_copy` bits a second of the `room`
    /// left on the link, none where that is not known, within `fanout`
    /// unicast copies, or all of them by multicast where `escalation`
    /// allows. Those it serves keep their place, then those it refuses,
    /// who ask and fail because of it, then newcomers ready for it, by
    /// address. A listener refused within the fan-out has no room on the
    /// link; beyond it, no place.
    pub fn admit(
        &self,
        listeners: impl IntoIterator<Item = (MacAddress, ListenerState)>,
        per_copy: u64,
        room: Option<u64>,
        fanout: usize,
        escalation: bool,
    ) -> Admitted {
        let rank = |mac: &MacAddress| {
            if self.copies.contains(mac) {
                0
            } else if self.refused.contains_key(mac) {
                1
            } else {
                2
            }
        };
        let mut wanting: Vec<(MacAddress, ListenerState)> = listeners
            .into_iter()
            .filter(|(mac, state)| state.ready() || self.refused.contains_key(mac))
            .collect();
        wanting.sort_by_key(|(mac, _)| (rank(mac), mac.0));
        let fanout = fanout.max(1);
        let room = room.map_or(usize::MAX, |room| {
            usize::try_from(room / per_copy.max(1)).unwrap_or(usize::MAX)
        });
        let escalate = escalation && wanting.len() > fanout && room >= 1;
        let limit = if escalate {
            wanting.len()
        } else {
            fanout.min(room)
        };
        let ready = wanting
            .iter()
            .take(limit)
            .filter(|(_, state)| state.ready())
            .map(|(mac, _)| *mac)
            .collect();
        let refused = wanting
            .iter()
            .enumerate()
            .skip(limit)
            .map(|(place, (mac, _))| {
                let code = if place < fanout {
                    FAILURE_BANDWIDTH
                } else {
                    FAILURE_RESOURCES
                };
                (*mac, code)
            })
            .collect();
        Admitted {
            ready,
            refused,
            escalate,
        }
    }

    /// Moves its copies to `ready`, or for them to `multicast` where it
    /// escalates: the declaration first and the frames `join_time` later
    /// to escalate, the frames first and the declaration after to go back
    /// (profile 6). The copies to send now, and whether its declaration
    /// changed, which then goes out at once.
    pub fn step(
        &mut self,
        now: Duration,
        escalate: bool,
        ready: &[MacAddress],
        multicast: MacAddress,
        join_time: Duration,
    ) -> (Vec<MacAddress>, bool) {
        let mut announce = false;
        let copies = if escalate {
            if !self.escalated {
                self.escalated = true;
                self.escalate_at = Some(now + join_time);
                announce = true;
            }
            match self.escalate_at {
                Some(at) if now < at => self.copies.clone(),
                _ => {
                    self.escalate_at = None;
                    if ready.is_empty() {
                        Vec::new()
                    } else {
                        vec![multicast]
                    }
                }
            }
        } else {
            if self.escalated {
                self.escalated = false;
                self.escalate_at = None;
                announce = true;
            }
            ready.to_vec()
        };
        (copies, announce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(last: u8) -> MacAddress {
        MacAddress([0x02, 0, 0, 0, 0, last])
    }

    const GROUP: MacAddress = MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x63, 0x26]);
    const JOIN_TIME: Duration = Duration::from_millis(200);

    /// Within the fan-out every ready listener has a copy; beyond it the
    /// newest are refused, for want of a place, and those served keep
    /// theirs as others come.
    #[test]
    fn listeners_are_served_within_the_fan_out() {
        let mut talker = Talker::default();
        let ready = |macs: &[u8]| -> Vec<(MacAddress, ListenerState)> {
            macs.iter()
                .map(|last| (mac(*last), ListenerState::Ready))
                .collect()
        };
        let admitted = talker.admit(ready(&[3, 1]), 1000, None, 2, false);
        assert_eq!(admitted.ready, [mac(1), mac(3)]);
        assert_eq!(admitted.state(), Some(ListenerState::Ready));
        talker.copies = admitted.ready;
        // A third, before both by address, is refused all the same.
        let admitted = talker.admit(ready(&[3, 1, 0]), 1000, None, 2, false);
        assert_eq!(admitted.ready, [mac(1), mac(3)]);
        assert_eq!(
            admitted.refused.into_iter().collect::<Vec<_>>(),
            [(mac(0), FAILURE_RESOURCES)]
        );
        // One asking and failed, not refused by it, is no listener of its.
        let asking = [(mac(4), ListenerState::AskingFailed)];
        assert_eq!(
            talker.admit(asking, 1000, None, 2, false),
            Admitted::default()
        );
    }

    /// The link's room limits the copies below the fan-out, refusing for
    /// bandwidth; a refused listener comes before newcomers once there is
    /// room.
    #[test]
    fn the_links_room_limits_the_copies() {
        let mut talker = Talker::default();
        let listeners = [
            (mac(1), ListenerState::Ready),
            (mac(2), ListenerState::Ready),
        ];
        let admitted = talker.admit(listeners, 1000, Some(1500), 2, false);
        assert_eq!(admitted.ready, [mac(1)]);
        assert_eq!(
            admitted.refused.clone().into_iter().collect::<Vec<_>>(),
            [(mac(2), FAILURE_BANDWIDTH)]
        );
        assert_eq!(admitted.state(), Some(ListenerState::ReadyFailed));
        talker.copies = admitted.ready;
        talker.refused = admitted.refused;
        // The refused one, now asking and failed, is first for the room.
        let listeners = [
            (mac(0), ListenerState::Ready),
            (mac(2), ListenerState::AskingFailed),
        ];
        let admitted = talker.admit(listeners, 1000, Some(1500), 1, false);
        assert!(admitted.ready.is_empty());
        assert_eq!(admitted.state(), Some(ListenerState::AskingFailed));
        assert_eq!(budget(1000), Some(750_000_000));
        assert_eq!(budget(0), None);
    }

    /// Beyond the fan-out, where allowed, the stream escalates: the
    /// declaration at once and the frames JoinTime later; going back, the
    /// frames at once.
    #[test]
    fn escalating_declares_before_the_frames_move() {
        let mut talker = Talker::default();
        let listeners: Vec<_> = (1..=3)
            .map(|last| (mac(last), ListenerState::Ready))
            .collect();
        let admitted = talker.admit(listeners.clone(), 1000, None, 2, true);
        assert!(admitted.escalate);
        assert_eq!(admitted.ready.len(), 3);
        talker.copies = vec![mac(1), mac(2)];
        let start = Duration::from_secs(5);
        let (copies, announce) = talker.step(start, true, &admitted.ready, GROUP, JOIN_TIME);
        assert!(announce);
        assert_eq!(copies, [mac(1), mac(2)]);
        let (copies, announce) =
            talker.step(start + JOIN_TIME, true, &admitted.ready, GROUP, JOIN_TIME);
        assert!(!announce);
        assert_eq!(copies, [GROUP]);
        talker.copies = copies;
        let (copies, announce) =
            talker.step(start + JOIN_TIME * 2, false, &[mac(1)], GROUP, JOIN_TIME);
        assert!(announce);
        assert_eq!(copies, [mac(1)]);
        // Not allowed, or without room for one copy, it does not.
        let fresh = Talker::default();
        assert!(
            !fresh
                .admit(listeners.clone(), 1000, None, 2, false)
                .escalate
        );
        assert!(!fresh.admit(listeners, 1000, Some(500), 2, true).escalate);
    }
}
