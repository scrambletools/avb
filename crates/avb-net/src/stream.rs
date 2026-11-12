//! Whole frames for media streams: sending frames a talker builds itself,
//! VLAN tag included, and receiving the frames of one AVTP stream whole,
//! tagged or with the tag already taken off by the interface. Linux only
//! so far; elsewhere opening fails as unsupported.

use std::io;
use std::time::Duration;

use crate::MacAddress;

/// The AVTP ethertype, and the VLAN tag's.
const ETHERTYPE_AVTP: u16 = 0x22f0;
const ETHERTYPE_VLAN: u16 = 0x8100;

/// Sends whole Ethernet frames on one interface.
pub struct FrameSender {
    inner: platform::FrameSender,
}

impl FrameSender {
    /// Opens `interface`. On Linux this needs `CAP_NET_RAW`.
    pub fn open(interface: &str) -> io::Result<Self> {
        Ok(Self {
            inner: platform::FrameSender::open(interface)?,
        })
    }

    /// Sends `frame`, from its destination address on.
    pub fn send(&self, frame: &[u8]) -> io::Result<()> {
        self.inner.send(frame)
    }

    /// Has frames sent with [`FrameSender::send_at`] leave at the times
    /// they carry, on the monotonic clock, where the interface's queue
    /// discipline holds frames until then (fq does, fq_codel sends them
    /// at once), and puts them first among the queue's traffic. An error
    /// leaves the sender as it was.
    pub fn time_departures(&self) -> io::Result<()> {
        self.inner.time_departures()
    }

    /// Sends `frame` to leave at `departure`, in nanoseconds of the
    /// socket's clock: the monotonic clock once
    /// [`FrameSender::time_departures`] has taken, the card's (CLOCK_TAI)
    /// for a sender opened with [`FrameSender::open_for_card`]. A queue
    /// with no room for it says so as ENOBUFS.
    pub fn send_at(&self, frame: &[u8], departure: u64) -> io::Result<()> {
        self.inner.send_at(frame, departure)
    }

    /// Opens `interface` to send frames at launch times the card keeps:
    /// departure times on CLOCK_TAI, which the ETF queue discipline with
    /// offload hands to a card such as the Intel I210, to send each frame
    /// when its PTP clock reaches the frame's time, however busy the
    /// computer, and reports each frame it drops. A socket takes CLOCK_TAI
    /// times only from a program holding CAP_NET_ADMIN, the helper or
    /// this one: without it, or with a helper older than the kind
    /// (raw-ether's `broker::Kind::Launch`), it fails, and a talker opens a
    /// plain sender.
    /// Frames go to the card's queue only after
    /// [`FrameSender::launch_on_card`].
    pub fn open_for_card(interface: &str) -> io::Result<Self> {
        Ok(Self {
            inner: platform::FrameSender::open_for_card(interface)?,
        })
    }

    /// Has frames go to the queue the card launches from, at priority 5,
    /// each sent with [`FrameSender::send_at`] at its launch time; or back
    /// to the streams' usual priority, 3, sent when due. ETF drops a
    /// frame without a time, so only a sender launching on the card sends
    /// at 5, and the link maps 5 alone to that queue. The VLAN tag's
    /// priority code point is written in the frame and does not change.
    pub fn launch_on_card(&self, on: bool) -> io::Result<()> {
        self.inner.launch_on_card(on)
    }

    /// The frames the queue discipline dropped since last asked, their
    /// launch times passed or not valid, as the socket's error queue says.
    /// Zero for a sender not opened for the card.
    pub fn missed_launches(&self) -> u64 {
        self.inner.missed_launches()
    }
}

/// How many frames the queue discipline `interface`'s stream frames go
/// through may hold, where it holds frames until the departure times
/// they carry: fq or etf at the root, or on the first transmit queue
/// under mq or mqprio, where every CPU's frames go once the link is set
/// up for streams. fq takes a packet socket's frames each as a flow of
/// its own, as it does any unconnected socket's, so its limit for the
/// whole queue is the one they meet; etf has none, `u32::MAX`. None
/// where the discipline sends a stamped frame at once, as fq_codel does,
/// or where it cannot tell.
pub fn paced_frames(interface: &str) -> Option<u32> {
    platform::paced_frames(interface)
}

/// Receives the whole frames of one AVTP stream: frames of an AVTP subtype
/// sent to one destination, or either of two, as an AVB Lite listener
/// takes a stream unicast to it or at its multicast address.
pub struct FrameReceiver {
    inner: platform::FrameReceiver,
}

impl FrameReceiver {
    /// Opens `interface` for frames of AVTP `subtype` sent to any of
    /// `destinations` (one or two), and joins those that are group
    /// addresses.
    pub fn open(interface: &str, subtype: u8, destinations: &[MacAddress]) -> io::Result<Self> {
        if destinations.is_empty() || destinations.len() > 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a stream comes to one or two addresses",
            ));
        }
        Ok(Self {
            inner: platform::FrameReceiver::open(interface, subtype, destinations)?,
        })
    }

    /// Waits up to `timeout` (forever when `None`) for a frame and writes
    /// it to `buffer` whole, returning its length.
    pub fn receive(
        &self,
        buffer: &mut [u8],
        timeout: Option<Duration>,
    ) -> io::Result<Option<usize>> {
        self.inner.receive(buffer, timeout)
    }

    /// Waits up to `timeout` (forever when `None`) for a frame at any of
    /// `receivers`, giving the places of those that have one: one thread
    /// taking several streams.
    pub fn ready(
        receivers: &[&FrameReceiver],
        timeout: Option<Duration>,
    ) -> io::Result<Vec<usize>> {
        let inner: Vec<&platform::FrameReceiver> =
            receivers.iter().map(|receiver| &receiver.inner).collect();
        platform::FrameReceiver::ready(&inner, timeout)
    }

    /// The frames the socket had no room for since last asked: its reader
    /// fell behind.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
}

/// Where an AVTP frame's payload starts: after the header, and the VLAN
/// tag when it still has one. `None` for another kind of frame.
pub fn avtp_payload(frame: &[u8]) -> Option<&[u8]> {
    let ethertype = |at: usize| Some(u16::from_be_bytes([*frame.get(at)?, *frame.get(at + 1)?]));
    match ethertype(12)? {
        ETHERTYPE_AVTP => frame.get(14..),
        ETHERTYPE_VLAN if ethertype(16)? == ETHERTYPE_AVTP => frame.get(18..),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    use super::{ETHERTYPE_AVTP, ETHERTYPE_VLAN};
    use crate::MacAddress;

    /// The receive buffer a stream's socket asks for: a quarter of a second
    /// of a class A stream of 1500-octet frames, as the kernel counts them.
    const RECEIVE_ROOM: libc::c_int = 1 << 20;

    /// The priority a stream's frames are sent with: class A's in the usual
    /// mqprio maps. Any priority up to 6 needs no capability.
    const SEND_PRIORITY: libc::c_int = 3;
    /// The priority of frames sent at a departure time: fq's first band in
    /// its default map, ahead of the queue's other traffic, and the
    /// highest a program may set without a capability.
    const TIMED_PRIORITY: libc::c_int = 6;
    /// The priority of frames the card launches at their times: one no
    /// other sender uses, so that the link can map it alone to the queue
    /// with ETF, which drops frames without a time.
    const LAUNCH_PRIORITY: libc::c_int = 5;
    /// SO_TXTIME and its control message, the same on every architecture
    /// but alpha, mips, parisc and sparc.
    const SO_TXTIME: libc::c_int = 61;
    const SCM_TXTIME: libc::c_int = SO_TXTIME;
    /// An error queue message from a queue discipline keeping departure
    /// times: a frame dropped for its time.
    const SO_EE_ORIGIN_TXTIME: u8 = 6;
    /// The control message a packet socket's error queue carries it in.
    const PACKET_TX_TIMESTAMP: libc::c_int = 16;

    /// Sets an option of `fd` to `value`.
    fn set_option<T>(
        fd: &OwnedFd,
        level: libc::c_int,
        name: libc::c_int,
        value: &T,
    ) -> io::Result<()> {
        // SAFETY: `value` is valid for its size through the call, and the
        // length passed is that size.
        let set = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                level,
                name,
                (value as *const T).cast(),
                size_of::<T>() as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open_raw(interface: &str, kind: crate::broker::Kind) -> io::Result<(OwnedFd, i32)> {
        let index = crate::interfaces::index_of(interface).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no interface named {interface}"),
            )
        })?;
        let fd = crate::broker::open(kind)?;
        Ok((fd, index))
    }

    fn bind(fd: &OwnedFd, index: i32, protocol: u16) -> io::Result<()> {
        // SAFETY: sockaddr_ll is plain data, valid when zeroed.
        let mut address: libc::sockaddr_ll = unsafe { zeroed() };
        address.sll_family = libc::AF_PACKET as u16;
        address.sll_protocol = protocol.to_be();
        address.sll_ifindex = index;
        // SAFETY: `address` is a valid sockaddr_ll that lives through the
        // call, and the length passed is its size.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A queue discipline found: its handle, its parent's, its kind and,
    /// for fq, how many frames it holds at most.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Discipline {
        pub handle: u32,
        pub parent: u32,
        pub kind: String,
        pub limit: Option<u32>,
    }

    /// rtnetlink's message types and flags, and the kind's attribute.
    const RTM_NEWQDISC: u16 = 36;
    const RTM_GETQDISC: u16 = 38;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_DUMP: u16 = 0x300;
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const TCA_KIND: u16 = 1;
    const TCA_OPTIONS: u16 = 2;
    const TCA_FQ_PLIMIT: u16 = 1;
    /// fq's limit where it does not say.
    const FQ_LIMIT: u32 = 10_000;
    /// An attribute's type, less the nested and byte order flags.
    const TYPE_MASK: u16 = 0x3fff;
    /// A message's header and a tcmsg.
    const HEADER_LEN: usize = 16;
    const TCMSG_LEN: usize = 20;
    /// The root's parent.
    const ROOT: u32 = u32::MAX;

    pub fn paced_frames(interface: &str) -> Option<u32> {
        let index = crate::interfaces::index_of(interface)?;
        paced(&disciplines(index).ok()?)
    }

    /// The frames a flow may queue where `found`, an interface's
    /// disciplines, hold frames until their departure times on its first
    /// transmit queue.
    pub(super) fn paced(found: &[Discipline]) -> Option<u32> {
        let frames = |discipline: &Discipline| match discipline.kind.as_str() {
            "fq" => Some(discipline.limit.unwrap_or(FQ_LIMIT)),
            "etf" => Some(u32::MAX),
            _ => None,
        };
        let root = found.iter().find(|discipline| discipline.parent == ROOT)?;
        match root.kind.as_str() {
            "mq" | "mqprio" => {
                // The first queue's class is 1 under the root's handle.
                let first = (root.handle & 0xffff_0000) | 1;
                found
                    .iter()
                    .find(|discipline| discipline.parent == first)
                    .and_then(frames)
            }
            _ => frames(root),
        }
    }

    /// Every queue discipline on interface `index`, asked of rtnetlink,
    /// which needs no privilege.
    fn disciplines(index: i32) -> io::Result<Vec<Discipline>> {
        // SAFETY: socket has no pointer arguments; a descriptor it returns
        // is ours alone.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is an open descriptor no one else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut request = [0u8; HEADER_LEN + TCMSG_LEN];
        request[0..4].copy_from_slice(&((HEADER_LEN + TCMSG_LEN) as u32).to_ne_bytes());
        request[4..6].copy_from_slice(&RTM_GETQDISC.to_ne_bytes());
        request[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        request[8..12].copy_from_slice(&1u32.to_ne_bytes());
        // SAFETY: `request` is valid for its length through the call.
        let sent = unsafe { libc::send(fd.as_raw_fd(), request.as_ptr().cast(), request.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut found = Vec::new();
        let mut buffer = vec![0u8; 32 * 1024];
        loop {
            // SAFETY: `buffer` is valid for its length through the call.
            let got =
                unsafe { libc::recv(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
            if got < 0 {
                return Err(io::Error::last_os_error());
            }
            if got == 0 || read_disciplines(&buffer[..got as usize], index, &mut found)? {
                return Ok(found);
            }
        }
    }

    /// The attributes in `bytes`, each its type and value.
    fn attributes(mut bytes: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
        std::iter::from_fn(move || {
            let size = usize::from(u16::from_ne_bytes(bytes.get(0..2)?.try_into().ok()?));
            let attribute = u16::from_ne_bytes(bytes.get(2..4)?.try_into().ok()?) & TYPE_MASK;
            let value = bytes.get(4..size)?;
            bytes = bytes.get(size.next_multiple_of(4)..).unwrap_or(&[]);
            Some((attribute, value))
        })
    }

    /// Reads one reply's messages into `found`, those of interface
    /// `index`; true once the dump is done.
    pub(super) fn read_disciplines(
        mut bytes: &[u8],
        index: i32,
        found: &mut Vec<Discipline>,
    ) -> io::Result<bool> {
        let word = |bytes: &[u8], at: usize| {
            bytes
                .get(at..at + 4)
                .map(|word| u32::from_ne_bytes(word.try_into().unwrap_or_default()))
        };
        let half = |bytes: &[u8], at: usize| {
            bytes
                .get(at..at + 2)
                .map(|half| u16::from_ne_bytes(half.try_into().unwrap_or_default()))
        };
        while let (Some(length), Some(kind)) = (word(bytes, 0), half(bytes, 4)) {
            let length = length as usize;
            if length < HEADER_LEN || length > bytes.len() {
                break;
            }
            let message = &bytes[..length];
            match kind {
                NLMSG_DONE => return Ok(true),
                NLMSG_ERROR => {
                    let code = word(message, HEADER_LEN).unwrap_or(0) as i32;
                    return Err(io::Error::from_raw_os_error(-code));
                }
                RTM_NEWQDISC if message.len() >= HEADER_LEN + TCMSG_LEN => {
                    let on = word(message, HEADER_LEN + 4).unwrap_or(0) as i32;
                    let handle = word(message, HEADER_LEN + 8).unwrap_or(0);
                    let parent = word(message, HEADER_LEN + 12).unwrap_or(0);
                    if on != index {
                        bytes = bytes.get(length.next_multiple_of(4)..).unwrap_or(&[]);
                        continue;
                    }
                    let mut named = None;
                    let mut limit = None;
                    for (attribute, value) in attributes(&message[HEADER_LEN + TCMSG_LEN..]) {
                        match attribute {
                            TCA_KIND => {
                                let name = value.split(|byte| *byte == 0).next().unwrap_or(value);
                                named = Some(String::from_utf8_lossy(name).into_owned());
                            }
                            TCA_OPTIONS => {
                                limit = attributes(value)
                                    .find(|(option, _)| *option == TCA_FQ_PLIMIT)
                                    .and_then(|(_, limit)| word(limit, 0));
                            }
                            _ => {}
                        }
                    }
                    if let Some(named) = named {
                        found.push(Discipline {
                            handle,
                            parent,
                            limit: limit.filter(|_| named == "fq"),
                            kind: named,
                        });
                    }
                }
                _ => {}
            }
            bytes = bytes.get(length.next_multiple_of(4)..).unwrap_or(&[]);
        }
        Ok(false)
    }

    pub struct FrameSender {
        fd: OwnedFd,
        /// Where each frame goes: the interface, the frame named as AVTP.
        to: libc::sockaddr_ll,
    }

    impl FrameSender {
        pub fn open(interface: &str) -> io::Result<Self> {
            Self::open_kind(interface, crate::broker::Kind::Raw)
        }

        fn open_kind(interface: &str, kind: crate::broker::Kind) -> io::Result<Self> {
            let (fd, index) = open_raw(interface, kind)?;
            // Bound for no protocol: it sends, and hears nothing.
            bind(&fd, index, 0)?;
            // Each frame is sent named as AVTP, whatever its VLAN tag, so
            // that the kernel's hash of it, by which fq_codel files it in a
            // flow and a transmit queue is chosen, is the same for every
            // frame: a stream keeps to one flow and one queue, and its
            // frames keep their order when the queue backs up. Named by
            // their first ethertype, the VLAN tag's, the hash took in the
            // AVTP header and put nearly every frame in a flow of its own.
            // The priority does not help there: fq_codel files by it only
            // where it names fq_codel's own handle, which a link unit's
            // fq_codel does not have. Only an nftables egress rule matching
            // `meta protocol` sees the name rather than the tag.
            // SAFETY: sockaddr_ll is plain data, valid when zeroed.
            let mut to: libc::sockaddr_ll = unsafe { zeroed() };
            to.sll_family = libc::AF_PACKET as u16;
            to.sll_protocol = ETHERTYPE_AVTP.to_be();
            to.sll_ifindex = index;
            // Priority 3, class A's in the usual mqprio maps. A refusal
            // leaves it as it was.
            let priority = SEND_PRIORITY;
            // SAFETY: `priority` is a c_int that lives through the call, and
            // the length passed is its size.
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PRIORITY,
                    (&raw const priority).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            Ok(Self { fd, to })
        }

        pub fn send(&self, frame: &[u8]) -> io::Result<()> {
            // SAFETY: `frame` and `to` are valid for their lengths through
            // the call.
            let sent = unsafe {
                libc::sendto(
                    self.fd.as_raw_fd(),
                    frame.as_ptr().cast(),
                    frame.len(),
                    0,
                    (&raw const self.to).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn time_departures(&self) -> io::Result<()> {
            // The monotonic clock is the one departure times need no
            // capability for, and the one fq keeps them on.
            let timing = libc::sock_txtime {
                clockid: libc::CLOCK_MONOTONIC,
                flags: 0,
            };
            set_option(&self.fd, libc::SOL_SOCKET, SO_TXTIME, &timing)?;
            // Without its own priority a timed frame still keeps its time,
            // behind the queue's other traffic.
            let _ = set_option(
                &self.fd,
                libc::SOL_SOCKET,
                libc::SO_PRIORITY,
                &TIMED_PRIORITY,
            );
            Ok(())
        }

        pub fn send_at(&self, frame: &[u8], departure: u64) -> io::Result<()> {
            let mut part = libc::iovec {
                iov_base: frame.as_ptr().cast_mut().cast(),
                iov_len: frame.len(),
            };
            // Room for one control message of a u64, aligned as its header.
            let mut control = [0u64; 4];
            // SAFETY: CMSG_SPACE only computes a length.
            let space = unsafe { libc::CMSG_SPACE(size_of::<u64>() as libc::c_uint) } as usize;
            // SAFETY: msghdr is plain data, valid when zeroed.
            let mut message: libc::msghdr = unsafe { zeroed() };
            // Named as AVTP, as `send` names it; the kernel only reads it.
            message.msg_name = (&raw const self.to).cast_mut().cast();
            message.msg_namelen = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
            message.msg_iov = &raw mut part;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = space.min(size_of_val(&control)) as _;
            // SAFETY: the control buffer holds a whole header and its u64,
            // as CMSG_SPACE said, so the first header and its data lie in
            // it; the data may sit unaligned for a u64, so it is written
            // unaligned.
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&raw const message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = SCM_TXTIME;
                (*header).cmsg_len = libc::CMSG_LEN(size_of::<u64>() as libc::c_uint) as _;
                libc::CMSG_DATA(header)
                    .cast::<u64>()
                    .write_unaligned(departure);
            }
            // SAFETY: `message` and what it points to, the frame and the
            // control buffer, live through the call.
            let sent = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &raw const message, 0) };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn open_for_card(interface: &str) -> io::Result<Self> {
            // Its departure times on CLOCK_TAI, set by whoever opens it,
            // the helper or this program, holding CAP_NET_ADMIN.
            Self::open_kind(interface, crate::broker::Kind::Launch)
        }

        pub fn launch_on_card(&self, on: bool) -> io::Result<()> {
            let priority = if on { LAUNCH_PRIORITY } else { SEND_PRIORITY };
            set_option(&self.fd, libc::SOL_SOCKET, libc::SO_PRIORITY, &priority)
        }

        pub fn missed_launches(&self) -> u64 {
            let mut missed = 0;
            loop {
                // The frame comes back too; its first octets are enough.
                let mut data = [0u8; 64];
                let mut part = libc::iovec {
                    iov_base: data.as_mut_ptr().cast(),
                    iov_len: data.len(),
                };
                // Room for a few control messages, aligned as their headers.
                let mut control = [0u64; 32];
                // SAFETY: msghdr is plain data, valid when zeroed.
                let mut message: libc::msghdr = unsafe { zeroed() };
                message.msg_iov = &raw mut part;
                message.msg_iovlen = 1;
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = size_of_val(&control) as _;
                // SAFETY: `message` and the buffers it points to live
                // through the call, and their lengths are theirs.
                let read = unsafe {
                    libc::recvmsg(
                        self.fd.as_raw_fd(),
                        &raw mut message,
                        libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT,
                    )
                };
                if read < 0 {
                    return missed;
                }
                // SAFETY: the kernel filled the control buffer up to
                // msg_controllen, and the CMSG macros keep within it.
                unsafe {
                    let mut header = libc::CMSG_FIRSTHDR(&raw const message);
                    while !header.is_null() {
                        let length = (*header).cmsg_len as usize;
                        let room =
                            libc::CMSG_LEN(size_of::<libc::sock_extended_err>() as _) as usize;
                        if (*header).cmsg_level == libc::SOL_PACKET
                            && (*header).cmsg_type == PACKET_TX_TIMESTAMP
                            && length >= room
                        {
                            let error = libc::CMSG_DATA(header)
                                .cast::<libc::sock_extended_err>()
                                .read_unaligned();
                            if error.ee_origin == SO_EE_ORIGIN_TXTIME {
                                missed += 1;
                            }
                        }
                        header = libc::CMSG_NXTHDR(&raw const message, header);
                    }
                }
            }
        }
    }

    pub struct FrameReceiver {
        fd: OwnedFd,
    }

    impl FrameReceiver {
        pub fn open(interface: &str, subtype: u8, destinations: &[MacAddress]) -> io::Result<Self> {
            let (fd, index) = open_raw(interface, crate::broker::Kind::Raw)?;
            // The filter goes on before the socket hears anything: the
            // destination, either of two, then the AVTP subtype, after a
            // VLAN tag or not.
            let step = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
            let halves = |mac: MacAddress| {
                let mac = mac.0;
                (
                    u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]),
                    u32::from(u16::from_be_bytes([mac[4], mac[5]])),
                )
            };
            let (high, low) = halves(destinations[0]);
            // With one destination the second test repeats the first.
            let (other_high, other_low) = halves(*destinations.get(1).unwrap_or(&destinations[0]));
            let program = [
                step(0x20, 0, 0, 0),                         // 0: ld [0]
                step(0x15, 0, 2, high),                      // 1: jeq #first[0..4], else 4
                step(0x28, 0, 0, 4),                         // 2: ldh [4]
                step(0x15, 4, 0, low),                       // 3: jeq #first[4..6], 8
                step(0x20, 0, 0, 0),                         // 4: ld [0]
                step(0x15, 0, 12, other_high),               // 5: jeq #second[0..4], else 18
                step(0x28, 0, 0, 4),                         // 6: ldh [4]
                step(0x15, 0, 10, other_low),                // 7: jeq #second[4..6], else 18
                step(0x28, 0, 0, 12),                        // 8: ldh [12]
                step(0x15, 0, 2, u32::from(ETHERTYPE_AVTP)), // 9: jeq #avtp
                step(0x30, 0, 0, 14),                        // 10: ldb [14]
                step(0x05, 0, 0, 4),                         // 11: ja 16
                step(0x15, 0, 5, u32::from(ETHERTYPE_VLAN)), // 12: jeq #vlan
                step(0x28, 0, 0, 16),                        // 13: ldh [16]
                step(0x15, 0, 3, u32::from(ETHERTYPE_AVTP)), // 14: jeq #avtp
                step(0x30, 0, 0, 18),                        // 15: ldb [18]
                step(0x15, 0, 1, u32::from(subtype)),        // 16: jeq #subtype
                step(0x06, 0, 0, u32::MAX),                  // 17: ret #-1
                step(0x06, 0, 0, 0),                         // 18: ret #0
            ];
            let filter = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr().cast_mut(),
            };
            // SAFETY: `filter` points at `program`, which both live through
            // the call; the kernel copies the program and does not write it.
            let attached = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ATTACH_FILTER,
                    (&raw const filter).cast(),
                    size_of::<libc::sock_fprog>() as libc::socklen_t,
                )
            };
            if attached < 0 {
                return Err(io::Error::last_os_error());
            }
            bind(&fd, index, libc::ETH_P_ALL as u16)?;
            // A membership of the socket's, which ends with it: a multicast
            // address the interface takes, or the interface promiscuous.
            let join = |kind: i32, mac: Option<[u8; 6]>| {
                // SAFETY: packet_mreq is plain data, valid when zeroed.
                let mut request: libc::packet_mreq = unsafe { zeroed() };
                request.mr_ifindex = index;
                request.mr_type = kind as u16;
                if let Some(mac) = mac {
                    request.mr_alen = 6;
                    request.mr_address[..6].copy_from_slice(&mac);
                }
                // SAFETY: `request` is a valid packet_mreq that lives
                // through the call, and the length passed is its size.
                let joined = unsafe {
                    libc::setsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_PACKET,
                        libc::PACKET_ADD_MEMBERSHIP,
                        (&raw const request).cast(),
                        size_of::<libc::packet_mreq>() as libc::socklen_t,
                    )
                };
                if joined < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            };
            for destination in destinations {
                let mac = destination.0;
                if mac[0] & 1 == 0 {
                    continue;
                }
                join(libc::PACKET_MR_MULTICAST, Some(mac))?;
            }
            // Streams come tagged with their VLAN, which an interface that
            // filters VLANs in hardware, as the I210 does, drops unless the
            // VLAN is registered with it, as only an administrator can.
            // Promiscuous, it takes them all.
            if crate::interfaces::filters_vlans(interface).unwrap_or(false) {
                join(libc::PACKET_MR_PROMISC, None)?;
            }
            // Room for a reader held up a while: as much as the system lets
            // a socket ask for, up to RECEIVE_ROOM, rather than the default
            // of a few tens of milliseconds of a stream.
            let room = RECEIVE_ROOM;
            // SAFETY: `room` is a c_int that lives through the call, and the
            // length passed is its size.
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&raw const room).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            Ok(Self { fd })
        }

        pub fn dropped(&self) -> u64 {
            // SAFETY: tpacket_stats is plain data, valid when zeroed.
            let mut stats: libc::tpacket_stats = unsafe { zeroed() };
            let mut length = size_of::<libc::tpacket_stats>() as libc::socklen_t;
            // SAFETY: `stats` and `length` live through the call, and the
            // length says how much of `stats` the kernel may write; reading
            // the counts resets them.
            let read = unsafe {
                libc::getsockopt(
                    self.fd.as_raw_fd(),
                    libc::SOL_PACKET,
                    libc::PACKET_STATISTICS,
                    (&raw mut stats).cast(),
                    &raw mut length,
                )
            };
            if read < 0 {
                0
            } else {
                u64::from(stats.tp_drops)
            }
        }

        pub fn receive(
            &self,
            buffer: &mut [u8],
            timeout: Option<Duration>,
        ) -> io::Result<Option<usize>> {
            let milliseconds = match timeout {
                Some(timeout) => {
                    timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
                }
                None => -1,
            };
            let mut poll = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, which lives through the call.
            let ready = unsafe { libc::poll(&raw mut poll, 1, milliseconds) };
            if ready <= 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    _ if ready == 0 => Ok(None),
                    io::ErrorKind::Interrupted => Ok(None),
                    _ => Err(error),
                };
            }
            // SAFETY: `buffer` is valid for its length through the call.
            let received = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if received < 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                    _ => Err(error),
                };
            }
            Ok(Some(received as usize))
        }

        pub fn ready(receivers: &[&Self], timeout: Option<Duration>) -> io::Result<Vec<usize>> {
            let milliseconds = match timeout {
                Some(timeout) => {
                    timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
                }
                None => -1,
            };
            let mut polls: Vec<libc::pollfd> = receivers
                .iter()
                .map(|receiver| libc::pollfd {
                    fd: receiver.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: `polls` holds valid pollfds and lives through the call.
            let ready = unsafe {
                libc::poll(
                    polls.as_mut_ptr(),
                    polls.len() as libc::nfds_t,
                    milliseconds,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::Interrupted => Ok(Vec::new()),
                    _ => Err(error),
                };
            }
            Ok(polls
                .iter()
                .enumerate()
                .filter(|(_, poll)| poll.revents & libc::POLLIN != 0)
                .map(|(place, _)| place)
                .collect())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::time::Duration;

    use crate::MacAddress;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "media streams are not supported on this system yet",
        )
    }

    pub fn paced_frames(_interface: &str) -> Option<u32> {
        None
    }

    pub struct FrameSender;

    impl FrameSender {
        pub fn open(_interface: &str) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn send(&self, _frame: &[u8]) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn time_departures(&self) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn send_at(&self, _frame: &[u8], _departure: u64) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn open_for_card(_interface: &str) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn launch_on_card(&self, _on: bool) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn missed_launches(&self) -> u64 {
            0
        }
    }

    pub struct FrameReceiver;

    impl FrameReceiver {
        pub fn open(
            _interface: &str,
            _subtype: u8,
            _destinations: &[MacAddress],
        ) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn receive(
            &self,
            _buffer: &mut [u8],
            _timeout: Option<Duration>,
        ) -> io::Result<Option<usize>> {
            Err(unsupported())
        }

        pub fn ready(_receivers: &[&Self], _timeout: Option<Duration>) -> io::Result<Vec<usize>> {
            Err(unsupported())
        }

        pub fn dropped(&self) -> u64 {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn a_paced_first_queue_has_fq_or_etf_under_mq_or_at_the_root() {
        use platform::{Discipline, paced};
        let discipline = |handle, parent, kind: &str, limit| Discipline {
            handle,
            parent,
            kind: kind.into(),
            limit,
        };
        let root = u32::MAX;
        // The kernel's own mq 0:, its first queue's fq at :1.
        assert_eq!(
            paced(&[
                discipline(0, root, "mq", None),
                discipline(0x8001_0000, 1, "fq", Some(100)),
            ]),
            Some(100)
        );
        assert_eq!(
            paced(&[
                discipline(0, root, "mq", None),
                discipline(0x8001_0000, 1, "fq_codel", None),
                discipline(0x8002_0000, 2, "fq", Some(100)),
            ]),
            None
        );
        // mq 1: with fq at 1:1, as the link units made it before they
        // set fq_codel there (2026-11-09).
        assert_eq!(
            paced(&[
                discipline(0x0001_0000, root, "mq", None),
                discipline(0x800e_0000, 0x0001_0001, "fq", Some(2000)),
            ]),
            Some(2000)
        );
        assert_eq!(
            paced(&[discipline(0x8001_0000, root, "fq", None)]),
            Some(10_000)
        );
        assert_eq!(
            paced(&[discipline(0x8001_0000, root, "etf", None)]),
            Some(u32::MAX)
        );
        assert_eq!(paced(&[discipline(0, root, "noqueue", None)]), None);
        assert_eq!(paced(&[]), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rtnetlink_replies_give_the_interfaces_own_disciplines() {
        use platform::{Discipline, read_disciplines};
        fn attribute(kind: u16, value: &[u8]) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&((4 + value.len()) as u16).to_ne_bytes());
            bytes.extend_from_slice(&kind.to_ne_bytes());
            bytes.extend_from_slice(value);
            bytes.resize(bytes.len().next_multiple_of(4), 0);
            bytes
        }
        fn message(
            index: i32,
            handle: u32,
            parent: u32,
            name: &str,
            limit: Option<u32>,
        ) -> Vec<u8> {
            let mut body = vec![0u8; 4];
            body.extend_from_slice(&index.to_ne_bytes());
            body.extend_from_slice(&handle.to_ne_bytes());
            body.extend_from_slice(&parent.to_ne_bytes());
            body.extend_from_slice(&0u32.to_ne_bytes());
            body.extend(attribute(1, format!("{name}\0").as_bytes()));
            if let Some(limit) = limit {
                let mut options = attribute(1, &10_000u32.to_ne_bytes());
                options.extend(attribute(2, &limit.to_ne_bytes()));
                // Nested, as the kernel flags it.
                body.extend(attribute(2 | 0x8000, &options));
            }
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
            bytes.extend_from_slice(&36u16.to_ne_bytes());
            bytes.extend_from_slice(&0u16.to_ne_bytes());
            bytes.extend_from_slice(&[0u8; 8]);
            bytes.extend_from_slice(&body);
            bytes
        }
        let mut reply = message(2, 0, u32::MAX, "mq", None);
        reply.extend(message(3, 0, u32::MAX, "noqueue", None));
        reply.extend(message(2, 0x8001_0000, 1, "fq", Some(2000)));
        let mut found = Vec::new();
        assert!(!read_disciplines(&reply, 2, &mut found).expect("read"));
        assert_eq!(
            found,
            [
                Discipline {
                    handle: 0,
                    parent: u32::MAX,
                    kind: "mq".into(),
                    limit: None,
                },
                Discipline {
                    handle: 0x8001_0000,
                    parent: 1,
                    kind: "fq".into(),
                    limit: Some(10_000),
                },
            ]
        );
        let mut done = 20u32.to_ne_bytes().to_vec();
        done.extend_from_slice(&3u16.to_ne_bytes());
        done.extend_from_slice(&[0u8; 14]);
        assert!(read_disciplines(&done, 2, &mut found).expect("read"));
    }

    #[test]
    fn payloads_start_after_the_tag_if_there_is_one() {
        let mut frame = vec![0u8; 12];
        frame.extend_from_slice(&[0x22, 0xf0, 0x02, 0x81]);
        assert_eq!(avtp_payload(&frame), Some(&[0x02, 0x81][..]));
        let mut tagged = vec![0u8; 12];
        tagged.extend_from_slice(&[0x81, 0x00, 0x60, 0x02, 0x22, 0xf0, 0x02]);
        assert_eq!(avtp_payload(&tagged), Some(&[0x02][..]));
        let mut other = vec![0u8; 12];
        other.extend_from_slice(&[0x08, 0x00, 0x45]);
        assert_eq!(avtp_payload(&other), None);
        assert_eq!(avtp_payload(&[0; 5]), None);
    }
}
