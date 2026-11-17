//! A log an entity keeps in a LOG_OBJECT memory object (IEEE 1722.1-2021,
//! 7.2.10), for a controller to read through Address Access (9.4), so a
//! device's log needs no console cable. The controller pulls: an entity
//! sends nothing unless asked.
//!
//! The object is a 128-octet header line of ASCII, then a ring holding the
//! log's latest octets:
//!
//! ```text
//! AVBLOG1 total=%016x drop=%08x boot=%08x ring=%08x ptp=%016x ms=%08x
//! ```
//!
//! padded with spaces, its last octet `\n`, so the whole object reads as
//! the UTF-8 text a LOG_OBJECT holds.
//! - `total`: the octets written this boot.
//! - `drop`: the lines dropped whole.
//! - `boot`: tells one boot from the next.
//! - `ring`: the ring's size.
//! - `ptp` and `ms`: gPTP time in nanoseconds (0 while unsynchronized) and
//!   milliseconds since boot, sampled together as the read is answered.
//! - `rst` and `resets`, which an entity may leave out: why this boot
//!   began, as ESP-IDF's `esp_reset_reason_t` numbers it, and how many
//!   boots began unexpectedly, after a panic, a watchdog or a brownout.
//!
//! Log octet `n` sits at `128 + n % ring` in the object. A reader takes
//! the `key=value` pairs by name and passes over keys it does not know, so
//! later fields can take the padding. An entity answers all of one
//! command's TLVs from one moment of its log, so a header and the octets
//! read with it agree.
//!
//! [`LogHeader`] is the header, [`LogRing`] an entity's log and
//! [`LogTail`] a controller's reading of one.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
#[cfg(feature = "alloc")]
use core::time::Duration;

#[cfg(feature = "alloc")]
use crate::aecp::{AaStatus, AaTlv, AddressAccessPdu};
#[cfg(feature = "alloc")]
use crate::time::Instant;

/// Where a log object starts, as Address Access reads it: above 32 bits,
/// clear of any address an entity's own memory has.
pub const LOG_ADDRESS: u64 = 0x1_0000_0000;

/// The log object's header line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogHeader {
    /// Octets written to the log this boot.
    pub total: u64,
    /// Lines dropped whole, never written.
    pub drop: u32,
    /// The boot it was written in.
    pub boot: u32,
    /// Octets of the ring.
    pub ring: u32,
    /// gPTP time in nanoseconds when it was read, 0 while unsynchronized.
    pub ptp: u64,
    /// Milliseconds since boot when it was read.
    pub ms: u32,
    /// Why this boot began, if the entity says: ESP-IDF's
    /// `esp_reset_reason_t` (see [`reset_reason_name`]).
    pub reset_reason: Option<u8>,
    /// How many boots began unexpectedly, if the entity counts them.
    pub resets: Option<u16>,
}

impl LogHeader {
    /// Octets of the header, before the ring.
    pub const LEN: usize = 128;

    const MAGIC: &'static [u8] = b"AVBLOG1";

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [b' '; Self::LEN];
        let mut at = 0;
        put(&mut out, &mut at, Self::MAGIC);
        for (key, value, digits) in [
            (&b" total="[..], self.total, 16),
            (b" drop=", u64::from(self.drop), 8),
            (b" boot=", u64::from(self.boot), 8),
            (b" ring=", u64::from(self.ring), 8),
            (b" ptp=", self.ptp, 16),
            (b" ms=", u64::from(self.ms), 8),
        ]
        .into_iter()
        .chain(
            self.reset_reason
                .map(|reason| (&b" rst="[..], u64::from(reason), 2)),
        )
        .chain(
            self.resets
                .map(|resets| (&b" resets="[..], u64::from(resets), 4)),
        ) {
            put(&mut out, &mut at, key);
            for place in (0..digits).rev() {
                let digit = ((value >> (4 * place)) & 0xf) as usize;
                put(&mut out, &mut at, &[b"0123456789abcdef"[digit]]);
            }
        }
        out[Self::LEN - 1] = b'\n';
        out
    }

    /// The header at the start of `bytes`, if it is one: `AVBLOG1` first,
    /// and at least `total`, `boot` and a ring of one octet or more.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let line = &bytes[..bytes.len().min(Self::LEN)];
        let line = line.split(|&octet| octet == b'\n').next()?;
        let mut words = line
            .split(|&octet| octet == b' ')
            .filter(|word| !word.is_empty());
        if words.next()? != Self::MAGIC {
            return None;
        }
        let mut header = LogHeader::default();
        let (mut total, mut boot) = (false, false);
        for word in words {
            let Some(equals) = word.iter().position(|&octet| octet == b'=') else {
                continue;
            };
            let (key, value) = (&word[..equals], &word[equals + 1..]);
            match key {
                b"total" => {
                    header.total = hex(value)?;
                    total = true;
                }
                b"drop" => header.drop = u32::try_from(hex(value)?).ok()?,
                b"boot" => {
                    header.boot = u32::try_from(hex(value)?).ok()?;
                    boot = true;
                }
                b"ring" => header.ring = u32::try_from(hex(value)?).ok()?,
                b"ptp" => header.ptp = hex(value)?,
                b"ms" => header.ms = u32::try_from(hex(value)?).ok()?,
                b"rst" => header.reset_reason = Some(u8::try_from(hex(value)?).ok()?),
                b"resets" => header.resets = Some(u16::try_from(hex(value)?).ok()?),
                _ => {}
            }
        }
        (total && boot && header.ring > 0).then_some(header)
    }

    /// The oldest log octet the ring still holds.
    pub const fn oldest(&self) -> u64 {
        self.total.saturating_sub(self.ring as u64)
    }
}

/// The name ESP-IDF gives a reset reason, `esp_reset_reason_t`.
pub fn reset_reason_name(reason: u8) -> Option<&'static str> {
    Some(match reason {
        0 => "UNKNOWN",
        1 => "POWERON",
        2 => "EXT",
        3 => "SW",
        4 => "PANIC",
        5 => "INT_WDT",
        6 => "TASK_WDT",
        7 => "WDT",
        8 => "DEEPSLEEP",
        9 => "BROWNOUT",
        10 => "SDIO",
        11 => "USB",
        12 => "JTAG",
        13 => "EFUSE",
        14 => "PWR_GLITCH",
        15 => "CPU_LOCKUP",
        _ => return None,
    })
}

fn put(out: &mut [u8], at: &mut usize, bytes: &[u8]) {
    out[*at..*at + bytes.len()].copy_from_slice(bytes);
    *at += bytes.len();
}

/// Hexadecimal digits, either case, 16 at most.
fn hex(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    digits.iter().try_fold(0u64, |value, &digit| {
        let digit = char::from(digit).to_digit(16)?;
        Some(value << 4 | u64::from(digit))
    })
}

/// An entity's log: the latest octets written, as its LOG_OBJECT holds
/// them. Writing never waits and never fails: the oldest octets give way.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct LogRing {
    octets: Vec<u8>,
    total: u64,
    drop: u32,
    boot: u32,
    reset_reason: Option<u8>,
    resets: Option<u16>,
}

#[cfg(feature = "alloc")]
impl LogRing {
    /// A log keeping its latest `ring` octets, one or more, in the boot
    /// `boot`.
    pub fn new(ring: u32, boot: u32) -> Self {
        LogRing {
            octets: alloc::vec![0; ring.max(1) as usize],
            total: 0,
            drop: 0,
            boot,
            reset_reason: None,
            resets: None,
        }
    }

    /// Says in its header why this boot began, ESP-IDF's
    /// `esp_reset_reason_t`, and how many boots began unexpectedly.
    pub fn set_reset(&mut self, reason: u8, resets: u16) {
        self.reset_reason = Some(reason);
        self.resets = Some(resets);
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let ring = self.octets.len();
        let kept = &bytes[bytes.len().saturating_sub(ring)..];
        let start = ((self.total + (bytes.len() - kept.len()) as u64) % ring as u64) as usize;
        let first = kept.len().min(ring - start);
        self.octets[start..start + first].copy_from_slice(&kept[..first]);
        self.octets[..kept.len() - first].copy_from_slice(&kept[first..]);
        self.total += bytes.len() as u64;
    }

    /// Counts a line dropped whole, never written.
    pub fn drop_line(&mut self) {
        self.drop = self.drop.wrapping_add(1);
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// The object's length, as GET_MEMORY_OBJECT_LENGTH answers: the
    /// header and the octets written so far.
    pub fn length(&self) -> u64 {
        LogHeader::LEN as u64 + self.total.min(self.octets.len() as u64)
    }

    /// The object's maximum_length: the header and the whole ring.
    pub fn maximum_length(&self) -> u64 {
        (LogHeader::LEN + self.octets.len()) as u64
    }

    /// The header, read at `ms` milliseconds since boot and gPTP time
    /// `ptp` (0 while unsynchronized).
    pub fn header(&self, ms: u32, ptp: u64) -> LogHeader {
        LogHeader {
            total: self.total,
            drop: self.drop,
            boot: self.boot,
            ring: self.octets.len() as u32,
            ptp,
            ms,
            reset_reason: self.reset_reason,
            resets: self.resets,
        }
    }

    /// Appends `length` octets of the object from `offset` to `out`:
    /// those of `header`, then of the ring as it lies. Past the object's
    /// end, ADDRESS_TOO_HIGH.
    pub fn read(
        &self,
        offset: u64,
        length: usize,
        header: &[u8; LogHeader::LEN],
        out: &mut Vec<u8>,
    ) -> Result<(), AaStatus> {
        let end = offset
            .checked_add(length as u64)
            .filter(|&end| end <= self.maximum_length())
            .ok_or(AaStatus::ADDRESS_TOO_HIGH)?;
        let (start, end) = (offset as usize, end as usize);
        let len = LogHeader::LEN;
        out.extend_from_slice(&header[start.min(len)..end.min(len)]);
        out.extend_from_slice(&self.octets[start.max(len) - len..end.max(len) - len]);
        Ok(())
    }
}

/// What a log read says, in order.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogPiece {
    /// The log's next octets, read in a response the entity built
    /// `board_ms` milliseconds after it booted. After a gap, or at a start
    /// partway through the log, they begin partway through a line.
    Text {
        octets: Vec<u8>,
        board_ms: u32,
        after_gap: bool,
    },
    /// Octets the ring let go before they could be read.
    Lost(u64),
    /// The entity booted again: its log starts over.
    Restarted,
    /// The header of the boot read first, and of each boot after a
    /// restart: its ID, why it began, and the entity's milliseconds since
    /// boot then.
    Boot(LogHeader),
    /// Lines the entity dropped whole since the last read.
    Dropped(u32),
}

/// Reads a log while following it, one read out at a time:
/// [`poll_read`](Self::poll_read) gives the Address Access READs to send
/// when one is due, and [`handle_read`](Self::handle_read) takes what
/// they read.
///
/// Each read takes the header and, while the log grows, the octets after
/// those read so far: twice as many as the log last grew by between
/// reads, in steps of 64 and at least 64, since a READ command carries as
/// many zero octets as it asks for. Octets known to be left are asked for
/// at once, as many as one response carries; otherwise the next read
/// follows after the flowing interval, 250 ms by default. After four
/// reads in a row found nothing new, the header alone is read at the idle
/// interval, also 250 ms by default, until it shows more: a line waits a
/// quarter second at most, for a header read four times a second.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct LogTail {
    base: u64,
    flowing: Duration,
    idle: Duration,
    always: bool,
    known: Option<Known>,
    /// Octets to read when none are known to be waiting.
    expect: usize,
    empty: u8,
    next: Option<Instant>,
    asked: Option<Asked>,
    paused: bool,
    gap: bool,
}

#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy)]
struct Known {
    boot: u32,
    ring: u32,
    drop: u32,
    total: u64,
    /// The next log octet to read.
    cursor: u64,
}

/// A read out: the log octet its data TLVs start at, if it has any, and
/// whether it was a paced read rather than one catching up.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone, Copy)]
struct Asked {
    from: Option<u64>,
    paced: bool,
}

/// Empty reads in a row after which the header alone is read.
#[cfg(feature = "alloc")]
const IDLE_AFTER: u8 = 4;

/// The step a read's size grows in, and its least.
#[cfg(feature = "alloc")]
const EXPECT_STEP: usize = 64;

/// How long to wait after a read failed.
#[cfg(feature = "alloc")]
const RETRY_AFTER: Duration = Duration::from_secs(2);

#[cfg(feature = "alloc")]
const TLV_HEADER: usize = AaTlv::HEADER_LEN;

#[cfg(feature = "alloc")]
impl LogTail {
    pub const FLOWING: Duration = Duration::from_millis(250);
    pub const IDLE: Duration = Duration::from_millis(250);

    /// Follows the log whose object starts at `base`, reading first at
    /// `now`.
    pub fn new(base: u64, now: Instant) -> Self {
        LogTail {
            base,
            flowing: Self::FLOWING,
            idle: Self::IDLE,
            always: false,
            known: None,
            expect: EXPECT_STEP,
            empty: 0,
            next: Some(now),
            asked: None,
            paused: false,
            gap: false,
        }
    }

    /// Reads after `flowing` while the log grows, and the header alone
    /// after `idle` once it stops.
    pub fn set_intervals(&mut self, flowing: Duration, idle: Duration) {
        self.flowing = flowing;
        self.idle = idle;
    }

    /// Reads octets every time at the flowing interval, never the header
    /// alone: the most a reader costs, for measuring it.
    pub fn set_always_read(&mut self, always: bool) {
        self.always = always;
    }

    /// When the next read is due, none while one is out or paused.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.paused || self.asked.is_some() {
            None
        } else {
            self.next
        }
    }

    /// The next read when it is due, as READ TLVs of addresses and
    /// lengths, to go in one Address Access command.
    pub fn poll_read(&mut self, now: Instant) -> Option<Vec<(u64, u16)>> {
        if self.poll_timeout()? > now {
            return None;
        }
        let mut reads = Vec::from([(self.base, LogHeader::LEN as u16)]);
        let mut asked = Asked {
            from: None,
            paced: true,
        };
        if let Some(known) = self.known {
            let behind = known.total > known.cursor;
            asked.paced = !behind;
            let idle = self.empty >= IDLE_AFTER && !self.always;
            if behind || !idle {
                asked.from = Some(known.cursor);
                let want = if behind || self.always {
                    usize::MAX
                } else {
                    self.expect
                };
                self.data_reads(known, want, &mut reads);
            }
        }
        self.asked = Some(asked);
        Some(reads)
    }

    /// The READs of the ring from the cursor: `want` octets, or as many
    /// as one response has room for beside the header, in two parts where
    /// they wrap.
    fn data_reads(&self, known: Known, want: usize, reads: &mut Vec<(u64, u16)>) {
        let ring = known.ring as usize;
        let ring_at = |place: usize| self.base + (LogHeader::LEN + place) as u64;
        let one = AddressAccessPdu::MAX_TLVS_LEN - LogHeader::LEN - 2 * TLV_HEADER;
        let room = ring.min(one).min(want);
        let place = (known.cursor % u64::from(known.ring)) as usize;
        if place + room <= ring {
            reads.push((ring_at(place), room as u16));
            return;
        }
        let first = ring - place;
        let both = (one - TLV_HEADER).min(ring).min(room);
        if first >= both {
            reads.push((ring_at(place), first as u16));
        } else {
            reads.push((ring_at(place), first as u16));
            reads.push((ring_at(0), (both - first) as u16));
        }
    }

    /// Takes a read's TLVs as answered, in order, addresses and octets,
    /// and appends what they say to `pieces`.
    pub fn handle_read(&mut self, now: Instant, tlvs: &[(u64, &[u8])], pieces: &mut Vec<LogPiece>) {
        let Some(asked) = self.asked.take() else {
            return;
        };
        let header = match tlvs.first() {
            Some(&(address, octets)) if address == self.base => LogHeader::decode(octets),
            _ => None,
        };
        let Some(header) = header else {
            self.next = Some(now + RETRY_AFTER);
            return;
        };
        let mut read = 0;
        match self.known {
            Some(known)
                if known.boot == header.boot
                    && known.ring == header.ring
                    && header.total >= known.cursor =>
            {
                if header.drop != known.drop {
                    pieces.push(LogPiece::Dropped(header.drop.wrapping_sub(known.drop)));
                }
                // What a paced read found grown since the last read sizes
                // the next; a read catching up comes too soon to say.
                if asked.paced {
                    let grew = (header.total - known.total).min(usize::MAX as u64 / 2) as usize;
                    self.expect = (2 * grew).next_multiple_of(EXPECT_STEP).max(EXPECT_STEP);
                }
                let mut cursor = known.cursor;
                if cursor < header.oldest() {
                    pieces.push(LogPiece::Lost(header.oldest() - cursor));
                    cursor = header.oldest();
                    self.gap = true;
                } else if asked.from == Some(cursor) {
                    read = self.take_octets(header, cursor, &tlvs[1..], pieces);
                    cursor += read as u64;
                }
                self.known = Some(Known {
                    drop: header.drop,
                    total: header.total,
                    cursor,
                    ..known
                });
            }
            known => {
                if known.is_some() {
                    pieces.push(LogPiece::Restarted);
                }
                pieces.push(LogPiece::Boot(header));
                self.gap = header.total > u64::from(header.ring);
                self.known = Some(Known {
                    boot: header.boot,
                    ring: header.ring,
                    drop: header.drop,
                    total: header.total,
                    cursor: header.oldest(),
                });
            }
        }
        let behind = self.known.is_some_and(|known| known.total > known.cursor);
        self.next = Some(if behind {
            self.empty = 0;
            now
        } else if read > 0 {
            self.empty = 0;
            now + self.flowing
        } else {
            self.empty = self.empty.saturating_add(1);
            if self.empty >= IDLE_AFTER && !self.always {
                now + self.idle
            } else {
                now + self.flowing
            }
        });
    }

    /// The octets of `tlvs` that are log octets from `cursor` on, up to
    /// what `header` says was written: appends them and returns how many.
    fn take_octets(
        &mut self,
        header: LogHeader,
        cursor: u64,
        tlvs: &[(u64, &[u8])],
        pieces: &mut Vec<LogPiece>,
    ) -> usize {
        let wanted = (header.total - cursor).min(usize::MAX as u64) as usize;
        let ring = u64::from(header.ring);
        let mut octets = Vec::new();
        let mut expected = cursor;
        for &(address, data) in tlvs {
            let place = self.base + LogHeader::LEN as u64 + expected % ring;
            if address != place || octets.len() >= wanted {
                break;
            }
            let take = data.len().min(wanted - octets.len());
            octets.extend_from_slice(&data[..take]);
            expected += take as u64;
        }
        let read = octets.len();
        if read > 0 {
            pieces.push(LogPiece::Text {
                octets,
                board_ms: header.ms,
                after_gap: core::mem::take(&mut self.gap),
            });
        }
        read
    }

    /// Takes a read that failed, unanswered or refused: the next follows
    /// after two seconds.
    pub fn handle_failure(&mut self, now: Instant) {
        self.asked = None;
        self.next = Some(now + RETRY_AFTER);
    }

    /// Stops reading, while the entity is away; a read out is forgotten.
    pub fn pause(&mut self) {
        self.paused = true;
        self.asked = None;
    }

    /// Reads again from `now`, once the entity is back.
    pub fn resume(&mut self, now: Instant) {
        if self.paused {
            self.paused = false;
            self.next = Some(now);
        }
    }

    /// Whether everything the last header showed is read, with no read
    /// out: where reading the log once ends.
    pub fn caught_up(&self) -> bool {
        self.asked.is_none() && self.known.is_some_and(|known| known.cursor >= known.total)
    }

    /// The log octets the last header showed that are not read yet.
    pub fn behind(&self) -> u64 {
        self.known
            .map_or(0, |known| known.total.saturating_sub(known.cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn ms(n: u64) -> Instant {
        Instant::from_millis(n)
    }

    /// Answers `reads` from `log` as an entity would.
    fn answer(log: &LogRing, reads: &[(u64, u16)], at_ms: u32) -> Vec<(u64, Vec<u8>)> {
        let header = log.header(at_ms, 0).encode();
        reads
            .iter()
            .map(|&(address, length)| {
                let mut out = Vec::new();
                log.read(
                    address - LOG_ADDRESS,
                    usize::from(length),
                    &header,
                    &mut out,
                )
                .unwrap();
                (address, out)
            })
            .collect()
    }

    /// Runs one read: the TLVs asked for, answered, and what they say.
    fn step(tail: &mut LogTail, log: &LogRing, now: Instant) -> Option<Vec<LogPiece>> {
        let reads = tail.poll_read(now)?;
        let answered = answer(log, &reads, now.since_start().as_millis() as u32);
        let tlvs: Vec<(u64, &[u8])> = answered.iter().map(|(a, o)| (*a, &o[..])).collect();
        let mut pieces = Vec::new();
        tail.handle_read(now, &tlvs, &mut pieces);
        Some(pieces)
    }

    fn text(pieces: &[LogPiece]) -> String {
        pieces
            .iter()
            .filter_map(|piece| match piece {
                LogPiece::Text { octets, .. } => Some(String::from_utf8_lossy(octets).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn header_round_trips_in_128_octets() {
        let header = LogHeader {
            total: 0x1_2345_6789,
            drop: 3,
            boot: 0xdead_beef,
            ring: 0x8000,
            ptp: 0x1122_3344_5566_7788,
            ms: 0x1e240,
            reset_reason: None,
            resets: None,
        };
        let bytes = header.encode();
        assert_eq!(bytes.len(), 128);
        assert_eq!(bytes[127], b'\n');
        let line = core::str::from_utf8(&bytes).unwrap();
        assert!(line.starts_with(
            "AVBLOG1 total=0000000123456789 drop=00000003 boot=deadbeef ring=00008000 \
             ptp=1122334455667788 ms=0001e240 "
        ));
        assert_eq!(line.trim_end().len(), 105);
        assert_eq!(LogHeader::decode(&bytes), Some(header));
    }

    #[test]
    fn reads_an_esp32_boards_header() {
        // esp-mic's, as esp_avb wrote it 45 s after boot.
        let mut line = [b' '; 128];
        let text = b"AVBLOG1 total=0000000000001acd drop=00000000 boot=812c21f8 \
                     ring=00008000 ptp=0001d3694c09482a ms=0000b194";
        line[..text.len()].copy_from_slice(text);
        line[127] = b'\n';
        assert_eq!(text.len(), 105);
        let header = LogHeader {
            total: 0x1acd,
            drop: 0,
            boot: 0x812c_21f8,
            ring: 0x8000,
            ptp: 0x0001_d369_4c09_482a,
            ms: 0xb194,
            reset_reason: None,
            resets: None,
        };
        assert_eq!(LogHeader::decode(&line), Some(header));
        assert_eq!(header.encode(), line);
    }

    #[test]
    fn a_header_says_why_its_boot_began_in_the_padding() {
        let header = LogHeader {
            total: 0x1acd,
            boot: 0x812c_21f8,
            ring: 0x8000,
            ms: 0xb194,
            reset_reason: Some(4),
            resets: Some(3),
            ..LogHeader::default()
        };
        let bytes = header.encode();
        let line = core::str::from_utf8(&bytes).unwrap();
        assert!(line.trim_end().ends_with(" ms=0000b194 rst=04 resets=0003"));
        assert_eq!(line.trim_end().len(), 124);
        assert_eq!(bytes[127], b'\n');
        assert_eq!(LogHeader::decode(&bytes), Some(header));
        assert_eq!(reset_reason_name(4), Some("PANIC"));
        assert_eq!(reset_reason_name(99), None);
        // A ring that knows says so in every header.
        let mut log = LogRing::new(64, 1);
        log.set_reset(6, 1);
        let read = log.header(0, 0);
        assert_eq!((read.reset_reason, read.resets), (Some(6), Some(1)));
    }

    #[test]
    fn header_takes_fields_by_name_and_passes_over_others() {
        let mut line = [b' '; 128];
        let text =
            b"AVBLOG1 ms=0000000a hw=00000040 boot=00000001 ring=00000100 total=0000000000000020";
        line[..text.len()].copy_from_slice(text);
        line[127] = b'\n';
        let header = LogHeader::decode(&line).unwrap();
        assert_eq!(
            (header.total, header.ring, header.boot, header.ms),
            (32, 256, 1, 10)
        );
        assert_eq!(LogHeader::decode(b"AVBLOG2 total=1 boot=1 ring=1\n"), None);
        assert_eq!(LogHeader::decode(b"AVBLOG1 total=1 boot=1\n"), None);
        assert_eq!(LogHeader::decode(b"AVBLOG1 total=1x boot=1 ring=1\n"), None);
    }

    #[test]
    fn ring_keeps_the_latest_octets_where_the_total_puts_them() {
        let mut log = LogRing::new(8, 1);
        log.write(b"abcde");
        assert_eq!(log.length(), 128 + 5);
        log.write(b"fghijk");
        assert_eq!(log.total(), 11);
        assert_eq!(log.length(), 128 + 8);
        let header = [b'h'; 128];
        let mut out = Vec::new();
        log.read(128, 8, &header, &mut out).unwrap();
        assert_eq!(&out, b"ijkdefgh");
        out.clear();
        log.read(126, 4, &header, &mut out).unwrap();
        assert_eq!(&out, b"hhij");
        assert_eq!(
            log.read(130, 7, &header, &mut out),
            Err(AaStatus::ADDRESS_TOO_HIGH)
        );
        log.write(b"0123456789");
        out.clear();
        log.read(128, 8, &header, &mut out).unwrap();
        assert_eq!(log.total(), 21);
        // The ring holds octets 13..21, octet 13 at 13 % 8 = 5.
        assert_eq!(&out, b"56789234");
    }

    #[test]
    fn follows_a_boot_burst_at_once_then_paces() {
        let mut log = LogRing::new(4096, 7);
        let mut burst = String::new();
        for line in 0..40 {
            burst.push_str(&alloc::format!("I ({line}) boot: line {line}\n"));
        }
        log.write(burst.as_bytes());
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        // The header first, then the burst in reads one after another.
        let first = step(&mut tail, &log, ms(0)).unwrap();
        assert!(matches!(&first[..], [LogPiece::Boot(header)] if header.boot == 7));
        let mut read = String::new();
        let mut reads = 0;
        while tail.poll_timeout() == Some(ms(0)) {
            read.push_str(&text(&step(&mut tail, &log, ms(0)).unwrap()));
            reads += 1;
        }
        assert_eq!(read, burst);
        assert_eq!(reads, burst.len().div_ceil(364));
        assert!(tail.caught_up());
        // Nothing new: the next read waits the flowing interval.
        assert_eq!(tail.poll_timeout(), Some(ms(250)));
    }

    #[test]
    fn idles_on_the_header_alone_and_reads_at_once_when_it_moves() {
        let mut log = LogRing::new(1024, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        // The first read, of the header alone, finds nothing too.
        step(&mut tail, &log, ms(0)).unwrap();
        for _ in 1..IDLE_AFTER {
            let now = tail.poll_timeout().unwrap();
            assert_eq!(tail.clone().poll_read(now).unwrap().len(), 2);
            assert!(step(&mut tail, &log, now).unwrap().is_empty());
        }
        let idle_at = tail.poll_timeout().unwrap();
        assert_eq!(idle_at, ms(3 * 250) + LogTail::IDLE);
        assert_eq!(
            tail.clone().poll_read(idle_at).unwrap(),
            Vec::from([(LOG_ADDRESS, 128)])
        );
        log.write(b"I (5000) app: hello\n");
        assert!(step(&mut tail, &log, idle_at).unwrap().is_empty());
        // Behind: the octets are read at once.
        assert_eq!(tail.poll_timeout(), Some(idle_at));
        let pieces = step(&mut tail, &log, idle_at).unwrap();
        assert_eq!(text(&pieces), "I (5000) app: hello\n");
        assert_eq!(tail.poll_timeout(), Some(idle_at + LogTail::FLOWING));
    }

    #[test]
    fn reads_across_the_ring_end_in_two_parts() {
        let mut log = LogRing::new(300, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        log.write(&[b'a'; 250]);
        step(&mut tail, &log, ms(0)).unwrap();
        assert_eq!(text(&step(&mut tail, &log, ms(0)).unwrap()).len(), 250);
        log.write(&[b'b'; 100]);
        // Nothing grew since the last read: 64 octets, across the end.
        let reads = tail.clone().poll_read(ms(250)).unwrap();
        assert_eq!(
            reads,
            Vec::from([
                (LOG_ADDRESS, 128),
                (LOG_ADDRESS + 128 + 250, 50),
                (LOG_ADDRESS + 128, 14)
            ])
        );
        let first = text(&step(&mut tail, &log, ms(250)).unwrap());
        assert_eq!(first, "b".repeat(64));
        // The rest, known to wait, at once and as much as fits.
        assert_eq!(tail.poll_timeout(), Some(ms(250)));
        assert_eq!(
            tail.clone().poll_read(ms(250)).unwrap(),
            Vec::from([
                (LOG_ADDRESS, 128),
                (LOG_ADDRESS + 128 + 14, 286),
                (LOG_ADDRESS + 128, 14)
            ])
        );
        let rest = text(&step(&mut tail, &log, ms(250)).unwrap());
        assert_eq!(rest, "b".repeat(36));
        // The next read expects twice the growth seen: 100 octets, so 256.
        assert_eq!(tail.expect, 256);
    }

    #[test]
    fn says_what_the_ring_let_go_and_starts_after_it() {
        let mut log = LogRing::new(256, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        step(&mut tail, &log, ms(0)).unwrap();
        log.write(&[b'x'; 1000]);
        let pieces = step(&mut tail, &log, ms(250)).unwrap();
        assert_eq!(pieces, Vec::from([LogPiece::Lost(1000 - 256)]));
        let pieces = step(&mut tail, &log, ms(250)).unwrap();
        assert!(
            matches!(&pieces[..], [LogPiece::Text { after_gap: true, octets, .. }] if octets.len() == 256)
        );
    }

    #[test]
    fn a_start_partway_and_a_new_boot_are_marked() {
        let mut log = LogRing::new(64, 1);
        log.write(&[b'y'; 100]);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        step(&mut tail, &log, ms(0)).unwrap();
        let pieces = step(&mut tail, &log, ms(0)).unwrap();
        assert!(
            matches!(&pieces[..], [LogPiece::Text { after_gap: true, octets, .. }] if octets.len() == 64)
        );
        let mut log = LogRing::new(64, 2);
        log.write(b"I (3) boot: again\n");
        let pieces = step(&mut tail, &log, ms(250)).unwrap();
        assert!(matches!(
            &pieces[..],
            [LogPiece::Restarted, LogPiece::Boot(header)] if header.boot == 2
        ));
        let pieces = step(&mut tail, &log, ms(250)).unwrap();
        assert_eq!(text(&pieces), "I (3) boot: again\n");
    }

    #[test]
    fn counts_lines_the_entity_dropped() {
        let mut log = LogRing::new(64, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        step(&mut tail, &log, ms(0)).unwrap();
        log.drop_line();
        log.drop_line();
        let pieces = step(&mut tail, &log, ms(250)).unwrap();
        assert_eq!(pieces, Vec::from([LogPiece::Dropped(2)]));
    }

    #[test]
    fn waits_after_a_failure_and_while_paused() {
        let log = LogRing::new(64, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        tail.poll_read(ms(0)).unwrap();
        assert_eq!(tail.poll_timeout(), None);
        tail.handle_failure(ms(300));
        assert_eq!(tail.poll_timeout(), Some(ms(2300)));
        tail.pause();
        assert_eq!(tail.poll_timeout(), None);
        tail.resume(ms(5000));
        assert!(step(&mut tail, &log, ms(5000)).is_some());
    }

    #[test]
    fn always_reading_never_idles() {
        let log = LogRing::new(64, 1);
        let mut tail = LogTail::new(LOG_ADDRESS, ms(0));
        tail.set_always_read(true);
        tail.set_intervals(Duration::from_millis(10), Duration::from_secs(1));
        step(&mut tail, &log, ms(0)).unwrap();
        for _ in 0..10 {
            let now = tail.poll_timeout().unwrap();
            assert_eq!(tail.clone().poll_read(now).unwrap().len(), 2);
            step(&mut tail, &log, now).unwrap();
        }
        assert_eq!(tail.poll_timeout(), Some(ms(110)));
    }
}
