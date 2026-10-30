//! Frames of one ethertype sent and received with the times the
//! interface's hardware put on them, as gPTP needs: when each received
//! frame crossed the wire in, and when each sent one went out, by the
//! interface's PTP hardware clock. The kernel adds and removes the
//! Ethernet header, as for a [`crate::socket::Socket`].
//!
//! [`enable`] turns the interface's time stamping on, for every frame
//! received and for the frames sent that ask for it; programs such as
//! ptp4l do the same. Only the frames sent with
//! [`StampedSocket::send_stamped`] ask, gPTP's event messages, so their
//! times are the ones that come back. Linux only; elsewhere opening fails
//! as unsupported.

use std::io;
use std::time::Duration;

use crate::MacAddress;

/// A frame received on a [`StampedSocket`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    pub source: MacAddress,
    /// Octets of payload written to the buffer, after the Ethernet header.
    pub length: usize,
    /// When it crossed the wire in, by the interface's hardware clock,
    /// when the interface stamped it.
    pub time: Option<Duration>,
}

/// Sends and receives the payloads of Ethernet frames of one ethertype on
/// one interface, with hardware time stamps.
pub struct StampedSocket {
    inner: platform::StampedSocket,
}

impl StampedSocket {
    /// Opens `interface` for frames of `ethertype`, stamping each frame
    /// received. On Linux this needs `CAP_NET_RAW`.
    pub fn open(interface: &str, ethertype: u16) -> io::Result<Self> {
        Ok(Self {
            inner: platform::StampedSocket::open(interface, ethertype)?,
        })
    }

    /// Receives frames sent to `group` as well.
    pub fn join_multicast(&self, group: MacAddress) -> io::Result<()> {
        self.inner.join_multicast(group)
    }

    /// Sends `payload` to `destination`, unstamped.
    pub fn send(&self, destination: MacAddress, payload: &[u8]) -> io::Result<()> {
        self.inner.send(destination, payload)
    }

    /// Sends `payload` to `destination` and has the interface stamp it;
    /// when it has gone out, [`transmit_time`](Self::transmit_time) gives
    /// the time. Times of frames sent before that came too late for
    /// `transmit_time` are dropped first, so the next it gives is this
    /// frame's.
    pub fn send_stamped(&self, destination: MacAddress, payload: &[u8]) -> io::Result<()> {
        self.inner.send_stamped(destination, payload)
    }

    /// The time the next of the frames sent with
    /// [`send_stamped`](Self::send_stamped) went out, by the interface's
    /// hardware clock, waiting up to `timeout` for the interface to say.
    /// `None` when it did not say in time.
    pub fn transmit_time(&self, timeout: Duration) -> io::Result<Option<Duration>> {
        self.inner.transmit_time(timeout)
    }

    /// Writes the next frame's payload to `buffer`, waiting up to
    /// `timeout` (forever when `None`) for one.
    pub fn receive(
        &self,
        buffer: &mut [u8],
        timeout: Option<Duration>,
    ) -> io::Result<Option<Received>> {
        self.inner.receive(buffer, timeout)
    }
}

/// Has `interface` stamp every frame it receives and the frames sent that
/// ask for it, as a gPTP program needs. This changes the interface for
/// every program, and needs `CAP_NET_ADMIN`; an interface already set so,
/// by ptp4l say, is left as it is, as setting it again can lose a time
/// the other program waits for.
pub fn enable(interface: &str) -> io::Result<()> {
    platform::enable(interface)
}

/// The control message of SO_TIMESTAMPING's three times: the raw hardware
/// time, the last, when it is there.
#[cfg(any(target_os = "linux", test))]
fn raw_hardware(stamps: [(i64, i64); 3]) -> Option<Duration> {
    let (seconds, nanoseconds) = stamps[2];
    (seconds > 0 || nanoseconds > 0).then(|| Duration::new(seconds as u64, nanoseconds as u32))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod platform {
    use std::io;
    use std::mem::{size_of, size_of_val, zeroed};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::time::Duration;

    use super::Received;
    use crate::MacAddress;

    /// From Linux's headers, which libc lacks for Linux.
    const SO_TIMESTAMPING: libc::c_int = 37;
    const SIOCSHWTSTAMP: libc::c_ulong = 0x89b0;
    const SIOCGHWTSTAMP: libc::c_ulong = 0x89b1;
    const HWTSTAMP_TX_ON: libc::c_int = 1;
    const HWTSTAMP_FILTER_ALL: libc::c_int = 1;
    const SOF_TIMESTAMPING_TX_HARDWARE: u32 = 1 << 0;
    const SOF_TIMESTAMPING_RX_HARDWARE: u32 = 1 << 2;
    const SOF_TIMESTAMPING_RAW_HARDWARE: u32 = 1 << 6;
    /// Only the time comes back on the error queue, not the frame.
    const SOF_TIMESTAMPING_OPT_TSONLY: u32 = 1 << 11;

    /// struct hwtstamp_config, from linux/net_tstamp.h.
    #[repr(C)]
    struct HardwareConfig {
        flags: libc::c_int,
        tx_type: libc::c_int,
        rx_filter: libc::c_int,
    }

    pub fn enable(interface: &str) -> io::Result<()> {
        let fd = crate::broker::open(crate::broker::Kind::Datagram)?;
        let mut config = HardwareConfig {
            flags: 0,
            tx_type: 0,
            rx_filter: 0,
        };
        if configure(&fd, interface, SIOCGHWTSTAMP, &mut config).is_ok()
            && config.tx_type == HWTSTAMP_TX_ON
            && config.rx_filter == HWTSTAMP_FILTER_ALL
        {
            return Ok(());
        }
        config = HardwareConfig {
            flags: 0,
            tx_type: HWTSTAMP_TX_ON,
            rx_filter: HWTSTAMP_FILTER_ALL,
        };
        configure(&fd, interface, SIOCSHWTSTAMP, &mut config)
    }

    /// Gets or sets `interface`'s time stamping with `request`, through
    /// `config`.
    fn configure(
        fd: &OwnedFd,
        interface: &str,
        request_code: libc::c_ulong,
        config: &mut HardwareConfig,
    ) -> io::Result<()> {
        // SAFETY: ifreq is plain data, valid when zeroed.
        let mut request: libc::ifreq = unsafe { zeroed() };
        let name = interface.as_bytes();
        if name.len() >= request.ifr_name.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name too long",
            ));
        }
        for (slot, octet) in request.ifr_name.iter_mut().zip(name) {
            *slot = *octet as libc::c_char;
        }
        request.ifr_ifru.ifru_data = std::ptr::from_mut(config).cast();
        // SAFETY: `request` names the interface and points at `config`,
        // both living through the call.
        if unsafe { libc::ioctl(fd.as_raw_fd(), request_code as _, &raw mut request) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub struct StampedSocket {
        fd: OwnedFd,
        index: i32,
        ethertype: u16,
    }

    fn timeout_ms(timeout: Option<Duration>) -> libc::c_int {
        match timeout {
            // Round up, so a short wait does not become a busy loop.
            Some(timeout) => timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32,
            None => -1,
        }
    }

    /// Waits up to `timeout` for `events` on `fd`: whether they came.
    fn wait(fd: &OwnedFd, events: libc::c_short, timeout: Option<Duration>) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: one valid pollfd, which lives through the call.
        let ready = unsafe { libc::poll(&raw mut poll, 1, timeout_ms(timeout)) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::Interrupted => Ok(false),
                _ => Err(error),
            };
        }
        Ok(ready > 0 && poll.revents & (events | libc::POLLERR) != 0)
    }

    /// Receives one message on `fd` with `flags` into `buffer`, its
    /// source into `address`: its length and its raw hardware time.
    fn receive_message(
        fd: &OwnedFd,
        buffer: &mut [u8],
        address: &mut libc::sockaddr_ll,
        flags: libc::c_int,
    ) -> io::Result<Option<(usize, Option<Duration>)>> {
        let mut control = [0u64; 64];
        let mut part = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        // SAFETY: msghdr is plain data, valid when zeroed.
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_name = std::ptr::from_mut(address).cast();
        message.msg_namelen = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        message.msg_iov = &raw mut part;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&control) as _;
        // SAFETY: every buffer the message points to lives through the
        // call, with its length given.
        let length =
            unsafe { libc::recvmsg(fd.as_raw_fd(), &raw mut message, flags | libc::MSG_DONTWAIT) };
        if length < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(error),
            };
        }
        let mut time = None;
        // SAFETY: the kernel wrote the control messages within `control`,
        // and msg_controllen says how far; CMSG_FIRSTHDR and CMSG_NXTHDR
        // stay within it.
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
        while !header.is_null() {
            // SAFETY: a header the kernel wrote, its data after it.
            let (level, kind, data) = unsafe {
                (
                    (*header).cmsg_level,
                    (*header).cmsg_type,
                    libc::CMSG_DATA(header),
                )
            };
            if level == libc::SOL_SOCKET && kind == SO_TIMESTAMPING {
                // SAFETY: SCM_TIMESTAMPING carries three timespecs.
                let stamps: [libc::timespec; 3] = unsafe { std::ptr::read_unaligned(data.cast()) };
                time = super::raw_hardware(stamps.map(|stamp| (stamp.tv_sec, stamp.tv_nsec)));
            }
            // SAFETY: as above.
            header = unsafe { libc::CMSG_NXTHDR(&raw const message, header) };
        }
        Ok(Some((length as usize, time)))
    }

    impl StampedSocket {
        pub fn open(interface: &str, ethertype: u16) -> io::Result<Self> {
            let index = crate::interfaces::index_of(interface).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no interface named {interface}"),
                )
            })?;
            let fd = crate::broker::open(crate::broker::Kind::Datagram)?;
            // Frames sent ask for a time one by one, in send_stamped: asked
            // here, every frame sent would leave one, follow ups too.
            let flags = SOF_TIMESTAMPING_RX_HARDWARE
                | SOF_TIMESTAMPING_RAW_HARDWARE
                | SOF_TIMESTAMPING_OPT_TSONLY;
            let value = flags as libc::c_int;
            // SAFETY: `value` is a valid c_int that lives through the call,
            // and the length passed is its size.
            let set = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    SO_TIMESTAMPING,
                    (&raw const value).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if set < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = Self {
                fd,
                index,
                ethertype,
            };
            let address = socket.address(None);
            // SAFETY: `address` is a valid sockaddr_ll that lives through the
            // call, and the length passed is its size.
            let bound = unsafe {
                libc::bind(
                    socket.fd.as_raw_fd(),
                    (&raw const address).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if bound < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(socket)
        }

        fn address(&self, destination: Option<MacAddress>) -> libc::sockaddr_ll {
            // SAFETY: sockaddr_ll is plain data, valid when zeroed.
            let mut address: libc::sockaddr_ll = unsafe { zeroed() };
            address.sll_family = libc::AF_PACKET as u16;
            address.sll_protocol = self.ethertype.to_be();
            address.sll_ifindex = self.index;
            if let Some(destination) = destination {
                address.sll_halen = 6;
                address.sll_addr[..6].copy_from_slice(&destination.0);
            }
            address
        }

        pub fn join_multicast(&self, group: MacAddress) -> io::Result<()> {
            // SAFETY: packet_mreq is plain data, valid when zeroed.
            let mut request: libc::packet_mreq = unsafe { zeroed() };
            request.mr_ifindex = self.index;
            request.mr_type = libc::PACKET_MR_MULTICAST as u16;
            request.mr_alen = 6;
            request.mr_address[..6].copy_from_slice(&group.0);
            // SAFETY: `request` is a valid packet_mreq that lives through the
            // call, and the length passed is its size.
            let joined = unsafe {
                libc::setsockopt(
                    self.fd.as_raw_fd(),
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
        }

        pub fn send(&self, destination: MacAddress, payload: &[u8]) -> io::Result<()> {
            let address = self.address(Some(destination));
            // SAFETY: `payload` and `address` are valid for the lengths
            // passed and live through the call.
            let sent = unsafe {
                libc::sendto(
                    self.fd.as_raw_fd(),
                    payload.as_ptr().cast(),
                    payload.len(),
                    0,
                    (&raw const address).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn send_stamped(&self, destination: MacAddress, payload: &[u8]) -> io::Result<()> {
            // SAFETY: plain data, valid when zeroed.
            let mut stale: libc::sockaddr_ll = unsafe { zeroed() };
            let mut buffer = [0u8; 64];
            while receive_message(&self.fd, &mut buffer, &mut stale, libc::MSG_ERRQUEUE)?.is_some()
            {
            }
            let mut address = self.address(Some(destination));
            let mut part = libc::iovec {
                iov_base: payload.as_ptr().cast_mut().cast(),
                iov_len: payload.len(),
            };
            let mut control = [0u64; 4];
            // SAFETY: msghdr is plain data, valid when zeroed.
            let mut message: libc::msghdr = unsafe { zeroed() };
            message.msg_name = (&raw mut address).cast();
            message.msg_namelen = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
            message.msg_iov = &raw mut part;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            // SAFETY: CMSG_SPACE only works out a length.
            message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<u32>() as u32) } as _;
            // SAFETY: `control` has room for one control message carrying a
            // u32, as msg_controllen says, so CMSG_FIRSTHDR points into it.
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&raw const message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = SO_TIMESTAMPING;
                (*header).cmsg_len = libc::CMSG_LEN(size_of::<u32>() as u32) as _;
                std::ptr::write_unaligned(
                    libc::CMSG_DATA(header).cast::<u32>(),
                    SOF_TIMESTAMPING_TX_HARDWARE,
                );
            }
            // SAFETY: every buffer the message points to lives through the
            // call, with its length given.
            if unsafe { libc::sendmsg(self.fd.as_raw_fd(), &raw const message, 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn transmit_time(&self, timeout: Duration) -> io::Result<Option<Duration>> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                // SAFETY: plain data, valid when zeroed.
                let mut address: libc::sockaddr_ll = unsafe { zeroed() };
                let mut buffer = [0u8; 64];
                if let Some((_, time)) =
                    receive_message(&self.fd, &mut buffer, &mut address, libc::MSG_ERRQUEUE)?
                {
                    return Ok(time);
                }
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() || !wait(&self.fd, 0, Some(left))? {
                    return Ok(None);
                }
            }
        }

        pub fn receive(
            &self,
            buffer: &mut [u8],
            timeout: Option<Duration>,
        ) -> io::Result<Option<Received>> {
            if !wait(&self.fd, libc::POLLIN, timeout)? {
                return Ok(None);
            }
            // SAFETY: plain data, valid when zeroed.
            let mut address: libc::sockaddr_ll = unsafe { zeroed() };
            loop {
                let Some((length, time)) = receive_message(&self.fd, buffer, &mut address, 0)?
                else {
                    return Ok(None);
                };
                // Frames this computer sends come back as outgoing.
                if address.sll_pkttype == libc::PACKET_OUTGOING {
                    continue;
                }
                let mut source = [0; 6];
                source.copy_from_slice(&address.sll_addr[..6]);
                return Ok(Some(Received {
                    source: MacAddress(source),
                    length: length.min(buffer.len()),
                    time,
                }));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;
    use std::time::Duration;

    use super::Received;
    use crate::MacAddress;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "hardware time stamps are not supported on this system yet",
        )
    }

    pub fn enable(_interface: &str) -> io::Result<()> {
        Err(unsupported())
    }

    pub struct StampedSocket;

    impl StampedSocket {
        pub fn open(_interface: &str, _ethertype: u16) -> io::Result<Self> {
            Err(unsupported())
        }

        pub fn join_multicast(&self, _group: MacAddress) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn send(&self, _destination: MacAddress, _payload: &[u8]) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn send_stamped(&self, _destination: MacAddress, _payload: &[u8]) -> io::Result<()> {
            Err(unsupported())
        }

        pub fn transmit_time(&self, _timeout: Duration) -> io::Result<Option<Duration>> {
            Err(unsupported())
        }

        pub fn receive(
            &self,
            _buffer: &mut [u8],
            _timeout: Option<Duration>,
        ) -> io::Result<Option<Received>> {
            Err(unsupported())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raw_hardware_time_is_the_last_of_three() {
        assert_eq!(
            raw_hardware([(1_793_300_000, 5), (0, 0), (265_258, 495_597_656)]),
            Some(Duration::new(265_258, 495_597_656))
        );
        assert_eq!(raw_hardware([(1_793_300_000, 5), (0, 0), (0, 0)]), None);
    }
}
