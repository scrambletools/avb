//! Whole frames crossing an interface, both ways, for capturing: those of
//! chosen ethertypes the interface receives and sends, a bridge port's
//! forwarded frames among them, each with the time the kernel saw it and
//! its VLAN tag put back where the kernel had taken it off. The filter
//! runs in the kernel, so frames of other ethertypes cost the program
//! nothing, and the kernel hands frames over in blocks of a ring shared
//! with the program, so a busy capture wakes it a few times a second
//! rather than once a frame. Linux only so far; elsewhere opening fails
//! as unsupported.

use std::io;
use std::time::Duration;

/// The ethertypes of a VLAN tag: 802.1Q's, and 802.1ad's service tag.
const ETHERTYPE_VLAN: u16 = 0x8100;
const ETHERTYPE_SERVICE_VLAN: u16 = 0x88a8;

/// Which way a captured frame crossed the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Received on the interface.
    Inbound,
    /// Sent out of it, by this computer or by a bridge forwarding it.
    Outbound,
}

/// Whom a received frame was addressed to, as the interface saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressed {
    /// This computer.
    Unicast,
    Multicast,
    Broadcast,
    /// Another computer: heard only because the interface is promiscuous.
    Elsewhere,
}

/// A frame a [`Capture`] took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Captured {
    /// Octets of the frame written to the buffer, from its destination
    /// address on: no more than the capture's snap length, or the buffer.
    pub length: usize,
    /// The frame's whole length, its VLAN tag included.
    pub original: usize,
    pub direction: Direction,
    /// `None` for an outbound frame.
    pub addressed: Option<Addressed>,
    /// When the kernel saw the frame, since the Unix epoch by the system
    /// clock.
    pub time: Duration,
}

/// What a capture took and what its ring had no room for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Statistics {
    /// Frames that passed the filter, those dropped included.
    pub passed: u64,
    /// Frames dropped because the ring was full: the program fell behind.
    pub dropped: u64,
}

/// Takes the frames of chosen ethertypes crossing one interface.
pub struct Capture {
    inner: platform::Capture,
}

impl Capture {
    /// Opens `interface` for frames of any of `ethertypes`, tagged or not,
    /// or of every ethertype when there are none, keeping at most `snap`
    /// octets of each. Frames wait to be read in a ring of about `room`
    /// octets, memory the kernel lends the program; when it fills, frames
    /// are dropped and counted. `promiscuous` has the interface hear frames
    /// for other computers too, as long as the capture is open; a bridge
    /// port already does. On Linux this needs `CAP_NET_RAW`.
    pub fn open(
        interface: &str,
        ethertypes: &[u16],
        snap: u32,
        promiscuous: bool,
        room: usize,
    ) -> io::Result<Self> {
        if ethertypes.len() > MOST_ETHERTYPES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a capture takes at most {MOST_ETHERTYPES} ethertypes"),
            ));
        }
        Ok(Self {
            inner: platform::Capture::open(interface, ethertypes, snap.max(1), promiscuous, room)?,
        })
    }

    /// Writes the next frame to `buffer`, waiting up to `timeout` (forever
    /// when `None`) for one. `None` when the time ran out or a signal
    /// interrupted the wait. A zero timeout never waits, for taking every
    /// frame already there.
    pub fn receive(
        &self,
        buffer: &mut [u8],
        timeout: Option<Duration>,
    ) -> io::Result<Option<Captured>> {
        self.inner.receive(buffer, timeout)
    }

    /// Waits up to `timeout` (forever when `None`) for frames at any of
    /// `captures`, giving the places of those that have some.
    pub fn ready(captures: &[&Capture], timeout: Option<Duration>) -> io::Result<Vec<usize>> {
        let inner: Vec<&platform::Capture> =
            captures.iter().map(|capture| &capture.inner).collect();
        platform::Capture::ready(&inner, timeout)
    }

    /// What the capture took since last asked.
    pub fn statistics(&self) -> io::Result<Statistics> {
        self.inner.statistics()
    }
}

/// The most ethertypes a filter can test: its jumps reach 255 steps.
const MOST_ETHERTYPES: usize = 100;

/// A classic BPF instruction: code, jump if true, jump if false, constant.
type Step = (u16, u8, u8, u32);

/// The filter keeping frames of any of `ethertypes`, after a VLAN tag or
/// not, cut to `snap` octets; every frame when there are none. A packet
/// socket's program sees a received frame with its tag already taken off,
/// a sent one with the tag still in it.
fn program(ethertypes: &[u16], snap: u32) -> Vec<Step> {
    const LOAD_HALF: u16 = 0x28; // ldh [k]
    const JUMP_EQUAL: u16 = 0x15; // jeq #k
    const RETURN: u16 = 0x06; // ret #k
    if ethertypes.is_empty() {
        return vec![(RETURN, 0, 0, snap)];
    }
    let count = ethertypes.len();
    // The tagged frames' tests start at `tagged`, and every match jumps to
    // the last step, `keep`. A jump counts from the step after its own.
    let tagged = 4 + count;
    let keep = 6 + 2 * count;
    let mut steps = vec![
        (LOAD_HALF, 0, 0, 12),
        (JUMP_EQUAL, (tagged - 2) as u8, 0, u32::from(ETHERTYPE_VLAN)),
        (
            JUMP_EQUAL,
            (tagged - 3) as u8,
            0,
            u32::from(ETHERTYPE_SERVICE_VLAN),
        ),
    ];
    for (place, ethertype) in ethertypes.iter().enumerate() {
        let at = 3 + place;
        steps.push((JUMP_EQUAL, (keep - at - 1) as u8, 0, u32::from(*ethertype)));
    }
    steps.push((RETURN, 0, 0, 0));
    steps.push((LOAD_HALF, 0, 0, 16));
    for (place, ethertype) in ethertypes.iter().enumerate() {
        let at = tagged + 1 + place;
        steps.push((JUMP_EQUAL, (keep - at - 1) as u8, 0, u32::from(*ethertype)));
    }
    steps.push((RETURN, 0, 0, 0));
    steps.push((RETURN, 0, 0, snap));
    steps
}

/// Puts a VLAN tag of `tpid` and `tci` back after the source address of
/// the `length` octets of frame in `buffer`, as it crossed the wire,
/// cutting the end where the buffer has no room. The frame's new length.
fn put_back_tag(buffer: &mut [u8], length: usize, tpid: u16, tci: u16) -> usize {
    if length < 12 || buffer.len() < 16 {
        return length;
    }
    let tagged = (length + 4).min(buffer.len());
    buffer.copy_within(12..tagged - 4, 16);
    buffer[12..14].copy_from_slice(&tpid.to_be_bytes());
    buffer[14..16].copy_from_slice(&tci.to_be_bytes());
    tagged
}

/// Where things sit in a block of the kernel's capture ring, version 3 of
/// its layout (`tpacket_block_desc` and `tpacket3_hdr` in
/// linux/if_packet.h), all in the computer's own byte order. A block
/// starts with its status, its count of frames and where the first is;
/// each frame with a header saying where the next is, then the link-layer
/// address it came from or went to, then the frame.
mod ring {
    pub const BLOCK_STATUS: usize = 8;
    pub const BLOCK_FRAMES: usize = 12;
    pub const BLOCK_FIRST: usize = 16;

    pub const NEXT: usize = 0;
    pub const SECONDS: usize = 4;
    pub const NANOSECONDS: usize = 8;
    pub const SNAP: usize = 12;
    pub const LENGTH: usize = 16;
    pub const STATUS: usize = 20;
    pub const MAC: usize = 24;
    pub const VLAN_TCI: usize = 32;
    pub const VLAN_TPID: usize = 36;
    /// The link-layer address, after the 48-octet header; within it, the
    /// kind of packet.
    pub const PACKET_TYPE: usize = 48 + 10;

    pub const USER: u32 = 1;
    pub const VLAN_VALID: u32 = 1 << 4;
    pub const VLAN_TPID_VALID: u32 = 1 << 6;

    pub const HOST: u8 = 0;
    pub const BROADCAST: u8 = 1;
    pub const MULTICAST: u8 = 2;
    pub const OUTGOING: u8 = 4;
}

fn word(block: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(block.get(at..at + 4)?.try_into().ok()?))
}

fn half(block: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(block.get(at..at + 2)?.try_into().ok()?))
}

/// How many frames a ring `block` holds and where the first starts.
fn block_frames(block: &[u8]) -> Option<(u32, usize)> {
    Some((
        word(block, ring::BLOCK_FRAMES)?,
        word(block, ring::BLOCK_FIRST)? as usize,
    ))
}

/// Copies the frame at `at` in a ring `block` to `buffer`, at most `snap`
/// octets of it, its VLAN tag put back: the frame as captured, and how
/// far on the next one starts. `None` when the block is not laid out as
/// it should be.
fn read_frame(block: &[u8], at: usize, buffer: &mut [u8], snap: u32) -> Option<(Captured, usize)> {
    let field = |offset: usize| word(block, at + offset);
    let next = field(ring::NEXT)? as usize;
    let time = Duration::new(
        u64::from(field(ring::SECONDS)?),
        field(ring::NANOSECONDS)?.min(999_999_999),
    );
    let kept = field(ring::SNAP)? as usize;
    let mut original = field(ring::LENGTH)? as usize;
    let status = field(ring::STATUS)?;
    let start = at + usize::from(half(block, at + ring::MAC)?);
    let frame = block.get(start..start + kept)?;
    let mut length = kept.min(buffer.len());
    buffer[..length].copy_from_slice(&frame[..length]);
    if status & ring::VLAN_VALID != 0 {
        let tci = field(ring::VLAN_TCI)? as u16;
        let tpid = if status & ring::VLAN_TPID_VALID != 0 {
            half(block, at + ring::VLAN_TPID)?
        } else {
            ETHERTYPE_VLAN
        };
        length = put_back_tag(buffer, length, tpid, tci);
        original += 4;
    }
    let (direction, addressed) = match *block.get(at + ring::PACKET_TYPE)? {
        ring::OUTGOING => (Direction::Outbound, None),
        ring::HOST => (Direction::Inbound, Some(Addressed::Unicast)),
        ring::MULTICAST => (Direction::Inbound, Some(Addressed::Multicast)),
        ring::BROADCAST => (Direction::Inbound, Some(Addressed::Broadcast)),
        _ => (Direction::Inbound, Some(Addressed::Elsewhere)),
    };
    let captured = Captured {
        length: length.min(snap as usize),
        original,
        direction,
        addressed,
        time,
    };
    Some((captured, next))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use std::cell::Cell;
    use std::io;
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{Captured, Statistics, ring};

    /// A ring block's size: room for the largest frame worth keeping.
    const BLOCK: usize = 1 << 16;

    /// The fewest blocks a ring has, so the kernel fills one while the
    /// program reads another.
    const FEWEST_BLOCKS: usize = 2;

    /// How long the kernel holds a block that has frames but is not full
    /// before handing it over: how late a frame may come to the program.
    const HAND_OVER_MS: u32 = 100;

    pub struct Capture {
        fd: OwnedFd,
        snap: u32,
        ring: *mut u8,
        blocks: usize,
        /// The block being read or waited for, and within it the frames
        /// left to read and where the next starts. With none left, the
        /// block is the kernel's.
        block: Cell<usize>,
        left: Cell<u32>,
        next: Cell<usize>,
    }

    // SAFETY: the ring is mapped for this capture alone and touched only
    // through it; sending the capture to another thread sends the ring.
    unsafe impl Send for Capture {}

    /// Sets the integer socket option `name` of `level` to `value`.
    fn set(
        fd: &OwnedFd,
        level: libc::c_int,
        name: libc::c_int,
        value: libc::c_int,
    ) -> io::Result<()> {
        set_raw(fd, level, name, &value)
    }

    /// Sets the socket option `name` of `level` to the plain data `value`.
    fn set_raw<T>(
        fd: &OwnedFd,
        level: libc::c_int,
        name: libc::c_int,
        value: &T,
    ) -> io::Result<()> {
        // SAFETY: `value` is plain data that lives through the call, and
        // the length passed is its size.
        let set = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                level,
                name,
                std::ptr::from_ref(value).cast(),
                size_of::<T>() as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn milliseconds(timeout: Option<Duration>) -> libc::c_int {
        match timeout {
            // Round up, so a short wait does not become a busy loop.
            Some(timeout) => timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32,
            None => -1,
        }
    }

    impl Capture {
        pub fn open(
            interface: &str,
            ethertypes: &[u16],
            snap: u32,
            promiscuous: bool,
            room: usize,
        ) -> io::Result<Self> {
            let index = crate::interfaces::index_of(interface).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no interface named {interface}"),
                )
            })?;
            // Opened for no protocol, so nothing arrives before the filter
            // and the ring are on.
            let fd = crate::broker::open(crate::broker::Kind::Raw)?;
            let program: Vec<libc::sock_filter> = super::program(ethertypes, snap)
                .into_iter()
                .map(|(code, jt, jf, k)| libc::sock_filter { code, jt, jf, k })
                .collect();
            let filter = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr().cast_mut(),
            };
            set_raw(&fd, libc::SOL_SOCKET, libc::SO_ATTACH_FILTER, &filter)?;
            set(
                &fd,
                libc::SOL_PACKET,
                libc::PACKET_VERSION,
                libc::tpacket_versions::TPACKET_V3 as libc::c_int,
            )?;
            let blocks = (room / BLOCK).max(FEWEST_BLOCKS);
            // Version 3 keeps frames of any size in a block; the frame
            // size only has to divide it.
            let frame = 1 << 11;
            let request = libc::tpacket_req3 {
                tp_block_size: BLOCK as u32,
                tp_block_nr: blocks as u32,
                tp_frame_size: frame,
                tp_frame_nr: (BLOCK / frame as usize * blocks) as u32,
                tp_retire_blk_tov: HAND_OVER_MS,
                tp_sizeof_priv: 0,
                tp_feature_req_word: 0,
            };
            set_raw(&fd, libc::SOL_PACKET, libc::PACKET_RX_RING, &request)?;
            // SAFETY: maps the ring the kernel just made for this socket,
            // its whole length, shared with the kernel.
            let ring = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    BLOCK * blocks,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            };
            if ring == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let capture = Self {
                fd,
                snap,
                ring: ring.cast(),
                blocks,
                block: Cell::new(0),
                left: Cell::new(0),
                next: Cell::new(0),
            };
            // SAFETY: sockaddr_ll is plain data, valid when zeroed.
            let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            address.sll_family = libc::AF_PACKET as u16;
            address.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
            address.sll_ifindex = index;
            // SAFETY: `address` is a valid sockaddr_ll that lives through the
            // call, and the length passed is its size.
            let bound = unsafe {
                libc::bind(
                    capture.fd.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if bound < 0 {
                return Err(io::Error::last_os_error());
            }
            if promiscuous {
                // SAFETY: packet_mreq is plain data, valid when zeroed.
                let mut request: libc::packet_mreq = unsafe { std::mem::zeroed() };
                request.mr_ifindex = index;
                request.mr_type = libc::PACKET_MR_PROMISC as u16;
                // The membership ends as the socket closes.
                set_raw(
                    &capture.fd,
                    libc::SOL_PACKET,
                    libc::PACKET_ADD_MEMBERSHIP,
                    &request,
                )?;
            }
            Ok(capture)
        }

        /// The block being read or waited for.
        fn block(&self) -> *mut u8 {
            // SAFETY: the block number stays below the ring's count of
            // blocks, so the result lies within the mapping.
            unsafe { self.ring.add(self.block.get() * BLOCK) }
        }

        /// A block's status, which the kernel and the program hand the
        /// block back and forth with.
        fn status(&self) -> &AtomicU32 {
            // SAFETY: the status is an aligned 32-bit word of the block,
            // the mapping page-aligned, and it lives as long as `self`;
            // the kernel and this program only ever touch it whole.
            unsafe { AtomicU32::from_ptr(self.block().add(ring::BLOCK_STATUS).cast()) }
        }

        /// Hands the current block back to the kernel and moves to the
        /// next.
        fn release(&self) {
            self.left.set(0);
            self.status().store(0, Ordering::Release);
            self.block.set((self.block.get() + 1) % self.blocks);
        }

        /// The next frame waiting, without waiting for one.
        fn take(&self, buffer: &mut [u8]) -> Option<Captured> {
            loop {
                if self.left.get() == 0 {
                    if self.status().load(Ordering::Acquire) & ring::USER == 0 {
                        return None;
                    }
                    // SAFETY: the block is the program's until released, so
                    // the kernel does not write it while this slice lives.
                    let block = unsafe { std::slice::from_raw_parts(self.block(), BLOCK) };
                    match super::block_frames(block) {
                        Some((frames, first)) if frames > 0 => {
                            self.left.set(frames);
                            self.next.set(first);
                        }
                        _ => {
                            self.release();
                            continue;
                        }
                    }
                }
                // SAFETY: as above, the block is the program's.
                let block = unsafe { std::slice::from_raw_parts(self.block(), BLOCK) };
                let read = super::read_frame(block, self.next.get(), buffer, self.snap);
                let left = self.left.get() - 1;
                self.left.set(left);
                match read {
                    Some((captured, next)) => {
                        self.next.set(self.next.get() + next);
                        if left == 0 {
                            self.release();
                        }
                        return Some(captured);
                    }
                    // A block not as it should be: skip what is left of it.
                    None => self.release(),
                }
            }
        }

        pub fn receive(
            &self,
            buffer: &mut [u8],
            timeout: Option<Duration>,
        ) -> io::Result<Option<Captured>> {
            if let Some(captured) = self.take(buffer) {
                return Ok(Some(captured));
            }
            if timeout == Some(Duration::ZERO) {
                return Ok(None);
            }
            let ready = Self::ready(&[self], timeout)?;
            if ready.is_empty() {
                return Ok(None);
            }
            Ok(self.take(buffer))
        }

        pub fn ready(captures: &[&Self], timeout: Option<Duration>) -> io::Result<Vec<usize>> {
            // A capture partway through a block has frames already.
            let waiting: Vec<usize> = captures
                .iter()
                .enumerate()
                .filter(|(_, capture)| capture.left.get() > 0)
                .map(|(place, _)| place)
                .collect();
            if !waiting.is_empty() {
                return Ok(waiting);
            }
            let mut polls: Vec<libc::pollfd> = captures
                .iter()
                .map(|capture| libc::pollfd {
                    fd: capture.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: `polls` holds valid pollfds and lives through the call.
            let ready = unsafe {
                libc::poll(
                    polls.as_mut_ptr(),
                    polls.len() as libc::nfds_t,
                    milliseconds(timeout),
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

        pub fn statistics(&self) -> io::Result<Statistics> {
            // SAFETY: tpacket_stats is plain data, valid when zeroed.
            let mut stats: libc::tpacket_stats = unsafe { std::mem::zeroed() };
            let mut length = size_of::<libc::tpacket_stats>() as libc::socklen_t;
            // SAFETY: `stats` and `length` live through the call, and the
            // length says how much of `stats` the kernel may write, the
            // counts a ring's statistics start with; reading resets them.
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
                return Err(io::Error::last_os_error());
            }
            Ok(Statistics {
                passed: u64::from(stats.tp_packets),
                dropped: u64::from(stats.tp_drops),
            })
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            // SAFETY: unmaps the ring this capture mapped, its whole
            // length; nothing refers into it once the capture is gone.
            unsafe {
                libc::munmap(self.ring.cast(), BLOCK * self.blocks);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::time::Duration;

    use super::{Captured, Statistics};

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "capturing is not supported on this system yet",
        )
    }

    pub struct Capture;

    impl Capture {
        pub fn open(
            _interface: &str,
            _ethertypes: &[u16],
            _snap: u32,
            _promiscuous: bool,
            _room: usize,
        ) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn receive(
            &self,
            _buffer: &mut [u8],
            _timeout: Option<Duration>,
        ) -> io::Result<Option<Captured>> {
            Err(unsupported())
        }

        pub fn ready(_captures: &[&Self], _timeout: Option<Duration>) -> io::Result<Vec<usize>> {
            Err(unsupported())
        }

        pub fn statistics(&self) -> io::Result<Statistics> {
            Err(unsupported())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a filter of the few instructions [`program`] uses over `frame`,
    /// as the kernel would: the octets kept, zero for none.
    fn run(steps: &[Step], frame: &[u8]) -> u32 {
        let mut accumulator = 0u32;
        let mut at = 0;
        loop {
            let (code, jt, jf, k) = steps[at];
            match code {
                0x28 => {
                    let k = k as usize;
                    let Some(half) = frame.get(k..k + 2) else {
                        return 0;
                    };
                    accumulator = u32::from(u16::from_be_bytes([half[0], half[1]]));
                    at += 1;
                }
                0x15 => at += 1 + usize::from(if accumulator == k { jt } else { jf }),
                0x06 => return k,
                _ => panic!("unexpected instruction {code:#x}"),
            }
        }
    }

    fn frame(ethertypes: &[u16]) -> Vec<u8> {
        let mut frame = vec![0u8; 12];
        for ethertype in ethertypes {
            frame.extend_from_slice(&ethertype.to_be_bytes());
            if [ETHERTYPE_VLAN, ETHERTYPE_SERVICE_VLAN].contains(ethertype) {
                frame.extend_from_slice(&[0x60, 0x02]);
            }
        }
        frame.extend_from_slice(&[0; 40]);
        frame
    }

    #[test]
    fn the_filter_keeps_its_ethertypes_tagged_or_not() {
        let steps = program(&[0x88f7, 0x22ea], 96);
        assert_eq!(run(&steps, &frame(&[0x88f7])), 96);
        assert_eq!(run(&steps, &frame(&[0x22ea])), 96);
        assert_eq!(run(&steps, &frame(&[ETHERTYPE_VLAN, 0x88f7])), 96);
        assert_eq!(run(&steps, &frame(&[ETHERTYPE_SERVICE_VLAN, 0x22ea])), 96);
        assert_eq!(run(&steps, &frame(&[0x0800])), 0);
        assert_eq!(run(&steps, &frame(&[ETHERTYPE_VLAN, 0x22f0])), 0);
        assert_eq!(run(&steps, &[0; 13]), 0);
    }

    #[test]
    fn the_filter_reaches_its_last_step_from_every_test() {
        let ethertypes: Vec<u16> = (0..MOST_ETHERTYPES as u16).map(|n| 0x9000 + n).collect();
        let steps = program(&ethertypes, u32::MAX);
        for ethertype in &ethertypes {
            assert_eq!(run(&steps, &frame(&[*ethertype])), u32::MAX);
            assert_eq!(run(&steps, &frame(&[ETHERTYPE_VLAN, *ethertype])), u32::MAX);
        }
        assert_eq!(run(&steps, &frame(&[0x8fff])), 0);
    }

    #[test]
    fn with_no_ethertypes_the_filter_keeps_everything() {
        assert_eq!(program(&[], 1514), vec![(0x06, 0, 0, 1514)]);
    }

    #[test]
    fn a_tag_goes_back_after_the_source_address() {
        let mut buffer = [0u8; 32];
        let untagged: Vec<u8> = (1..=20).collect();
        buffer[..20].copy_from_slice(&untagged);
        let length = put_back_tag(&mut buffer, 20, 0x8100, 0x6002);
        assert_eq!(length, 24);
        assert_eq!(&buffer[..12], &untagged[..12]);
        assert_eq!(&buffer[12..16], &[0x81, 0x00, 0x60, 0x02]);
        assert_eq!(&buffer[16..24], &untagged[12..]);
    }

    #[test]
    fn a_tag_put_back_into_a_full_buffer_cuts_the_end() {
        let mut buffer: Vec<u8> = (1..=20).collect();
        let length = put_back_tag(&mut buffer, 20, 0x8100, 0x0002);
        assert_eq!(length, 20);
        assert_eq!(&buffer[12..16], &[0x81, 0x00, 0x00, 0x02]);
        assert_eq!(&buffer[16..20], &[13, 14, 15, 16]);
    }

    /// A ring block holding `frames`, each (packet type, VLAN tag, frame),
    /// laid out as the kernel lays it out.
    fn block(frames: &[(u8, Option<u16>, &[u8])]) -> Vec<u8> {
        let mut block = vec![0u8; 48];
        let put = |block: &mut Vec<u8>, at: usize, value: u32| {
            block[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        };
        put(&mut block, ring::BLOCK_STATUS, ring::USER);
        put(&mut block, ring::BLOCK_FRAMES, frames.len() as u32);
        put(&mut block, ring::BLOCK_FIRST, 48);
        for (place, (packet_type, tag, data)) in frames.iter().enumerate() {
            let at = block.len();
            let mac = 80;
            let size = (mac + data.len()).div_ceil(16) * 16;
            block.resize(at + size, 0);
            let last = place + 1 == frames.len();
            put(
                &mut block,
                at + ring::NEXT,
                if last { 0 } else { size as u32 },
            );
            put(&mut block, at + ring::SECONDS, 1_700_000_000);
            put(&mut block, at + ring::NANOSECONDS, 42 + place as u32);
            put(&mut block, at + ring::SNAP, data.len() as u32);
            put(&mut block, at + ring::LENGTH, data.len() as u32 + 100);
            let status = if tag.is_some() { ring::VLAN_VALID } else { 0 };
            put(&mut block, at + ring::STATUS, status);
            block[at + ring::MAC..at + ring::MAC + 2].copy_from_slice(&(mac as u16).to_ne_bytes());
            put(&mut block, at + ring::VLAN_TCI, u32::from(tag.unwrap_or(0)));
            block[at + ring::PACKET_TYPE] = *packet_type;
            block[at + mac..at + mac + data.len()].copy_from_slice(data);
        }
        block
    }

    #[test]
    fn frames_come_out_of_a_ring_block_in_order() {
        let first: Vec<u8> = (0..20).collect();
        let second: Vec<u8> = (100..130).collect();
        let block = block(&[
            (ring::OUTGOING, None, &first),
            (ring::MULTICAST, Some(0x6002), &second),
        ]);
        let (frames, mut at) = block_frames(&block).unwrap();
        assert_eq!(frames, 2);
        let mut buffer = [0u8; 64];

        let (captured, next) = read_frame(&block, at, &mut buffer, 1500).unwrap();
        assert_eq!(captured.direction, Direction::Outbound);
        assert_eq!(captured.addressed, None);
        assert_eq!(captured.length, 20);
        assert_eq!(captured.original, 120);
        assert_eq!(captured.time, Duration::new(1_700_000_000, 42));
        assert_eq!(&buffer[..20], &first[..]);
        at += next;

        let (captured, _) = read_frame(&block, at, &mut buffer, 1500).unwrap();
        assert_eq!(captured.direction, Direction::Inbound);
        assert_eq!(captured.addressed, Some(Addressed::Multicast));
        assert_eq!(captured.length, 34);
        assert_eq!(captured.original, 134);
        assert_eq!(&buffer[12..16], &[0x81, 0x00, 0x60, 0x02]);
        assert_eq!(&buffer[16..34], &second[12..]);
    }

    #[test]
    fn a_frame_is_cut_to_the_snap_length() {
        let data: Vec<u8> = (0..60).collect();
        let block = block(&[(ring::HOST, None, &data)]);
        let mut buffer = [0u8; 64];
        let (captured, _) = read_frame(&block, 48, &mut buffer, 16).unwrap();
        assert_eq!(captured.length, 16);
        assert_eq!(captured.original, 160);
        assert_eq!(captured.addressed, Some(Addressed::Unicast));
    }

    #[test]
    fn a_frame_running_past_its_block_is_refused() {
        let data: Vec<u8> = (0..20).collect();
        let mut block = block(&[(ring::HOST, None, &data)]);
        block[48 + ring::SNAP..48 + ring::SNAP + 4].copy_from_slice(&5000u32.to_ne_bytes());
        let mut buffer = [0u8; 64];
        assert_eq!(read_frame(&block, 48, &mut buffer, 1500), None);
        assert_eq!(read_frame(&block, 4000, &mut buffer, 1500), None);
    }
}
