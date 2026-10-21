//! Sharing a port's MRP participant between programs on one computer.
//! MRP runs per port: two participants on one point-to-point port do not
//! hear each other's Leave, so each would withdraw what the other
//! declared. So the program running a port's participant serves it to the
//! others: they ask it to declare and withdraw their talkers, and it tells
//! them as listeners for their streams come and go, and whether it can
//! declare for them at all, as it cannot while the port runs AVB Lite.
//! From version 2, they ask it to listen to streams too: it declares a
//! listener for each on the port, as the stream's talker is registered,
//! and tells them that talker's declaration as it comes, changes and
//! goes.
//!
//! It serves on a Unix socket in the user's runtime folder, named for the
//! interface, which only the same user reaches. Each message is an octet
//! of kind, two octets of length, and the payload: a Talker Advertise as
//! MSRP encodes it, a stream ID, a stream ID and a listener's state or a
//! refusal, a hello, or a stream ID and its talker's declaration.
//!
//! Each end skips the kinds it does not know, so a kind once given keeps
//! its payload, and what comes later takes a new kind. Each end says
//! hello as the connection opens, with its version and features: a
//! program that hears none from the participant soon after it connects
//! shares one of version 0, which answers no Declare and tells only the
//! network's listeners.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::msrp::{self, ListenerState, TalkerDeclaration};

mod kind {
    pub const DECLARE: u8 = 1;
    pub const WITHDRAW: u8 = 2;
    pub const LISTENER: u8 = 3;
    pub const UNAVAILABLE: u8 = 4;
    pub const AVAILABLE: u8 = 5;
    pub const HELLO: u8 = 6;
    pub const DECLARED: u8 = 7;
    pub const REFUSED: u8 = 8;
    pub const LISTEN: u8 = 9;
    pub const UNLISTEN: u8 = 10;
    pub const TALKER: u8 = 11;
}

/// The version this module speaks: 1 since each end says hello and the
/// participant answers each Declare, which version 0 did not; 2 since
/// programs can listen through it.
pub const VERSION: u16 = 2;

/// How long a program waits for the participant's hello before taking it
/// for one of version 0, which says none.
pub const HELLO_WAIT: Duration = Duration::from_millis(500);

/// What an end can do beyond its version, one bit each.
pub mod feature {
    /// The participant's Listener notices count the listeners on this
    /// computer too, which take a program's stream as it leaves with no
    /// reservation, merged with the network's.
    pub const LOCAL_LISTENERS: u32 = 1 << 0;
    /// The participant takes Listen and Unlisten: it declares listeners
    /// for a program's streams and tells it their talkers.
    pub const LISTENERS: u32 = 1 << 1;
}

/// The state octet of a listener that left.
const LEFT: u8 = 0xff;

/// An end's version and features, as it says hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub features: u32,
}

impl Hello {
    /// Whether it has every feature of `features`.
    pub fn has(self, features: u32) -> bool {
        self.features & features == features
    }
}

/// What a program asks of the participant it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// The program's version and features, as it connects.
    Hello(Hello),
    /// Declare this talker's stream, or declare it again as it changed.
    Declare(TalkerDeclaration),
    /// Withdraw the stream with this ID.
    Withdraw(u64),
    /// Listen to the stream with this ID: declare a listener for it on
    /// the port while its talker is registered there, Ready or Asking
    /// Failed as the talker's declaration is, merged with the port's
    /// other listeners here for it, none where the talker is on this
    /// computer, and tell the program the talker's declaration as it
    /// changes. The first Talker notice answers it, at once, even when no
    /// talker declares the stream; Refused answers it while the
    /// participant cannot declare listeners, as in AVB Lite. Asking twice
    /// is asking once.
    Listen(u64),
    /// Stop listening to the stream with this ID; nothing, for one not
    /// listened to. A connection closing stops all of its listening.
    Unlisten(u64),
    /// A Declare whose talker could not be read, with the stream ID its
    /// first octets give, where it has them.
    Unreadable(Option<u64>),
}

/// What the participant tells a program sharing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    /// The participant's version and features, as the program connects.
    Hello(Hello),
    /// The listeners for one of its streams, as the port registers them:
    /// their state, or none once they left.
    Listener {
        stream_id: u64,
        state: Option<ListenerState>,
    },
    /// Its talkers and listeners cannot be declared now, as while the
    /// port runs AVB Lite; they are withdrawn until it says they can
    /// again, when the program declares and listens again.
    Unavailable,
    Available,
    /// It declared the stream with this ID, as asked.
    Declared(u64),
    /// It did not declare the stream with this ID, zero where the Declare
    /// gave none, and why; or did not listen to it.
    Refused {
        stream_id: u64,
        refusal: Refusal,
    },
    /// The talker of a stream the program listens to, as it declares the
    /// stream, Talker Advertise or Talker Failed, on the port or on this
    /// computer; none while no talker declares it.
    Talker {
        stream_id: u64,
        talker: Option<TalkerDeclaration>,
    },
}

/// Why the participant did not declare a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// The Declare's talker could not be read.
    Unreadable,
    /// It cannot declare talkers now, as while the port runs AVB Lite.
    Unavailable,
    /// Another talker here declares a stream of the same ID.
    Taken,
    /// One a later version gives.
    Other(u8),
}

impl Refusal {
    fn number(self) -> u8 {
        match self {
            Self::Unreadable => 1,
            Self::Unavailable => 2,
            Self::Taken => 3,
            Self::Other(number) => number,
        }
    }

    fn from_number(number: u8) -> Self {
        match number {
            1 => Self::Unreadable,
            2 => Self::Unavailable,
            3 => Self::Taken,
            _ => Self::Other(number),
        }
    }
}

/// The socket a participant on `interface` serves, in the user's runtime
/// folder.
pub fn socket_path(interface: &str) -> Option<PathBuf> {
    let folder = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    Some(folder.join(format!("avb-mrp-{interface}.sock")))
}

fn write_message(out: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let length = u16::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too long"))?;
    let mut message = Vec::with_capacity(3 + payload.len());
    message.push(kind);
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(payload);
    out.write_all(&message)
}

/// The next message's kind and payload.
fn read_message(input: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 3];
    input.read_exact(&mut header)?;
    let mut payload = vec![0u8; usize::from(u16::from_be_bytes([header[1], header[2]]))];
    input.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn stream_id(payload: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(payload.get(..8)?.try_into().ok()?))
}

fn hello_payload(hello: Hello) -> Vec<u8> {
    let mut payload = hello.version.to_be_bytes().to_vec();
    payload.extend_from_slice(&hello.features.to_be_bytes());
    payload
}

/// A hello, from its first six octets: a later version may add more.
fn hello(payload: &[u8]) -> Option<Hello> {
    Some(Hello {
        version: u16::from_be_bytes(payload.get(..2)?.try_into().ok()?),
        features: u32::from_be_bytes(payload.get(2..6)?.try_into().ok()?),
    })
}

pub fn write_request(out: &mut impl Write, request: &Request) -> io::Result<()> {
    match request {
        Request::Hello(hello) => write_message(out, kind::HELLO, &hello_payload(*hello)),
        Request::Declare(declaration) => {
            write_message(out, kind::DECLARE, &msrp::talker_value(declaration))
        }
        Request::Withdraw(stream_id) => {
            write_message(out, kind::WITHDRAW, &stream_id.to_be_bytes())
        }
        Request::Listen(stream_id) => write_message(out, kind::LISTEN, &stream_id.to_be_bytes()),
        Request::Unlisten(stream_id) => {
            write_message(out, kind::UNLISTEN, &stream_id.to_be_bytes())
        }
        Request::Unreadable(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an unreadable Declare is only read",
        )),
    }
}

/// The next request, skipping any it does not know; an error when the
/// other end closed.
pub fn read_request(input: &mut impl Read) -> io::Result<Request> {
    loop {
        let (kind, payload) = read_message(input)?;
        let request = match kind {
            kind::HELLO => hello(&payload).map(Request::Hello),
            kind::DECLARE => Some(
                msrp::decode_talker(
                    msrp::attribute::TALKER_ADVERTISE,
                    &payload,
                    crate::Event::New,
                )
                .map_or_else(
                    || Request::Unreadable(stream_id(&payload)),
                    Request::Declare,
                ),
            ),
            kind::WITHDRAW => stream_id(&payload).map(Request::Withdraw),
            kind::LISTEN => stream_id(&payload).map(Request::Listen),
            kind::UNLISTEN => stream_id(&payload).map(Request::Unlisten),
            _ => None,
        };
        if let Some(request) = request {
            return Ok(request);
        }
    }
}

pub fn write_notice(out: &mut impl Write, notice: &Notice) -> io::Result<()> {
    match notice {
        Notice::Hello(hello) => write_message(out, kind::HELLO, &hello_payload(*hello)),
        Notice::Listener { stream_id, state } => {
            let mut payload = stream_id.to_be_bytes().to_vec();
            payload.push(state.map_or(LEFT, ListenerState::number));
            write_message(out, kind::LISTENER, &payload)
        }
        Notice::Unavailable => write_message(out, kind::UNAVAILABLE, &[]),
        Notice::Available => write_message(out, kind::AVAILABLE, &[]),
        Notice::Declared(stream_id) => write_message(out, kind::DECLARED, &stream_id.to_be_bytes()),
        Notice::Refused { stream_id, refusal } => {
            let mut payload = stream_id.to_be_bytes().to_vec();
            payload.push(refusal.number());
            write_message(out, kind::REFUSED, &payload)
        }
        // The declaration's value, which starts with the stream ID, its
        // length telling Talker Failed's from Talker Advertise's; the
        // stream ID alone once it is gone.
        Notice::Talker { stream_id, talker } => match talker {
            Some(talker) => write_message(out, kind::TALKER, &msrp::talker_value(talker)),
            None => write_message(out, kind::TALKER, &stream_id.to_be_bytes()),
        },
    }
}

/// A Talker notice's declaration, from its value.
fn talker(payload: &[u8]) -> Option<Option<TalkerDeclaration>> {
    if payload.len() == 8 {
        return Some(None);
    }
    let attribute = if payload.len() >= TALKER_FAILED_LENGTH {
        msrp::attribute::TALKER_FAILED
    } else {
        msrp::attribute::TALKER_ADVERTISE
    };
    msrp::decode_talker(attribute, payload, crate::Event::New).map(Some)
}

/// Octets of a Talker Failed's value: Talker Advertise's and the failure.
const TALKER_FAILED_LENGTH: usize = 25 + 9;

/// The next notice, skipping any it does not know; an error when the
/// other end closed.
pub fn read_notice(input: &mut impl Read) -> io::Result<Notice> {
    loop {
        let (kind, payload) = read_message(input)?;
        let notice = match kind {
            kind::HELLO => hello(&payload).map(Notice::Hello),
            kind::LISTENER => stream_id(&payload).and_then(|stream_id| {
                let state = *payload.get(8)?;
                Some(Notice::Listener {
                    stream_id,
                    state: (state != LEFT).then(|| ListenerState::from_number(state)),
                })
            }),
            kind::UNAVAILABLE => Some(Notice::Unavailable),
            kind::AVAILABLE => Some(Notice::Available),
            kind::DECLARED => stream_id(&payload).map(Notice::Declared),
            kind::REFUSED => stream_id(&payload).and_then(|stream_id| {
                Some(Notice::Refused {
                    stream_id,
                    refusal: Refusal::from_number(*payload.get(8)?),
                })
            }),
            kind::TALKER => stream_id(&payload).and_then(|stream_id| {
                Some(Notice::Talker {
                    stream_id,
                    talker: talker(&payload)?,
                })
            }),
            _ => None,
        };
        if let Some(notice) = notice {
            return Ok(notice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, MacAddress};

    fn talker() -> TalkerDeclaration {
        TalkerDeclaration {
            stream_id: 0xa010_a3a5_1726_7000,
            destination: MacAddress([0x91, 0xe0, 0xf0, 0x00, 0x63, 0x26]),
            vlan_id: 2,
            max_frame_size: 56,
            max_interval_frames: 1,
            priority: 3,
            rank: true,
            accumulated_latency: 125_000,
            failure: None,
            event: Event::New,
        }
    }

    const HELLO: Hello = Hello {
        version: VERSION,
        features: feature::LOCAL_LISTENERS,
    };

    #[test]
    fn requests_and_notices_go_across() {
        let mut wire = Vec::new();
        write_request(&mut wire, &Request::Hello(HELLO)).unwrap();
        write_request(&mut wire, &Request::Declare(talker())).unwrap();
        // One the other end does not know, which it skips.
        write_message(&mut wire, 99, &[1, 2, 3]).unwrap();
        write_request(&mut wire, &Request::Withdraw(7)).unwrap();
        write_request(&mut wire, &Request::Listen(8)).unwrap();
        write_request(&mut wire, &Request::Unlisten(8)).unwrap();
        let mut input = wire.as_slice();
        assert_eq!(read_request(&mut input).unwrap(), Request::Hello(HELLO));
        assert_eq!(
            read_request(&mut input).unwrap(),
            Request::Declare(talker())
        );
        assert_eq!(read_request(&mut input).unwrap(), Request::Withdraw(7));
        assert_eq!(read_request(&mut input).unwrap(), Request::Listen(8));
        assert_eq!(read_request(&mut input).unwrap(), Request::Unlisten(8));
        assert!(read_request(&mut input).is_err(), "nothing more");

        let notices = [
            Notice::Hello(HELLO),
            Notice::Listener {
                stream_id: 7,
                state: Some(ListenerState::Ready),
            },
            Notice::Listener {
                stream_id: 7,
                state: None,
            },
            Notice::Unavailable,
            Notice::Available,
            Notice::Declared(7),
            Notice::Refused {
                stream_id: 7,
                refusal: Refusal::Taken,
            },
            Notice::Refused {
                stream_id: 0,
                refusal: Refusal::Other(42),
            },
            Notice::Talker {
                stream_id: talker().stream_id,
                talker: Some(talker()),
            },
            Notice::Talker {
                stream_id: talker().stream_id,
                talker: Some(TalkerDeclaration {
                    failure: Some(msrp::TalkerFailure {
                        bridge_id: 0x8000_0001_f2ff_ee42,
                        code: 2,
                    }),
                    ..talker()
                }),
            },
            Notice::Talker {
                stream_id: 7,
                talker: None,
            },
        ];
        let mut wire = Vec::new();
        for notice in &notices {
            write_notice(&mut wire, notice).unwrap();
        }
        let mut input = wire.as_slice();
        for notice in notices {
            assert_eq!(read_notice(&mut input).unwrap(), notice);
        }
    }

    #[test]
    fn a_declare_that_cannot_be_read_comes_through() {
        let value = msrp::talker_value(&talker());
        let mut wire = Vec::new();
        write_message(&mut wire, kind::DECLARE, &value[..10]).unwrap();
        write_message(&mut wire, kind::DECLARE, &[1, 2]).unwrap();
        let mut input = wire.as_slice();
        assert_eq!(
            read_request(&mut input).unwrap(),
            Request::Unreadable(Some(talker().stream_id))
        );
        assert_eq!(read_request(&mut input).unwrap(), Request::Unreadable(None));
    }

    #[test]
    fn a_later_hello_keeps_its_first_octets() {
        let mut payload = hello_payload(HELLO);
        payload.extend_from_slice(&[9, 9, 9]);
        let mut wire = Vec::new();
        write_message(&mut wire, kind::HELLO, &payload).unwrap();
        write_message(&mut wire, kind::HELLO, &[0, 2]).unwrap();
        write_notice(&mut wire, &Notice::Available).unwrap();
        let mut input = wire.as_slice();
        assert_eq!(read_notice(&mut input).unwrap(), Notice::Hello(HELLO));
        // Too short to read, so skipped.
        assert_eq!(read_notice(&mut input).unwrap(), Notice::Available);
        assert!(HELLO.has(feature::LOCAL_LISTENERS));
        assert!(!HELLO.has(feature::LISTENERS));
        assert!(
            !Hello {
                features: 0,
                ..HELLO
            }
            .has(feature::LOCAL_LISTENERS)
        );
    }

    #[test]
    fn the_socket_is_the_users_own() {
        if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
            let path = socket_path("enp2s0").unwrap();
            assert!(path.ends_with("avb-mrp-enp2s0.sock"));
        }
    }
}
