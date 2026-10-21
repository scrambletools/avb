//! CVU SRP's participant for one port (profile 6): it declares the
//! talkers and listeners here as AECP vendor unique commands, and keeps
//! what the others on the link declare, with MRP's timing as the profile
//! carries it over. Each declaration goes out twice, JoinTime apart, and
//! again every LeaveAllTime and up to half as much more; one withdrawn
//! lasts LeaveTime; and one not heard again within 16 s is gone.
//!
//! Talkers declare to the broadcast address, with the destination
//! `00:00:00:00:00:00` while their stream goes unicast, one copy to each
//! listener they admit, and the multicast address once it escalates; a
//! listener they refuse hears a Talker Failed sent to it alone. Listeners
//! declare to their talker alone, one declaration for a stream however
//! many listen to it here, as a talker knows a computer by its address.
//!
//! A computer does not hear what it sends, so the participant hands its
//! own broadcast declarations, and those to this computer's address, back
//! to itself: a talker and a listener here find each other as two on the
//! link do.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::time::Duration;

use atdecc::{EntityId, MacAddress};
use avb_mrp::msrp::{ListenerState, TalkerDeclaration, TalkerFailure};
use avb_mrp::{Event as MrpEvent, mrpdu};

use crate::admission::{self, FAILURE_BANDWIDTH, Talker};
use crate::cvu::{self, Declared};

/// CVU SRP's timing, MRP's (IEEE 802.1Q-2022, Table 10-7) as the profile
/// carries it over.
pub const JOIN_TIME: Duration = Duration::from_millis(200);
pub const LEAVE_TIME: Duration = Duration::from_millis(800);
pub const LEAVE_ALL_TIME: Duration = Duration::from_secs(10);
pub const AGE_OUT: Duration = Duration::from_secs(16);

/// Where talkers declare.
pub const BROADCAST: MacAddress = MacAddress([0xff; 6]);

/// AVB Lite's priority for streams and CVU SRP (profile 7).
pub const PRIORITY: u8 = 5;

/// The unicast fan-out the profile recommends.
pub const FANOUT: u8 = 2;

/// The media VLAN where none is configured.
pub const MEDIA_VLAN: u16 = 2;

/// A talker's destination while its stream goes unicast.
pub const UNICAST: MacAddress = MacAddress([0; 6]);

/// What the participant tells its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Where a talker here sends its stream now: a copy to each address,
    /// its multicast address alone once escalated, or nowhere; how many
    /// listeners it serves, and how they stand together.
    Copies {
        stream_id: u64,
        copies: Vec<MacAddress>,
        ready: usize,
        state: Option<ListenerState>,
    },
    /// The talker of a stream came, changed or went, as it declares to
    /// all or to this computer alone: its listeners here ask
    /// [`Participant::listen`] again.
    Talker(u64),
}

/// A talker's declaration heard: what, from where, and when last.
#[derive(Debug, Clone, Copy)]
struct Heard {
    declaration: TalkerDeclaration,
    from: MacAddress,
    at: Duration,
}

/// A talker here: in whose name it declares, what as AVB declares it,
/// with its multicast address, and whom it serves.
struct Local {
    sender: EntityId,
    declaration: TalkerDeclaration,
    talker: Talker,
    /// What its caller last heard of it.
    told: Option<(Vec<MacAddress>, usize, Option<ListenerState>)>,
}

/// The Listener declaration this computer sends for a stream: to its
/// talker, in which entity's name, and its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listening {
    talker: MacAddress,
    state: ListenerState,
    sender: EntityId,
}

/// A declaration to send a second time, JoinTime after the first.
struct Repeat {
    at: Duration,
    sender: EntityId,
    destination: MacAddress,
    message: mrpdu::Message,
}

pub struct Participant {
    mac: MacAddress,
    /// The link's speed in Mb/s, 0 where not known.
    link_speed: u32,
    fanout: usize,
    escalation: bool,
    media_vlan: u16,
    sequence: u16,
    /// Talker declarations heard, by stream: those to all, and those to
    /// this computer alone, which refuse one of its listeners and outrank
    /// the others (profile 6, item 10).
    talkers: BTreeMap<u64, Heard>,
    refusals: BTreeMap<u64, Heard>,
    /// Listener declarations heard for the talkers here, by stream and by
    /// the listener's address.
    listeners: BTreeMap<u64, BTreeMap<MacAddress, (ListenerState, Duration)>>,
    /// Declarations withdrawn, kept until LeaveTime after their Lv unless
    /// declared again first.
    leaving_talkers: BTreeMap<u64, Duration>,
    leaving_refusals: BTreeMap<u64, Duration>,
    leaving_listeners: BTreeMap<(u64, MacAddress), Duration>,
    local: BTreeMap<u64, Local>,
    listening: BTreeMap<u64, Listening>,
    repeats: Vec<Repeat>,
    next_refresh: Duration,
    /// Draws the refresh interval.
    random: u64,
    outgoing: VecDeque<(MacAddress, Vec<u8>)>,
    /// What it sent that this computer takes too: whether to all, and the
    /// command.
    looped: VecDeque<(bool, Vec<u8>)>,
    events: VecDeque<Event>,
}

impl Participant {
    /// The port's participant on the interface with address `mac`, from
    /// `now`: a fan-out of 2, escalation not allowed, the media VLAN 2,
    /// and no link speed to go by until it is set.
    pub fn new(mac: MacAddress, now: Duration) -> Self {
        let [a, b, c, d, e, f] = mac.0;
        let mut participant = Participant {
            mac,
            link_speed: 0,
            fanout: usize::from(FANOUT),
            escalation: false,
            media_vlan: MEDIA_VLAN,
            sequence: 0,
            talkers: BTreeMap::new(),
            refusals: BTreeMap::new(),
            listeners: BTreeMap::new(),
            leaving_talkers: BTreeMap::new(),
            leaving_refusals: BTreeMap::new(),
            leaving_listeners: BTreeMap::new(),
            local: BTreeMap::new(),
            listening: BTreeMap::new(),
            repeats: Vec::new(),
            next_refresh: now,
            random: u64::from_be_bytes([0, 0, a, b, c, d, e, f]) | 1,
            outgoing: VecDeque::new(),
            looped: VecDeque::new(),
            events: VecDeque::new(),
        };
        participant.next_refresh = participant.refresh_after(now);
        participant
    }

    pub fn link_speed(&self) -> u32 {
        self.link_speed
    }

    pub fn fanout(&self) -> usize {
        self.fanout
    }

    pub fn escalation(&self) -> bool {
        self.escalation
    }

    pub fn media_vlan(&self) -> u16 {
        self.media_vlan
    }

    /// The VLAN tag's TCI streams and CVU SRP go with: AVB Lite's
    /// priority and the media VLAN, or 0 for a priority tag only.
    pub fn tag(&self) -> u16 {
        (u16::from(PRIORITY) << 13) | self.media_vlan
    }

    /// The link's speed in Mb/s, 0 where not known: the talkers here are
    /// admitted again within 75% of it.
    pub fn set_link_speed(&mut self, now: Duration, link_speed: u32) {
        if link_speed != self.link_speed {
            self.link_speed = link_speed;
            self.admit_all(now);
        }
    }

    /// How many listeners a talker here serves unicast, at least one.
    pub fn set_fanout(&mut self, now: Duration, fanout: usize) {
        let fanout = fanout.max(1);
        if fanout != self.fanout {
            self.fanout = fanout;
            self.admit_all(now);
        }
    }

    /// Whether the talkers here may escalate to multicast beyond their
    /// fan-out (SET_LITE_CONFIG).
    pub fn set_escalation(&mut self, now: Duration, allowed: bool) {
        if allowed != self.escalation {
            self.escalation = allowed;
            self.admit_all(now);
        }
    }

    /// The media VLAN, 0 for priority tags only: the talkers here declare
    /// their streams in it.
    pub fn set_media_vlan(&mut self, now: Duration, vlan: u16) {
        let vlan = vlan & 0x0fff;
        if vlan != self.media_vlan {
            self.media_vlan = vlan;
            let streams: Vec<u64> = self.local.keys().copied().collect();
            for stream_id in streams {
                if let Some(declared) = self.declared(stream_id) {
                    let sender = self.local[&stream_id].sender;
                    self.declare(
                        now,
                        sender,
                        BROADCAST,
                        cvu::talker_message(&declared, MrpEvent::New),
                    );
                }
            }
        }
    }

    /// Declares a talker here, in entity `sender`'s name: `declaration` as
    /// AVB declares it, with its multicast address, which AVB Lite
    /// declares in the media VLAN at its priority. Declaring it again as
    /// it was does nothing.
    pub fn declare_talker(
        &mut self,
        now: Duration,
        sender: EntityId,
        declaration: TalkerDeclaration,
    ) {
        // Which event declared it says nothing of the stream.
        let declaration = TalkerDeclaration {
            event: MrpEvent::New,
            ..declaration
        };
        let stream_id = declaration.stream_id;
        if let Some(local) = self.local.get_mut(&stream_id) {
            if local.sender == sender && local.declaration == declaration {
                return;
            }
            local.sender = sender;
            local.declaration = declaration;
        } else {
            self.local.insert(
                stream_id,
                Local {
                    sender,
                    declaration,
                    talker: Talker::default(),
                    told: None,
                },
            );
        }
        if let Some(declared) = self.declared(stream_id) {
            self.declare(
                now,
                sender,
                BROADCAST,
                cvu::talker_message(&declared, MrpEvent::New),
            );
        }
        self.admit_all(now);
    }

    /// Withdraws a talker here, and its refusals: the others may take the
    /// room it leaves.
    pub fn withdraw_talker(&mut self, now: Duration, stream_id: u64) {
        if self.withdraw(stream_id) {
            self.admit_all(now);
        }
    }

    /// Sends a talker here's Lv, and its refusals', and forgets it.
    /// Whether there was one.
    fn withdraw(&mut self, stream_id: u64) -> bool {
        let Some(declared) = self.declared(stream_id) else {
            return false;
        };
        let Some(local) = self.local.remove(&stream_id) else {
            return false;
        };
        self.send(
            local.sender,
            BROADCAST,
            &cvu::talker_message(&declared, MrpEvent::Lv),
        );
        for (listener, code) in local.talker.refused {
            let refusal = self.refused(declared, code);
            self.send(
                local.sender,
                listener,
                &cvu::talker_message(&refusal, MrpEvent::Lv),
            );
        }
        true
    }

    /// Where a talker here sends its stream now.
    pub fn copies(&self, stream_id: u64) -> Option<&[MacAddress]> {
        self.local
            .get(&stream_id)
            .map(|local| local.talker.copies.as_slice())
    }

    /// The bandwidth the copies of the talkers here take, in bits a second.
    pub fn committed(&self) -> u64 {
        self.committed_but(None)
    }

    /// The talker of `stream_id` as heard: one refusing this computer's
    /// listener outranks its declaration to all.
    pub fn talker(&self, stream_id: u64) -> Option<(TalkerDeclaration, MacAddress)> {
        self.refusals
            .get(&stream_id)
            .or_else(|| self.talkers.get(&stream_id))
            .map(|heard| (heard.declaration, heard.from))
    }

    /// Listens to `stream_id` here, in entity `sender`'s name, or stops
    /// listening for none: one Listener declaration goes to its talker,
    /// Ready, or Asking Failed where the talker refuses this computer or
    /// its link has no room for the stream beside the others it takes
    /// (profile 6, item 6), and moves as the talker does. The talker's
    /// declaration as it stands for this computer: a stream the link has
    /// no room for comes Talker Failed, for bandwidth, from this
    /// computer.
    pub fn listen(
        &mut self,
        now: Duration,
        stream_id: u64,
        sender: Option<EntityId>,
    ) -> Option<TalkerDeclaration> {
        let heard = self.talker(stream_id);
        let before = self.listening.get(&stream_id).copied();
        let full = heard.is_some_and(|(declaration, from)| {
            declaration.failure.is_none()
                && from != self.mac
                && !self.ingress_fits(stream_id, &declaration)
        });
        let wanted = heard
            .zip(sender)
            .map(|((declaration, from), sender)| Listening {
                talker: from,
                state: if declaration.failure.is_some() || full {
                    ListenerState::AskingFailed
                } else {
                    ListenerState::Ready
                },
                sender,
            });
        if let Some(before) = before
            && wanted.map(|wanted| wanted.talker) != Some(before.talker)
        {
            self.send(
                before.sender,
                before.talker,
                &cvu::listener_message(stream_id, before.state, MrpEvent::Lv),
            );
        }
        match wanted {
            Some(wanted) => self.listening.insert(stream_id, wanted),
            None => self.listening.remove(&stream_id),
        };
        let state = |listening: Option<Listening>| listening.map(|sent| (sent.talker, sent.state));
        if let Some(wanted) = wanted
            && state(Some(wanted)) != state(before)
        {
            self.declare(
                now,
                wanted.sender,
                wanted.talker,
                cvu::listener_message(stream_id, wanted.state, MrpEvent::New),
            );
        }
        heard.map(|(declaration, _)| {
            if full {
                self.refused(declaration, FAILURE_BANDWIDTH)
            } else {
                declaration
            }
        })
    }

    /// Takes a CVU SRP command `source` sent, to a group address or to
    /// this computer alone, after the AVTP ethertype. One from this
    /// computer's own address is its own, which a socket hearing outgoing
    /// frames hands back, and which it has taken already: a refusal it
    /// sent another computer would read as one refusing it.
    pub fn handle_pdu(&mut self, now: Duration, source: MacAddress, group: bool, pdu: &[u8]) {
        if source != self.mac {
            self.take(now, source, group, pdu);
        }
    }

    fn take(&mut self, now: Duration, source: MacAddress, group: bool, pdu: &[u8]) {
        let Some(declared) = cvu::cvu_declarations(pdu) else {
            return;
        };
        for declaration in declared {
            match declaration {
                Declared::Talker(talker) if !group => {
                    self.leaving_refusals.remove(&talker.stream_id);
                    let changed = self.refusals.get(&talker.stream_id).is_none_or(|known| {
                        known.declaration.failure != talker.failure || known.from != source
                    });
                    self.refusals.insert(
                        talker.stream_id,
                        Heard {
                            declaration: talker,
                            from: source,
                            at: now,
                        },
                    );
                    if changed {
                        self.events.push_back(Event::Talker(talker.stream_id));
                    }
                }
                Declared::TalkerGone(stream_id) if !group => {
                    if self.refusals.contains_key(&stream_id) {
                        self.leaving_refusals
                            .entry(stream_id)
                            .or_insert(now + LEAVE_TIME);
                    }
                }
                Declared::Talker(talker) => {
                    self.leaving_talkers.remove(&talker.stream_id);
                    let changed = self.talkers.get(&talker.stream_id).is_none_or(|known| {
                        known.declaration.destination != talker.destination
                            || known.declaration.failure != talker.failure
                            || known.from != source
                    });
                    self.talkers.insert(
                        talker.stream_id,
                        Heard {
                            declaration: talker,
                            from: source,
                            at: now,
                        },
                    );
                    if changed {
                        self.events.push_back(Event::Talker(talker.stream_id));
                    }
                }
                Declared::TalkerGone(stream_id) => {
                    if self.talkers.contains_key(&stream_id) {
                        self.leaving_talkers
                            .entry(stream_id)
                            .or_insert(now + LEAVE_TIME);
                    }
                }
                Declared::Listener(stream_id, state) => {
                    self.leaving_listeners.remove(&(stream_id, source));
                    let listeners = self.listeners.entry(stream_id).or_default();
                    let changed = listeners
                        .get(&source)
                        .is_none_or(|(known, _)| *known != state);
                    listeners.insert(source, (state, now));
                    if changed && self.local.contains_key(&stream_id) {
                        self.admit_all(now);
                    }
                }
                Declared::ListenerGone(stream_id) => {
                    let known = self
                        .listeners
                        .get(&stream_id)
                        .is_some_and(|listeners| listeners.contains_key(&source));
                    if known {
                        self.leaving_listeners
                            .entry((stream_id, source))
                            .or_insert(now + LEAVE_TIME);
                    }
                }
            }
        }
    }

    /// Runs its timers: what it sent itself, declarations gone quiet or
    /// withdrawn, second sends, escalations due and the refresh.
    pub fn handle_timeout(&mut self, now: Duration) {
        while let Some((group, pdu)) = self.looped.pop_front() {
            let mac = self.mac;
            self.take(now, mac, group, &pdu);
        }
        let stale = |at: Duration| now.saturating_sub(at) > AGE_OUT;
        let mut gone: Vec<u64> = Vec::new();
        self.talkers.retain(|stream_id, heard| {
            let keep = !stale(heard.at);
            if !keep {
                gone.push(*stream_id);
            }
            keep
        });
        let left: Vec<u64> = due(&self.leaving_talkers, now);
        for stream_id in left {
            self.leaving_talkers.remove(&stream_id);
            if self.talkers.remove(&stream_id).is_some() {
                gone.push(stream_id);
            }
        }
        let left: Vec<u64> = due(&self.leaving_refusals, now);
        for stream_id in left {
            self.leaving_refusals.remove(&stream_id);
            if self.refusals.remove(&stream_id).is_some() {
                gone.push(stream_id);
            }
        }
        self.refusals.retain(|stream_id, heard| {
            let keep = !stale(heard.at);
            if !keep {
                gone.push(*stream_id);
            }
            keep
        });
        let mut listeners_went = false;
        for listeners in self.listeners.values_mut() {
            let before = listeners.len();
            listeners.retain(|_, (_, at)| !stale(*at));
            listeners_went |= listeners.len() != before;
        }
        let left: Vec<(u64, MacAddress)> = due(&self.leaving_listeners, now);
        for (stream_id, listener) in left {
            self.leaving_listeners.remove(&(stream_id, listener));
            listeners_went |= self
                .listeners
                .get_mut(&stream_id)
                .and_then(|listeners| listeners.remove(&listener))
                .is_some();
        }
        self.listeners.retain(|_, listeners| !listeners.is_empty());
        gone.sort_unstable();
        gone.dedup();
        for stream_id in gone {
            self.events.push_back(Event::Talker(stream_id));
        }
        let (repeats, later) = core::mem::take(&mut self.repeats)
            .into_iter()
            .partition::<Vec<_>, _>(|repeat| repeat.at <= now);
        self.repeats = later;
        for repeat in repeats {
            self.send(repeat.sender, repeat.destination, &repeat.message);
        }
        let escalating = self
            .local
            .values()
            .any(|local| local.talker.escalate_at.is_some_and(|at| at <= now));
        if listeners_went || escalating {
            self.admit_all(now);
        }
        if now >= self.next_refresh {
            self.next_refresh = self.refresh_after(now);
            self.refresh(now);
        }
    }

    /// When it next needs [`Participant::handle_timeout`]: at once where
    /// it has its own commands to take.
    pub fn poll_timeout(&self) -> Duration {
        if !self.looped.is_empty() {
            return Duration::ZERO;
        }
        let ages = self
            .talkers
            .values()
            .chain(self.refusals.values())
            .map(|heard| heard.at)
            .chain(
                self.listeners
                    .values()
                    .flat_map(|listeners| listeners.values().map(|(_, at)| *at)),
            )
            .map(|at| at + AGE_OUT + Duration::from_millis(1));
        let leaving = self
            .leaving_talkers
            .values()
            .chain(self.leaving_refusals.values())
            .chain(self.leaving_listeners.values())
            .copied();
        let escalations = self
            .local
            .values()
            .filter_map(|local| local.talker.escalate_at);
        ages.chain(leaving)
            .chain(escalations)
            .chain(self.repeats.iter().map(|repeat| repeat.at))
            .fold(self.next_refresh, Duration::min)
    }

    /// The next CVU SRP command to send, and where to: the caller sends it
    /// from this computer's address, tagged with [`Participant::tag`]
    /// (profile 6, item 5), after the AVTP ethertype.
    pub fn poll_transmit(&mut self) -> Option<(MacAddress, Vec<u8>)> {
        self.outgoing.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Leaves AVB Lite, as the link comes up again (profile 2.2): every
    /// declaration here is withdrawn, and what was heard goes; each talker
    /// here hears it sends nowhere.
    pub fn leave(&mut self) {
        let streams: Vec<u64> = self.local.keys().copied().collect();
        for stream_id in streams {
            if self.withdraw(stream_id) {
                self.events.push_back(Event::Copies {
                    stream_id,
                    copies: Vec::new(),
                    ready: 0,
                    state: None,
                });
            }
        }
        for (stream_id, listening) in core::mem::take(&mut self.listening) {
            self.send(
                listening.sender,
                listening.talker,
                &cvu::listener_message(stream_id, listening.state, MrpEvent::Lv),
            );
        }
        self.repeats.clear();
        self.looped.clear();
        self.talkers.clear();
        self.refusals.clear();
        self.listeners.clear();
        self.leaving_talkers.clear();
        self.leaving_refusals.clear();
        self.leaving_listeners.clear();
    }

    /// When to send every declaration again: from LeaveAllTime to half as
    /// much again from `now`.
    fn refresh_after(&mut self, now: Duration) -> Duration {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        now + LEAVE_ALL_TIME + Duration::from_millis(self.random % 5000)
    }

    /// Sends every declaration here again: the talkers', their refusals
    /// and the listeners'.
    fn refresh(&mut self, now: Duration) {
        let streams: Vec<u64> = self.local.keys().copied().collect();
        for stream_id in streams {
            let (Some(declared), Some(local)) =
                (self.declared(stream_id), self.local.get(&stream_id))
            else {
                continue;
            };
            let sender = local.sender;
            let refused = local.talker.refused.clone();
            self.declare(
                now,
                sender,
                BROADCAST,
                cvu::talker_message(&declared, MrpEvent::JoinIn),
            );
            for (listener, code) in refused {
                let refusal = self.refused(declared, code);
                self.declare(
                    now,
                    sender,
                    listener,
                    cvu::talker_message(&refusal, MrpEvent::JoinIn),
                );
            }
        }
        let listening: Vec<(u64, Listening)> = self
            .listening
            .iter()
            .map(|(stream_id, listening)| (*stream_id, *listening))
            .collect();
        for (stream_id, listening) in listening {
            self.declare(
                now,
                listening.sender,
                listening.talker,
                cvu::listener_message(stream_id, listening.state, MrpEvent::JoinIn),
            );
        }
    }

    /// Admits the listeners of every talker here again, as they, the link
    /// or the settings changed: each talker's room is what the others'
    /// copies leave, so where one sends fewer, the others are admitted
    /// once more for the room it left.
    fn admit_all(&mut self, now: Duration) {
        let streams: Vec<u64> = self.local.keys().copied().collect();
        let copies = |participant: &Self, stream_id: u64| {
            participant
                .local
                .get(&stream_id)
                .map_or(0, |local| local.talker.copies.len())
        };
        let mut shrank = false;
        for stream_id in &streams {
            let before = copies(self, *stream_id);
            self.admit(now, *stream_id);
            shrank |= copies(self, *stream_id) < before;
        }
        if shrank {
            for stream_id in streams {
                self.admit(now, stream_id);
            }
        }
    }

    /// Sends a talker's stream to the listeners that want it, within its
    /// fan-out and 75% of the link, or to its multicast address where it
    /// escalates; refuses the others, each alone, and withdraws the
    /// refusal from those it serves again (profile 6, item 10).
    fn admit(&mut self, now: Duration, stream_id: u64) {
        let room = admission::budget(self.link_speed)
            .map(|budget| budget.saturating_sub(self.committed_but(Some(stream_id))));
        let listeners: Vec<(MacAddress, ListenerState)> = self
            .listeners
            .get(&stream_id)
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(mac, (state, _))| (*mac, *state))
                    .collect()
            })
            .unwrap_or_default();
        let (fanout, escalation) = (self.fanout, self.escalation);
        let Some(local) = self.local.get_mut(&stream_id) else {
            return;
        };
        let per_copy = local.declaration.bandwidth(8000).max(1);
        let admitted = local
            .talker
            .admit(listeners, per_copy, room, fanout, escalation);
        let (copies, announce) = local.talker.step(
            now,
            admitted.escalate,
            &admitted.ready,
            local.declaration.destination,
            JOIN_TIME,
        );
        local.talker.copies.clone_from(&copies);
        let before = core::mem::replace(&mut local.talker.refused, admitted.refused.clone());
        let sender = local.sender;
        let told = Some((copies.clone(), admitted.ready.len(), admitted.state()));
        let tell = local.told != told;
        local.told = told;
        if tell {
            self.events.push_back(Event::Copies {
                stream_id,
                copies,
                ready: admitted.ready.len(),
                state: admitted.state(),
            });
        }
        let Some(declared) = self.declared(stream_id) else {
            return;
        };
        if announce {
            self.declare(
                now,
                sender,
                BROADCAST,
                cvu::talker_message(&declared, MrpEvent::New),
            );
        }
        for (listener, code) in &admitted.refused {
            if before.get(listener) != Some(code) {
                let refusal = self.refused(declared, *code);
                self.declare(
                    now,
                    sender,
                    *listener,
                    cvu::talker_message(&refusal, MrpEvent::New),
                );
            }
        }
        for (listener, code) in before {
            if !admitted.refused.contains_key(&listener) {
                let refusal = self.refused(declared, code);
                self.send(
                    sender,
                    listener,
                    &cvu::talker_message(&refusal, MrpEvent::Lv),
                );
            }
        }
    }

    /// A talker here's declaration as AVB Lite makes it: its frames'
    /// destination, `00:00:00:00:00:00` while unicast, its multicast
    /// address once escalated; in the media VLAN, at AVB Lite's priority.
    fn declared(&self, stream_id: u64) -> Option<TalkerDeclaration> {
        let local = self.local.get(&stream_id)?;
        Some(TalkerDeclaration {
            destination: if local.talker.escalated {
                local.declaration.destination
            } else {
                UNICAST
            },
            vlan_id: self.media_vlan,
            priority: PRIORITY,
            ..local.declaration
        })
    }

    /// `declaration` failed with `code`, this computer the system that
    /// failed: its address as the Bridge ID, after a priority of 0
    /// (profile 6, Declaration content).
    fn refused(&self, declaration: TalkerDeclaration, code: u8) -> TalkerDeclaration {
        let [a, b, c, d, e, f] = self.mac.0;
        TalkerDeclaration {
            failure: Some(TalkerFailure {
                bridge_id: u64::from_be_bytes([0, 0, a, b, c, d, e, f]),
                code,
            }),
            ..declaration
        }
    }

    /// The bandwidth the copies of the talkers here take, but for
    /// `except`'s, in bits a second.
    fn committed_but(&self, except: Option<u64>) -> u64 {
        self.local
            .iter()
            .filter(|(stream_id, _)| Some(**stream_id) != except)
            .map(|(_, local)| local.talker.copies.len() as u64 * local.declaration.bandwidth(8000))
            .sum()
    }

    /// Whether this computer's link takes a stream of `declaration` on top
    /// of those from other computers it is ready for, within 75% (profile
    /// 6, item 6).
    fn ingress_fits(&self, stream_id: u64, declaration: &TalkerDeclaration) -> bool {
        let Some(budget) = admission::budget(self.link_speed) else {
            return true;
        };
        let taken: u64 = self
            .listening
            .iter()
            .filter(|(other, listening)| {
                **other != stream_id && listening.talker != self.mac && listening.state.ready()
            })
            .filter_map(|(other, _)| self.talkers.get(other))
            .map(|heard| heard.declaration.bandwidth(8000))
            .sum();
        taken + declaration.bandwidth(8000) <= budget
    }

    /// Sends a new, changed or refreshed declaration, and the same again
    /// JoinTime later, as an MRP applicant makes sure two go out.
    fn declare(
        &mut self,
        now: Duration,
        sender: EntityId,
        destination: MacAddress,
        message: mrpdu::Message,
    ) {
        self.send(sender, destination, &message);
        self.repeats.push(Repeat {
            at: now + JOIN_TIME,
            sender,
            destination,
            message,
        });
    }

    /// Sends a CVU SRP declaration in entity `sender`'s name to
    /// `destination`, superseding a second send of it still to go; this
    /// computer takes it too where it goes to all or to this computer.
    fn send(&mut self, sender: EntityId, destination: MacAddress, message: &mrpdu::Message) {
        self.repeats.retain(|repeat| {
            !(repeat.sender == sender
                && repeat.destination == destination
                && cvu::same_declaration(&repeat.message, message))
        });
        self.sequence = self.sequence.wrapping_add(1);
        let Some(pdu) = cvu::cvu_command(sender, self.sequence, message) else {
            return;
        };
        if destination == BROADCAST || destination == self.mac {
            self.looped
                .push_back((destination == BROADCAST, pdu.clone()));
        }
        self.outgoing.push_back((destination, pdu));
    }
}

/// The keys of `deadlines` due by `now`.
fn due<K: Copy + Ord>(deadlines: &BTreeMap<K, Duration>, now: Duration) -> Vec<K> {
    deadlines
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(key, _)| *key)
        .collect()
}

#[cfg(test)]
mod tests;
