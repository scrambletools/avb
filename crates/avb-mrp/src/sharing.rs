//! Sharing a port's MRP participant between programs on one computer.
//! MRP runs per port: two participants on one point-to-point port do not
//! hear each other's Leave, so each would withdraw what the other
//! declared. So the program running a port's participant serves it to the
//! others: they ask it to declare and withdraw their talkers, and it tells
//! them as listeners for their streams come and go, and whether it can
//! declare for them at all, as it cannot while the port runs AVB Lite.
//!
//! It serves on a Unix socket in the user's runtime folder, named for the
//! interface, which only the same user reaches. Each message is an octet
//! of kind, two octets of length, and the payload: a Talker Advertise as
//! MSRP encodes it, a stream ID, or a stream ID and a listener's state.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::msrp::{self, ListenerState, TalkerDeclaration};

mod kind {
    pub const DECLARE: u8 = 1;
    pub const WITHDRAW: u8 = 2;
    pub const LISTENER: u8 = 3;
    pub const UNAVAILABLE: u8 = 4;
    pub const AVAILABLE: u8 = 5;
}

/// The state octet of a listener that left.
const LEFT: u8 = 0xff;

/// What a program asks of the participant it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Declare this talker's stream, or declare it again as it changed.
    Declare(TalkerDeclaration),
    /// Withdraw the stream with this ID.
    Withdraw(u64),
}

/// What the participant tells a program sharing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The listeners for one of its streams, as the port registers them:
    /// their state, or none once they left.
    Listener {
        stream_id: u64,
        state: Option<ListenerState>,
    },
    /// Its talkers cannot be declared now, as while the port runs AVB
    /// Lite; they are withdrawn until it says they can again.
    Unavailable,
    Available,
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

pub fn write_request(out: &mut impl Write, request: &Request) -> io::Result<()> {
    match request {
        Request::Declare(declaration) => {
            write_message(out, kind::DECLARE, &msrp::talker_value(declaration))
        }
        Request::Withdraw(stream_id) => {
            write_message(out, kind::WITHDRAW, &stream_id.to_be_bytes())
        }
    }
}

/// The next request, skipping any it does not know; an error when the
/// other end closed.
pub fn read_request(input: &mut impl Read) -> io::Result<Request> {
    loop {
        let (kind, payload) = read_message(input)?;
        let request = match kind {
            kind::DECLARE => msrp::decode_talker(
                msrp::attribute::TALKER_ADVERTISE,
                &payload,
                crate::Event::New,
            )
            .map(Request::Declare),
            kind::WITHDRAW => stream_id(&payload).map(Request::Withdraw),
            _ => None,
        };
        if let Some(request) = request {
            return Ok(request);
        }
    }
}

pub fn write_notice(out: &mut impl Write, notice: &Notice) -> io::Result<()> {
    match notice {
        Notice::Listener { stream_id, state } => {
            let mut payload = stream_id.to_be_bytes().to_vec();
            payload.push(state.map_or(LEFT, ListenerState::number));
            write_message(out, kind::LISTENER, &payload)
        }
        Notice::Unavailable => write_message(out, kind::UNAVAILABLE, &[]),
        Notice::Available => write_message(out, kind::AVAILABLE, &[]),
    }
}

/// The next notice, skipping any it does not know; an error when the
/// other end closed.
pub fn read_notice(input: &mut impl Read) -> io::Result<Notice> {
    loop {
        let (kind, payload) = read_message(input)?;
        let notice = match kind {
            kind::LISTENER => stream_id(&payload).and_then(|stream_id| {
                let state = *payload.get(8)?;
                Some(Notice::Listener {
                    stream_id,
                    state: (state != LEFT).then(|| ListenerState::from_number(state)),
                })
            }),
            kind::UNAVAILABLE => Some(Notice::Unavailable),
            kind::AVAILABLE => Some(Notice::Available),
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

    #[test]
    fn requests_and_notices_go_across() {
        let mut wire = Vec::new();
        write_request(&mut wire, &Request::Declare(talker())).unwrap();
        // One the other end does not know, which it skips.
        write_message(&mut wire, 99, &[1, 2, 3]).unwrap();
        write_request(&mut wire, &Request::Withdraw(7)).unwrap();
        let mut input = wire.as_slice();
        assert_eq!(
            read_request(&mut input).unwrap(),
            Request::Declare(talker())
        );
        assert_eq!(read_request(&mut input).unwrap(), Request::Withdraw(7));
        assert!(read_request(&mut input).is_err(), "nothing more");

        let notices = [
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
    fn the_socket_is_the_users_own() {
        if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
            let path = socket_path("enp2s0").unwrap();
            assert!(path.ends_with("avb-mrp-enp2s0.sock"));
        }
    }
}
