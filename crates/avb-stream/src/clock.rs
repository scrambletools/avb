//! gPTP time as the media clock: the grandmaster's time as gptpd gives it
//! for the interface, a line against the monotonic clock the threads
//! sleep on, read again ten times a second. gptpd moves no clock to carry
//! the grandmaster's time. A new line that does not carry the last one on,
//! as from a grandmaster with time of its own, is a discontinuity the
//! streams start again across, once records in a row confirm it; one that
//! does, as gptpd restarting or taking over as grandmaster, is not,
//! whatever gptpd numbers it. The line is read only while something asks
//! for the time; one nothing read for a while has drifted, and the next
//! record replaces it at once, as the first does.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use avb_net::clock::monotonic_now;
use gptp::time::{Mapping, State};

/// How often gptpd's mapping is read again.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const REFRESH: Duration = Duration::from_millis(100);
/// How far a new line may stand from where the last puts it, in
/// nanoseconds, and still carry it on: far past a Sync's jitter and what
/// NTP moving CLOCK_MONOTONIC's rate does in a tenth of a second, short
/// of half a sample at 48 kHz.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CARRIED: i64 = 10_000;
/// Records in a row, a tenth of a second apart, that a line not carrying
/// the last one on needs before it is taken: a single record off and then
/// back, as gptpd can give while a comparison settles, is no
/// discontinuity.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CONFIRMATIONS: u8 = 3;
/// How long the line may go unread and still judge the next record: ten
/// reads missed. The line runs on at the rate of the record it came from,
/// and the monotonic clock's rate moves under it, as NTP moves it, so
/// that after minutes or hours it stands far past [`CARRIED`] from gPTP
/// time with no discontinuity in gPTP time at all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const STALE: Duration = Duration::from_secs(1);

/// A new line waiting to be confirmed, and the records in a row that
/// agreed with it.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    at: i64,
    base: i64,
    rate: f64,
    seen: u8,
}

impl Candidate {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn gptp_at(&self, monotonic: i64) -> i64 {
        self.base
            .saturating_add((monotonic.saturating_sub(self.at) as f64 * self.rate) as i64)
    }
}

/// The grandmaster's time as a line against the monotonic clock: at
/// monotonic time `at` it was `base`, and it runs `rate` times as fast,
/// all behind a sequence count, odd while a line is being stored, so that
/// a thread never takes half of one line and half of the next.
struct Line {
    sequence: AtomicU64,
    at: AtomicI64,
    base: AtomicI64,
    rate: AtomicU64,
}

impl Line {
    /// The monotonic clock itself.
    fn new() -> Self {
        Line {
            sequence: AtomicU64::new(0),
            at: AtomicI64::new(0),
            base: AtomicI64::new(0),
            rate: AtomicU64::new(1.0f64.to_bits()),
        }
    }

    /// Stores a line; one thread at a time, the one refreshing.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn store(&self, at: i64, base: i64, rate: f64) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.at.store(at, Ordering::Relaxed);
        self.base.store(base, Ordering::Relaxed);
        self.rate.store(rate.to_bits(), Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }

    fn load(&self) -> (i64, i64, f64) {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            let at = self.at.load(Ordering::Relaxed);
            let base = self.base.load(Ordering::Relaxed);
            let rate = self.rate.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if before.is_multiple_of(2) && self.sequence.load(Ordering::Relaxed) == before {
                return (at, base, f64::from_bits(rate));
            }
            std::hint::spin_loop();
        }
    }

    /// gPTP time at monotonic time `monotonic`.
    fn gptp_at(&self, monotonic: i64) -> i64 {
        let (at, base, rate) = self.load();
        base.saturating_add((monotonic.saturating_sub(at) as f64 * rate) as i64)
    }

    /// Monotonic time when gPTP time is `gptp`.
    fn monotonic_at(&self, gptp: i64) -> i64 {
        let (at, base, rate) = self.load();
        at.saturating_add((gptp.saturating_sub(base) as f64 / rate) as i64)
    }
}

fn monotonic_nanos() -> i64 {
    monotonic_now().as_nanos() as i64
}

/// gPTP time, read through the monotonic clock.
pub struct MediaClock {
    #[cfg(target_os = "linux")]
    source: Option<Mutex<gptp::time::Reader>>,
    line: Line,
    /// The line is a grandmaster's, not the monotonic clock's, and read
    /// recently enough to judge the next record by.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    held: AtomicBool,
    /// A line that does not carry the current one on, until confirmed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    candidate: Mutex<Option<Candidate>>,
    /// When gptpd's mapping was last read, in monotonic nanoseconds.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    measured: AtomicI64,
    /// A grandmaster's time is followed, or this system's given, as gptpd
    /// last said within the second.
    locked: AtomicBool,
    /// Counts the discontinuities: the first grandmaster's line, the first
    /// after the line went unread past [`STALE`], and each that does not
    /// carry the last one on.
    generation: AtomicU32,
    /// How far the last discontinuity moved gPTP time, in nanoseconds.
    last_step: AtomicI64,
}

impl MediaClock {
    /// gPTP time as gptpd gives it for `interface`, on Linux; elsewhere,
    /// and until gptpd gives it, the monotonic clock.
    pub fn gptpd(interface: &str) -> Self {
        #[cfg(target_os = "linux")]
        {
            let clock = MediaClock {
                source: gptp::time::Reader::open(interface).ok().map(Mutex::new),
                ..MediaClock::monotonic()
            };
            clock.refresh(true);
            clock
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = interface;
            MediaClock::monotonic()
        }
    }

    /// The monotonic clock alone: streams flow, but their presentation
    /// times mean nothing to other entities.
    pub fn monotonic() -> Self {
        MediaClock {
            #[cfg(target_os = "linux")]
            source: None,
            line: Line::new(),
            held: AtomicBool::new(false),
            candidate: Mutex::new(None),
            measured: AtomicI64::new(i64::MIN),
            locked: AtomicBool::new(false),
            generation: AtomicU32::new(0),
            last_step: AtomicI64::new(0),
        }
    }

    /// Reads gptpd's mapping again, when it is due or `now` asks, unless
    /// another thread is reading it.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn refresh(&self, now: bool) {
        #[cfg(target_os = "linux")]
        {
            let monotonic = monotonic_nanos();
            let measured = self.measured.load(Ordering::Relaxed);
            if !now && monotonic.saturating_sub(measured) < REFRESH.as_nanos() as i64 {
                return;
            }
            let Some(source) = &self.source else {
                return;
            };
            let Ok(mut reader) = source.try_lock() else {
                return;
            };
            // Swapped under the reader's lock, the last read's time is the
            // one before this, not one another thread has just replaced.
            let last = self.measured.swap(monotonic, Ordering::Relaxed);
            let mapping = reader.read(monotonic).ok().flatten();
            self.take(mapping, monotonic.saturating_sub(last));
        }
    }

    /// Takes a mapping read `since` nanoseconds after the read before it.
    /// A line nothing read for longer than [`STALE`], as while no stream
    /// ran, says nothing of where the next record should stand, so that
    /// record goes in at once, as the first does: a stream starting then
    /// starts on the new line, rather than on the old one until records
    /// confirm the new and then again across the change.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn take(&self, mapping: Option<Mapping>, since: i64) {
        if since > STALE.as_nanos() as i64 {
            self.held.store(false, Ordering::Relaxed);
        }
        self.apply(mapping);
    }

    /// Takes a mapping read, or none. Without a grandmaster's time the
    /// last line carries on, unlocked. A line that does not carry the last
    /// one on waits for [`CONFIRMATIONS`] records in a row agreeing with
    /// it, the last line carrying on meanwhile.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn apply(&self, mapping: Option<Mapping>) {
        let mut candidate = self
            .candidate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(mapping) = mapping.filter(|mapping| mapping.state != State::None) else {
            *candidate = None;
            self.locked.store(false, Ordering::Relaxed);
            return;
        };
        let base = mapping
            .at_monotonic
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        let held = self.held.load(Ordering::Relaxed);
        let step = base - self.line.gptp_at(mapping.monotonic);
        let carried = held && step.abs() <= CARRIED;
        if held && !carried {
            let seen = match *candidate {
                Some(waiting) if (base - waiting.gptp_at(mapping.monotonic)).abs() <= CARRIED => {
                    waiting.seen + 1
                }
                _ => 1,
            };
            if seen < CONFIRMATIONS {
                *candidate = Some(Candidate {
                    at: mapping.monotonic,
                    base,
                    rate: mapping.monotonic_rate,
                    seen,
                });
                self.locked.store(true, Ordering::Relaxed);
                return;
            }
        }
        *candidate = None;
        self.line
            .store(mapping.monotonic, base, mapping.monotonic_rate);
        if !carried {
            self.last_step.store(step, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        self.held.store(true, Ordering::Relaxed);
        self.locked.store(true, Ordering::Relaxed);
    }

    /// Whether gPTP time is a grandmaster's, followed or this system's,
    /// rather than the monotonic clock's or an old line carried on.
    pub fn locked(&self) -> bool {
        self.refresh(false);
        self.locked.load(Ordering::Relaxed)
    }

    /// Counts gPTP time's discontinuities: a stream's timeline built on
    /// it before a change is no longer good after.
    pub fn generation(&self) -> u32 {
        self.refresh(false);
        self.generation.load(Ordering::Relaxed)
    }

    /// How far gPTP time moved at the last discontinuity, in nanoseconds:
    /// against the monotonic clock at the first, against the last line
    /// after.
    pub fn last_step(&self) -> i64 {
        self.last_step.load(Ordering::Relaxed)
    }

    /// gPTP time now, in nanoseconds.
    pub fn now(&self) -> i64 {
        self.refresh(false);
        self.line.gptp_at(monotonic_nanos())
    }

    /// gPTP time less monotonic time, in nanoseconds.
    pub fn offset(&self) -> i64 {
        self.refresh(false);
        let monotonic = monotonic_nanos();
        self.line.gptp_at(monotonic) - monotonic
    }

    /// The monotonic time at which gPTP time reaches `gptp` nanoseconds.
    pub fn monotonic_at(&self, gptp: i64) -> Duration {
        Duration::from_nanos(self.line.monotonic_at(gptp).max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gptp::ClockIdentity;

    /// A grandmaster at `base` ns when the monotonic clock read `at`,
    /// running `rate` times as fast, in generation `generation`.
    fn mapping(generation: u32, at: i64, base: i128, rate: f64) -> Mapping {
        Mapping {
            generation,
            state: State::Following,
            grandmaster: ClockIdentity(0x0001_f2ff_feff_3b14),
            updated: at,
            monotonic: at,
            at_monotonic: base,
            monotonic_rate: rate,
            local: 0,
            at_local: base,
            local_rate: rate,
            asymmetry_included: false,
            residual: Some(12),
        }
    }

    const SECOND: i64 = 1_000_000_000;

    #[test]
    fn without_gptpd_the_monotonic_clock_is_the_time() {
        let clock = MediaClock::monotonic();
        assert!(!clock.locked());
        let before = monotonic_nanos();
        let now = clock.now();
        assert!(now >= before && now - before < SECOND);
        assert_eq!(clock.monotonic_at(now).as_nanos() as i64, now);
        assert_eq!(clock.generation(), 0);
    }

    /// gPTP time runs along the mapping's line, its rate and all, and
    /// wake times come back off it.
    #[test]
    fn time_follows_the_mapping() {
        let clock = MediaClock::monotonic();
        let at = monotonic_nanos();
        let base = 1_793_400_000 * i128::from(SECOND);
        clock.apply(Some(mapping(3, at, base, 1.000_010)));
        assert!(clock.locked());
        assert_eq!(clock.generation(), 1);
        assert_eq!(clock.line.gptp_at(at), base as i64);
        // 10 ppm fast: a second on the monotonic clock is 10 us more.
        assert_eq!(
            clock.line.gptp_at(at + SECOND),
            base as i64 + SECOND + 10_000
        );
        let gptp = base as i64 + 2 * SECOND;
        let wake = clock.line.monotonic_at(gptp);
        assert!((clock.line.gptp_at(wake) - gptp).abs() <= 1);
        let offset = clock.offset();
        assert!((offset - (base as i64 - at)).abs() < SECOND / 1000);
    }

    /// Three records, a tenth of a second apart, of a line from `at`,
    /// gPTP time `base` there.
    fn confirmed(clock: &MediaClock, generation: u32, at: i64, base: i128) {
        for record in 0..3 {
            let later = at + record * SECOND / 10;
            clock.apply(Some(mapping(
                generation,
                later,
                base + i128::from(record * SECOND / 10),
                1.0,
            )));
        }
    }

    /// A line that carries the last one on counts no discontinuity, as
    /// gptpd restarting or taking over as grandmaster gives, whatever its
    /// generation; one that does not counts once three records in a row
    /// confirm it, as does the first.
    #[test]
    fn only_a_line_that_does_not_carry_on_counts() {
        let clock = MediaClock::monotonic();
        clock.apply(Some(mapping(5, 0, 1_000 * i128::from(SECOND), 1.0)));
        assert_eq!(clock.generation(), 1);
        clock.apply(Some(mapping(5, SECOND, 1_001 * i128::from(SECOND), 1.0)));
        // A restart's new generation, its time 400 ns on.
        clock.apply(Some(Mapping {
            state: State::Grandmaster,
            ..mapping(6, 2 * SECOND, 1_002 * i128::from(SECOND) + 400, 1.0)
        }));
        assert_eq!(clock.generation(), 1);
        // A grandmaster of its own time, 20 us off, taken at its third
        // record.
        clock.apply(Some(mapping(
            6,
            3 * SECOND,
            1_003 * i128::from(SECOND) + 20_400,
            1.0,
        )));
        assert_eq!(clock.generation(), 1);
        assert_eq!(clock.line.gptp_at(3 * SECOND), 1_003 * SECOND + 400);
        confirmed(&clock, 6, 3 * SECOND, 1_003 * i128::from(SECOND) + 20_400);
        assert_eq!(clock.generation(), 2);
        assert_eq!(clock.last_step(), 20_000);
        confirmed(&clock, 7, 4 * SECOND, 5_000 * i128::from(SECOND));
        assert_eq!(clock.generation(), 3);
        assert_eq!(clock.line.gptp_at(5 * SECOND), 5_001 * SECOND);
    }

    /// One record off and then back, as gptpd can give while a
    /// comparison settles, moves nothing; nor do two records that
    /// disagree with each other.
    #[test]
    fn a_record_off_and_back_is_no_discontinuity() {
        let clock = MediaClock::monotonic();
        clock.apply(Some(mapping(1, 0, 1_000 * i128::from(SECOND), 1.0)));
        clock.apply(Some(mapping(
            1,
            SECOND,
            1_001 * i128::from(SECOND) + 50_000,
            1.0,
        )));
        clock.apply(Some(mapping(
            1,
            SECOND + SECOND / 10,
            1_001 * i128::from(SECOND + SECOND / 10),
            1.0,
        )));
        clock.apply(Some(mapping(
            1,
            2 * SECOND,
            1_002 * i128::from(SECOND) + 90_000,
            1.0,
        )));
        clock.apply(Some(mapping(
            1,
            2 * SECOND + SECOND / 10,
            1_002 * i128::from(SECOND + SECOND / 10) - 70_000,
            1.0,
        )));
        clock.apply(Some(mapping(
            1,
            2 * SECOND + SECOND / 5,
            1_002 * i128::from(SECOND + SECOND / 5) + 30_000,
            1.0,
        )));
        assert_eq!(clock.generation(), 1);
        assert_eq!(clock.line.gptp_at(3 * SECOND), 1_003 * SECOND);
    }

    /// A line nothing read for hours has drifted with the monotonic
    /// clock's rate, and the next record goes in at once, as the first
    /// does; a record a tenth of a second after its last read still waits
    /// for confirmation.
    #[test]
    fn a_line_unread_for_long_gives_way_at_once() {
        let clock = MediaClock::monotonic();
        clock.take(
            Some(mapping(1, 0, 1_000 * i128::from(SECOND), 1.0)),
            i64::MAX,
        );
        assert_eq!(clock.generation(), 1);
        // Read again a tenth of a second on, 20 us off: a candidate.
        clock.take(
            Some(mapping(
                1,
                SECOND / 10,
                1_000 * i128::from(SECOND) + i128::from(SECOND / 10) + 20_000,
                1.0,
            )),
            SECOND / 10,
        );
        assert_eq!(clock.generation(), 1);
        // Five hours unread, gPTP time where a rate 0.04 ppm off puts it,
        // 720 us from the line: taken at its first record.
        let later = 5 * 3600 * SECOND;
        let base = 1_000 * i128::from(SECOND) + i128::from(later) + 720_000;
        clock.take(Some(mapping(1, later, base, 1.0)), later - SECOND / 10);
        assert_eq!(clock.generation(), 2);
        assert_eq!(clock.line.gptp_at(later), base as i64);
        assert_eq!(clock.last_step(), 720_000);
        // The next read, on time and on the new line, carries it on.
        clock.take(
            Some(mapping(
                1,
                later + SECOND / 10,
                base + i128::from(SECOND / 10),
                1.0,
            )),
            SECOND / 10,
        );
        assert_eq!(clock.generation(), 2);
    }

    /// While gptpd says no grandmaster is present the line holds, and the
    /// first grandmaster after counts only if it does not carry it on.
    #[test]
    fn a_pause_without_a_grandmaster_is_no_discontinuity() {
        let clock = MediaClock::monotonic();
        clock.apply(Some(mapping(1, 0, 1_000 * i128::from(SECOND), 1.0)));
        let none = Mapping {
            state: State::None,
            ..mapping(2, SECOND, 0, 1.0)
        };
        clock.apply(Some(none));
        clock.apply(None);
        clock.apply(Some(mapping(
            2,
            2 * SECOND,
            1_002 * i128::from(SECOND),
            1.0,
        )));
        assert_eq!(clock.generation(), 1);
        clock.apply(Some(none));
        confirmed(&clock, 3, 3 * SECOND, 9_000 * i128::from(SECOND));
        assert_eq!(clock.generation(), 2);
    }

    /// When gptpd says nothing, or that no grandmaster is present, the
    /// last line carries on, unlocked, so time does not jump back to the
    /// monotonic clock's.
    #[test]
    fn without_a_grandmaster_the_last_line_carries_on() {
        let clock = MediaClock::monotonic();
        clock.apply(Some(mapping(1, 0, 1_000 * i128::from(SECOND), 1.0)));
        clock.apply(None);
        assert!(!clock.locked.load(Ordering::Relaxed));
        assert_eq!(clock.line.gptp_at(SECOND), 1_001 * SECOND);
        let none = Mapping {
            state: State::None,
            ..mapping(2, SECOND, 9 * i128::from(SECOND), 1.0)
        };
        clock.apply(Some(none));
        assert!(!clock.locked.load(Ordering::Relaxed));
        assert_eq!(clock.line.gptp_at(2 * SECOND), 1_002 * SECOND);
        assert_eq!(clock.generation.load(Ordering::Relaxed), 1);
        // This system as grandmaster is locked too.
        let ours = Mapping {
            state: State::Grandmaster,
            ..mapping(3, 3 * SECOND, 2_000 * i128::from(SECOND), 1.0)
        };
        clock.apply(Some(ours));
        assert!(clock.locked.load(Ordering::Relaxed));
    }

    /// Threads reading while another stores a line each take one line
    /// whole, never half of two.
    #[test]
    fn a_line_is_read_whole() {
        let line = std::sync::Arc::new(Line::new());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let line = line.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let (at, base, rate) = line.load();
                        // Each line stored has base = 2 × at and rate = at.
                        assert_eq!(base, 2 * at);
                        assert!(at == 0 || rate == at as f64);
                    }
                })
            })
            .collect();
        for at in 1..200_000i64 {
            line.store(at, 2 * at, at as f64);
        }
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }
    }
}
