//! What this computer is plugged into, from the gPTP messages heard on its
//! link without taking part in gPTP: the AVB bridge port, which stops every
//! other port's peer delay requests, or a switch taking no part in AVB,
//! which lets the endpoints' sharing it through.

use std::io;
use std::time::{Duration, Instant};

use avb_net::{MacAddress, Socket};

use crate::ClockIdentity;

const ETHERTYPE_GPTP: u16 = 0x88f7;
/// Where gPTP sends its peer delay requests, to the neighbor only: an AVB
/// bridge passes none on, though a switch taking no part in AVB may.
const GPTP_MULTICAST: MacAddress = MacAddress([0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e]);
/// How long a port is known after its last peer delay request: gPTP sends
/// one a second, an AVB Lite endpoint's beacon one every 3 s.
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long gPTP counts as running after a port's last Sync, which a
/// bridge sends eight times a second.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);
/// gPTP's majorSdoId, the high nibble of a message's first octet; AVB
/// Lite's PTP between endpoints has 0.
const GPTP_SDO: u8 = 0x1;
/// Octets of a Pdelay_Req before its TLVs.
const PDELAY_LEN: usize = 54;
/// The Endpoint Declaration TLV's organizationId, organizationSubType and
/// dataField (AVB Lite profile, 2.1): its sender is an endpoint.
const ENDPOINT_DECLARATION: [u8; 7] = [0x8c, 0x1f, 0x64, 0x36, 0xc0, 0x01, 0x01];
/// The organization extension TLV types it comes in: do-not-propagate, as
/// the profile has it, and the plain one of its earlier revisions.
const ORGANIZATION_EXTENSIONS: [u16; 2] = [0x8000, 0x0003];

/// A port sending gPTP peer delay requests on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Peer {
    pub clock: ClockIdentity,
    pub port: u16,
    /// It declares itself an endpoint, as AVB Lite's do, so is no bridge.
    pub endpoint: bool,
    /// It sends gPTP's Sync on the link, as a bridge does once gPTP runs
    /// there.
    pub synced: bool,
}

/// What this computer is plugged into, as the ports on its link show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plugged {
    /// This port of an AVB bridge: the only one heard, and no endpoint.
    Bridge(Peer),
    /// A switch taking no part in AVB, which lets peer delay requests
    /// through as no AVB bridge does: these endpoints' among them.
    Shared(Vec<Peer>),
    /// Nothing heard: gPTP runs on no port of the link, or a switch in
    /// front stops its messages.
    Unheard,
}

impl Plugged {
    /// What the `peers` heard say this computer is plugged into, a port
    /// whose clock is an entity's, as `entity` tells, being an endpoint
    /// whether or not it declares itself one.
    pub fn of(peers: &[Peer], entity: impl Fn(ClockIdentity) -> bool) -> Self {
        let (endpoints, bridges): (Vec<Peer>, Vec<Peer>) = peers
            .iter()
            .partition(|peer| peer.endpoint || entity(peer.clock));
        match (endpoints.is_empty(), bridges.as_slice()) {
            (true, []) => Self::Unheard,
            (true, [bridge]) => Self::Bridge(*bridge),
            // Endpoints', or more than one bridge's, reach only through a
            // switch passing them on.
            _ => Self::Shared(endpoints),
        }
    }
}

/// Listens to the gPTP messages the ports on this computer's link send it.
pub struct NeighborListener {
    socket: Socket,
    heard: Heard,
    reported: Vec<Peer>,
}

impl NeighborListener {
    /// Listens on `interface`. It fails where the system keeps gPTP from
    /// other programs, or raw Ethernet is out of reach.
    pub fn open(interface: &str) -> io::Result<Self> {
        let socket = Socket::open(interface, ETHERTYPE_GPTP)?;
        socket.join_multicast(GPTP_MULTICAST)?;
        Ok(Self {
            socket,
            heard: Heard::default(),
            reported: Vec::new(),
        })
    }

    /// Reads what arrived, returning the ports heard when they changed.
    pub fn poll(&mut self) -> Option<Vec<Peer>> {
        let peers = self.peers();
        (peers != self.reported).then(|| {
            self.reported.clone_from(&peers);
            peers
        })
    }

    /// Reads what arrived and returns the ports heard lately.
    pub fn peers(&mut self) -> Vec<Peer> {
        let mut buffer = [0; 128];
        let now = Instant::now();
        let own = self.socket.mac();
        while let Ok(Some(received)) = self.socket.receive(&mut buffer, Some(Duration::ZERO)) {
            // The system's own gPTP, where it runs one, is no peer.
            if received.source == own {
                continue;
            }
            self.heard
                .hear(now, &buffer[..received.length.min(buffer.len())]);
        }
        self.heard.peers(now)
    }
}

/// The ports heard, and when each last sent a peer delay request and a
/// Sync.
#[derive(Debug, Default)]
struct Heard {
    ports: Vec<Port>,
}

#[derive(Debug, Clone, Copy)]
struct Port {
    clock: ClockIdentity,
    port: u16,
    endpoint: bool,
    request: Option<Instant>,
    sync: Option<Instant>,
}

impl Heard {
    fn hear(&mut self, now: Instant, message: &[u8]) {
        let Some(message) = GptpMessage::decode(message) else {
            return;
        };
        if message.sdo != GPTP_SDO {
            return;
        }
        let index = match self
            .ports
            .iter()
            .position(|port| (port.clock, port.port) == (message.clock, message.port))
        {
            Some(index) => index,
            None => {
                self.ports.push(Port {
                    clock: message.clock,
                    port: message.port,
                    endpoint: false,
                    request: None,
                    sync: None,
                });
                self.ports.len() - 1
            }
        };
        let port = &mut self.ports[index];
        match message.message_type {
            GptpMessage::PDELAY_REQ => {
                port.request = Some(now);
                port.endpoint = message.endpoint;
            }
            GptpMessage::SYNC => port.sync = Some(now),
            _ => {}
        }
    }

    /// The ports that sent a peer delay request lately, forgetting those
    /// that sent nothing lately.
    fn peers(&mut self, now: Instant) -> Vec<Peer> {
        let lately =
            |at: Option<Instant>, timeout| at.is_some_and(|at| now.duration_since(at) < timeout);
        self.ports
            .retain(|port| lately(port.request, PEER_TIMEOUT) || lately(port.sync, SYNC_TIMEOUT));
        let mut peers: Vec<Peer> = self
            .ports
            .iter()
            .filter(|port| lately(port.request, PEER_TIMEOUT))
            .map(|port| Peer {
                clock: port.clock,
                port: port.port,
                endpoint: port.endpoint,
                synced: lately(port.sync, SYNC_TIMEOUT),
            })
            .collect();
        peers.sort();
        peers
    }
}

/// The parts of a gPTP message the ports on the link are told by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GptpMessage {
    sdo: u8,
    message_type: u8,
    clock: ClockIdentity,
    port: u16,
    /// A peer delay request carrying the Endpoint Declaration TLV.
    endpoint: bool,
}

impl GptpMessage {
    const SYNC: u8 = 0x0;
    const PDELAY_REQ: u8 = 0x2;

    /// Decodes the common header: the majorSdoId and message type, then
    /// the sending port's identity at octet 20; and for a peer delay
    /// request, whether its TLVs declare an endpoint.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let header: &[u8; 34] = bytes.get(..34)?.try_into().ok()?;
        let clock: [u8; 8] = header[20..28].try_into().ok()?;
        let message_type = header[0] & 0x0f;
        Some(Self {
            sdo: header[0] >> 4,
            message_type,
            clock: ClockIdentity(u64::from_be_bytes(clock)),
            port: u16::from_be_bytes([header[28], header[29]]),
            endpoint: message_type == Self::PDELAY_REQ && declares_endpoint(bytes),
        })
    }
}

/// Whether a peer delay request's TLVs, within its messageLength, hold the
/// Endpoint Declaration TLV.
fn declares_endpoint(message: &[u8]) -> bool {
    let length = usize::from(u16::from_be_bytes([message[2], message[3]])).min(message.len());
    let mut at = PDELAY_LEN;
    while at + 4 <= length {
        let kind = u16::from_be_bytes([message[at], message[at + 1]]);
        let size = usize::from(u16::from_be_bytes([message[at + 2], message[at + 3]]));
        let Some(body) = message.get(at + 4..at + 4 + size) else {
            return false;
        };
        if ORGANIZATION_EXTENSIONS.contains(&kind) && body.starts_with(&ENDPOINT_DECLARATION) {
            return true;
        }
        at += 4 + size;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWITCH: ClockIdentity = ClockIdentity(0x0001_f2ff_feff_3b14);
    const SYNTHIA: ClockIdentity = ClockIdentity(0xe8f6_0aff_fee0_9220);
    const MOTU: ClockIdentity = ClockIdentity(0x0001_f2ff_fe01_62e5);

    /// A gPTP message of `message_type` from port `port` of `clock`, with
    /// the Endpoint Declaration TLV, of the given type, after a peer delay
    /// request's body where `tlv` gives one.
    fn message(message_type: u8, clock: ClockIdentity, port: u16, tlv: Option<u16>) -> Vec<u8> {
        let mut frame = vec![0u8; PDELAY_LEN];
        frame[0] = (GPTP_SDO << 4) | message_type;
        frame[1] = 0x02;
        frame[20..28].copy_from_slice(&clock.0.to_be_bytes());
        frame[28..30].copy_from_slice(&port.to_be_bytes());
        if let Some(kind) = tlv {
            frame.extend_from_slice(&kind.to_be_bytes());
            frame.extend_from_slice(&8u16.to_be_bytes());
            frame.extend_from_slice(&ENDPOINT_DECLARATION);
            frame.push(0);
        }
        let length = frame.len() as u16;
        frame[2..4].copy_from_slice(&length.to_be_bytes());
        frame
    }

    #[test]
    fn peer_delay_requests_name_the_bridge_port() {
        // A bridge's Pdelay_Req, as on the bench: port 6 of the switch.
        let frame = message(GptpMessage::PDELAY_REQ, SWITCH, 6, None);
        let message = GptpMessage::decode(&frame).unwrap();
        assert_eq!(message.message_type, GptpMessage::PDELAY_REQ);
        assert_eq!(message.clock, SWITCH);
        assert_eq!(message.port, 6);
        assert!(!message.endpoint);
        assert_eq!(GptpMessage::decode(&frame[..33]), None);
    }

    /// An AVB Lite endpoint's request declares it, in either TLV type, and
    /// one cut short of its TLV does not.
    #[test]
    fn an_endpoint_declares_itself() {
        for kind in ORGANIZATION_EXTENSIONS {
            let frame = message(GptpMessage::PDELAY_REQ, SYNTHIA, 1, Some(kind));
            assert!(GptpMessage::decode(&frame).unwrap().endpoint, "{kind:#x}");
            assert!(
                !GptpMessage::decode(&frame[..PDELAY_LEN + 6])
                    .unwrap()
                    .endpoint
            );
        }
        let other = message(GptpMessage::PDELAY_REQ, SYNTHIA, 1, Some(0x0001));
        assert!(!GptpMessage::decode(&other).unwrap().endpoint);
    }

    /// Each port is heard on its own, in sync while it sends gPTP's Sync,
    /// AVB Lite's own PTP not counting, and forgotten once quiet.
    #[test]
    fn each_port_is_heard_on_its_own() {
        let start = Instant::now();
        let mut heard = Heard::default();
        heard.hear(start, &message(GptpMessage::PDELAY_REQ, SWITCH, 6, None));
        heard.hear(start, &message(GptpMessage::SYNC, SWITCH, 6, None));
        let mut lite = message(GptpMessage::SYNC, SYNTHIA, 1, None);
        lite[0] &= 0x0f;
        heard.hear(start, &lite);
        heard.hear(
            start,
            &message(GptpMessage::PDELAY_REQ, SYNTHIA, 1, Some(0x8000)),
        );
        assert_eq!(
            heard.peers(start),
            [
                Peer {
                    clock: SWITCH,
                    port: 6,
                    endpoint: false,
                    synced: true,
                },
                Peer {
                    clock: SYNTHIA,
                    port: 1,
                    endpoint: true,
                    synced: false,
                },
            ]
        );
        let later = start + SYNC_TIMEOUT;
        heard.hear(later, &message(GptpMessage::PDELAY_REQ, SWITCH, 6, None));
        assert!(heard.peers(later).iter().all(|peer| !peer.synced));
        assert_eq!(heard.peers(later + PEER_TIMEOUT), []);
        assert!(heard.ports.is_empty());
    }

    /// One bridge alone is the port this computer is plugged into; an
    /// endpoint, declared or an entity's, or a second bridge, means a
    /// switch passing them on; nothing heard is nothing.
    #[test]
    fn what_this_computer_is_plugged_into() {
        let peer = |clock, endpoint| Peer {
            clock,
            port: 1,
            endpoint,
            synced: false,
        };
        let none = |_| false;
        let bridge = peer(SWITCH, false);
        assert_eq!(Plugged::of(&[bridge], none), Plugged::Bridge(bridge));
        assert_eq!(Plugged::of(&[], none), Plugged::Unheard);
        let synthia = peer(SYNTHIA, true);
        let motu = peer(MOTU, false);
        assert_eq!(
            Plugged::of(&[synthia, motu], |clock| clock == MOTU),
            Plugged::Shared(vec![synthia, motu])
        );
        assert_eq!(
            Plugged::of(&[bridge, peer(ClockIdentity(1), false)], none),
            Plugged::Shared(Vec::new())
        );
    }
}
