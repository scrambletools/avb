//! Talkers of AAF or AM824: one frame each class interval, paced on gPTP
//! time, its presentation time the max transit time ahead.
//!
//! A group sends several streams on one thread, each from its own source.
//! Their samples are counted on one clock, sample `s` going out at the
//! group's origin plus `s` sample periods, so the streams present
//! together, sample for sample. A media clock stream, CRF, is counted on
//! the same clock, its timestamps the presentation times of the samples
//! they fall on. A [`Talker`] is a group of one.
//!
//! Asked to, with [`TalkerGroup::start_ahead`], and where the interface's
//! queue discipline keeps departure times, as fq does, the thread hands
//! each frame over that far before it is due, stamped with the time it
//! should leave, and the kernel sends it then. It is off by default: fq
//! releases frames from a softirq, which a busy computer leaves to a
//! thread of ordinary priority, and on the bench frames then left late,
//! together and out of order. The card's own launch time, through ETF,
//! is the way meant to replace it.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use atdecc::stream_format::StreamFormat;
use avb_net::MacAddress;
use avb_net::clock::{monotonic_now, precise_sleeps};
use avb_net::stream::FrameSender;

use crate::MediaClock;
use crate::audio::{Input, Source};
use crate::lock::{Guard, Lock};
use crate::media::{Media, Packetizer};
use crate::meter::Peaks;
use crate::recovered::{Fit, RecoveredClock, div_ceil};
use crate::timing::Timing;

/// The VLAN tag's ethertype, and AVTP's.
const ETHERTYPE_VLAN: u16 = 0x8100;
const ETHERTYPE_AVTP: u16 = 0x22f0;
/// The Ethernet header with its VLAN tag.
const ETHERNET_LEN: usize = 18;
/// How long a frame may take to cross the network to its listeners, in
/// nanoseconds: a frame sent later than this before its presentation time
/// would be late.
const CROSSING: i64 = 500_000;
/// How far ahead of its time the thread hands a frame over, stamped with
/// its departure time, where the socket takes departure times.
pub const LAUNCH_AHEAD: Duration = Duration::from_millis(2);
/// A frame handed over after its departure time is stamped this far
/// past the moment it is handed over instead: fq sends a frame whose
/// time has passed at once, as a flow of its own, ahead of earlier frames
/// it is just then releasing, and the stamp keeps it behind them.
const LATE_STAMP: Duration = Duration::from_micros(20);
/// The longest the pacing thread sleeps at once, so that it follows a
/// clock that moved.
const LONGEST_SLEEP: Duration = Duration::from_millis(20);
/// Streams start on a sample a multiple of this, sent on a millisecond:
/// every frame size of 6 to 48 samples divides it, so each stream's
/// frames go out on its class interval's boundaries.
const ALIGN: u64 = 48;
const NANOS: u128 = 1_000_000_000;

/// What a talker sends, and from where.
#[derive(Debug, Clone, PartialEq)]
pub struct TalkerConfig {
    pub interface: String,
    /// The interface's address, the frames' source.
    pub mac: MacAddress,
    pub stream_id: u64,
    /// Where the stream goes: its multicast address, or in AVB Lite a
    /// unicast copy to each listener. [`Talker::set_destinations`] moves
    /// it while it runs.
    pub destinations: Vec<MacAddress>,
    pub vlan_id: u16,
    pub priority: u8,
    pub format: StreamFormat,
    /// How far ahead of sending the presentation times are.
    pub max_transit_time: Duration,
    pub source: Source,
}

/// Where a group's streams go out, and the rate they all run at.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupConfig {
    pub interface: String,
    /// The interface's address, the frames' source.
    pub mac: MacAddress,
    pub rate: u32,
}

/// One of a group's streams.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStream {
    pub stream_id: u64,
    /// Where the stream goes: its multicast address, or in AVB Lite a
    /// unicast copy to each listener.
    pub destinations: Vec<MacAddress>,
    pub vlan_id: u16,
    pub priority: u8,
    /// Its format, at the group's rate.
    pub format: StreamFormat,
    /// How far ahead of sending the presentation times are.
    pub max_transit_time: Duration,
    /// Where its samples come from.
    pub source: Source,
    /// The channels its source gives, which go out as the format's first,
    /// with silence in the rest, and those past the format's not at all.
    pub channels: u16,
}

/// A talker's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TalkerStats {
    pub frames_sent: u64,
    /// Frames left out as due too late to arrive in time, as when the
    /// thread woke late: the stream goes on past them on its timeline.
    pub frames_late: u64,
    /// Times the media clock started again, which the mr bit tells.
    pub media_resets: u64,
    /// Of those, the ones for gPTP time jumping, as to a grandmaster of
    /// other time, or going back.
    pub clock_resets: u64,
    /// Times the input dropped frames as its device ran far ahead, and
    /// reads that found it empty.
    pub adjustments: u64,
    pub underruns: u64,
    /// Frames the interface would not take.
    pub send_errors: u64,
    /// The thread pacing the frames runs real time.
    pub realtime: bool,
    /// How far the audio device's clock runs from gPTP's, in parts per
    /// million.
    pub drift_ppm: i32,
}

/// A stream's own counters, and its audio's peaks.
#[derive(Default)]
struct StreamCounters {
    frames_sent: AtomicU64,
    frames_late: AtomicU64,
    send_errors: AtomicU64,
    peaks: Peaks,
}

/// The group's counters, which its streams share.
#[derive(Default)]
struct GroupCounters {
    media_resets: AtomicU64,
    clock_resets: AtomicU64,
    adjustments: AtomicU64,
    underruns: AtomicU64,
    realtime: AtomicBool,
    drift_ppm: AtomicI32,
}

/// What the group's owner and its thread share.
struct Shared {
    /// The streams wanted, by place, and how often they changed, which
    /// the thread compares with what it last took.
    streams: Lock<Vec<Option<GroupStream>>>,
    changes: AtomicU64,
    /// Each new stream's source, opened for the thread to take; none for
    /// a media clock.
    inputs: Lock<Vec<Option<Input>>>,
    /// The media clock the streams follow, none for gPTP time itself.
    follow: Lock<Option<RecoveredClock>>,
    counters: Vec<StreamCounters>,
    group: GroupCounters,
}

impl Shared {
    fn lock(&self) -> Guard<'_, Vec<Option<GroupStream>>> {
        self.streams.lock()
    }
}

/// A running group of streams. Dropping it stops them.
pub struct TalkerGroup {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    rate: u32,
    ahead: Option<Duration>,
}

impl TalkerGroup {
    /// Starts the group's thread, with room for `places` streams, none
    /// sending yet, each frame sent when it is due.
    pub fn start(config: GroupConfig, places: usize, clock: Arc<MediaClock>) -> io::Result<Self> {
        Self::start_ahead(config, places, clock, None)
    }

    /// As [`TalkerGroup::start`], handing frames over `ahead` early, or
    /// each when it is due for none.
    pub fn start_ahead(
        config: GroupConfig,
        places: usize,
        clock: Arc<MediaClock>,
        ahead: Option<Duration>,
    ) -> io::Result<Self> {
        let sender = FrameSender::open(&config.interface)?;
        // Only where the queue holds a frame until its time: elsewhere a
        // frame handed over early would leave early.
        let room = avb_net::stream::paced_frames(&config.interface);
        let ahead = ahead.filter(|_| room.is_some() && sender.time_departures().is_ok());
        let launch = ahead.map(|ahead| (ahead, room.unwrap_or(u32::MAX)));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            streams: Lock::new(vec![None; places]),
            changes: AtomicU64::new(0),
            inputs: Lock::new((0..places).map(|_| None).collect()),
            follow: Lock::new(None),
            counters: (0..places).map(|_| StreamCounters::default()).collect(),
            group: GroupCounters::default(),
        });
        let rate = config.rate;
        let handle = {
            let stop = stop.clone();
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("talker".into())
                .spawn(move || run(&config, &shared, &sender, &clock, launch, &stop))?
        };
        Ok(TalkerGroup {
            shared,
            stop,
            handle: Some(handle),
            rate,
            ahead,
        })
    }

    /// The sampling rate its source runs at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// How far ahead of their time frames are handed over, stamped with
    /// it; none where each is sent when due.
    pub fn launch_ahead(&self) -> Option<Duration> {
        self.ahead
    }

    /// Starts sending `stream` in `place`, in place of what went there,
    /// opening its source, or stops what went there.
    pub fn set_stream(&self, place: usize, stream: Option<GroupStream>) -> io::Result<()> {
        let input = match &stream {
            Some(stream) => {
                let media = Media::of(stream.format).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "not an AAF, AM824 or CRF format this talker sends",
                    )
                })?;
                if media.frame_samples_at(self.rate).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a stream at another rate than its group's",
                    ));
                }
                if media.is_clock() {
                    None
                } else {
                    Some(Input::open(&stream.source, stream.channels, self.rate)?)
                }
            }
            None => None,
        };
        let mut streams = self.shared.lock();
        let Some(slot) = streams.get_mut(place) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no such place"));
        };
        if let Some(waiting) = self.shared.inputs.lock().get_mut(place) {
            *waiting = input;
        }
        *slot = stream;
        self.shared.changes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Sends `place`'s stream to `destinations` from its next frame on,
    /// none pausing it.
    pub fn set_destinations(&self, place: usize, destinations: Vec<MacAddress>) {
        let mut streams = self.shared.lock();
        if let Some(Some(stream)) = streams.get_mut(place) {
            stream.destinations = destinations;
            self.shared.changes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Times the streams on `clock`, a media clock recovered from a CRF
    /// stream, while it is recovered, and on gPTP time otherwise; `None`
    /// on gPTP time alone. The streams start again on each change of
    /// clock, as a media reset tells their listeners.
    pub fn follow(&self, clock: Option<RecoveredClock>) {
        let mut follow = self.shared.follow.lock();
        if *follow != clock {
            *follow = clock;
            self.shared.changes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether any of its streams sends.
    pub fn sending(&self) -> bool {
        self.shared.lock().iter().any(Option::is_some)
    }

    /// Whether `place`'s stream sends.
    pub fn sends(&self, place: usize) -> bool {
        self.shared.lock().get(place).is_some_and(Option::is_some)
    }

    /// `place`'s counters, with the group's it shares.
    pub fn stats(&self, place: usize) -> TalkerStats {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let group = &self.shared.group;
        let (frames_sent, frames_late, send_errors) =
            self.shared
                .counters
                .get(place)
                .map_or((0, 0, 0), |counters| {
                    (
                        read(&counters.frames_sent),
                        read(&counters.frames_late),
                        read(&counters.send_errors),
                    )
                });
        TalkerStats {
            frames_sent,
            frames_late,
            media_resets: read(&group.media_resets),
            clock_resets: read(&group.clock_resets),
            adjustments: read(&group.adjustments),
            underruns: read(&group.underruns),
            send_errors,
            realtime: group.realtime.load(Ordering::Relaxed),
            drift_ppm: group.drift_ppm.load(Ordering::Relaxed),
        }
    }

    /// The loudest sample in each of the first two channels `place`'s
    /// stream sent since the last time this was asked, from 0 to 1, for a
    /// meter.
    pub fn take_peaks(&self, place: usize) -> [f32; 2] {
        self.shared
            .counters
            .get(place)
            .map_or([0.0; 2], |counters| counters.peaks.take())
    }
}

impl Drop for TalkerGroup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One of a group's streams, sending while it lives: dropping it stops
/// that stream, and the group once none holds it.
pub struct GroupTalker {
    group: Arc<TalkerGroup>,
    place: usize,
}

impl GroupTalker {
    /// Starts `stream` in `group`'s `place`.
    pub fn start(group: &Arc<TalkerGroup>, place: usize, stream: GroupStream) -> io::Result<Self> {
        group.set_stream(place, Some(stream))?;
        Ok(GroupTalker {
            group: group.clone(),
            place,
        })
    }

    /// Sends the stream to `destinations` from its next frame on, none
    /// pausing it.
    pub fn set_destinations(&self, destinations: Vec<MacAddress>) {
        self.group.set_destinations(self.place, destinations);
    }

    pub fn stats(&self) -> TalkerStats {
        self.group.stats(self.place)
    }

    /// The loudest sample in each of the first two channels sent since the
    /// last time this was asked, from 0 to 1.
    pub fn take_peaks(&self) -> [f32; 2] {
        self.group.take_peaks(self.place)
    }
}

impl Drop for GroupTalker {
    fn drop(&mut self) {
        let _ = self.group.set_stream(self.place, None);
    }
}

/// A running talker of one stream. Dropping it stops the stream.
pub struct Talker {
    group: TalkerGroup,
}

impl Talker {
    /// Starts sending `config`'s stream on `clock`'s time, each frame sent
    /// when it is due.
    pub fn start(config: TalkerConfig, clock: Arc<MediaClock>) -> io::Result<Self> {
        Self::start_ahead(config, clock, None)
    }

    /// As [`Talker::start`], handing frames over `ahead` early, or each
    /// when it is due for none.
    pub fn start_ahead(
        config: TalkerConfig,
        clock: Arc<MediaClock>,
        ahead: Option<Duration>,
    ) -> io::Result<Self> {
        let media = Media::of(config.format).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "not an AAF, AM824 or CRF format this talker sends",
            )
        })?;
        let group = TalkerGroup::start_ahead(
            GroupConfig {
                interface: config.interface,
                mac: config.mac,
                rate: media.sample_rate(),
            },
            1,
            clock,
            ahead,
        )?;
        group.set_stream(
            0,
            Some(GroupStream {
                stream_id: config.stream_id,
                destinations: config.destinations,
                vlan_id: config.vlan_id,
                priority: config.priority,
                format: config.format,
                max_transit_time: config.max_transit_time,
                source: config.source,
                channels: media.channels(),
            }),
        )?;
        Ok(Talker { group })
    }

    /// Sends the stream to `destinations` from the next frame on, none
    /// pausing it.
    pub fn set_destinations(&self, destinations: Vec<MacAddress>) {
        self.group.set_destinations(0, destinations);
    }

    pub fn stats(&self) -> TalkerStats {
        self.group.stats(0)
    }

    /// How far ahead of their time frames are handed over, stamped with
    /// it; none where each is sent when due.
    pub fn launch_ahead(&self) -> Option<Duration> {
        self.group.launch_ahead()
    }
}

/// The frame's Ethernet header: destination, source, VLAN tag, ethertype.
fn ethernet_header(mac: MacAddress, stream: &GroupStream) -> [u8; ETHERNET_LEN] {
    let mut header = [0; ETHERNET_LEN];
    header[..6].copy_from_slice(&stream.destinations.first().map_or([0; 6], |mac| mac.0));
    header[6..12].copy_from_slice(&mac.0);
    header[12..14].copy_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
    let tag = (u16::from(stream.priority & 0x7) << 13) | (stream.vlan_id & 0x0fff);
    header[14..16].copy_from_slice(&tag.to_be_bytes());
    header[16..18].copy_from_slice(&ETHERTYPE_AVTP.to_be_bytes());
    header
}

/// A stream the thread sends.
struct Running {
    stream: GroupStream,
    /// Where its samples come from; none for a media clock.
    input: Option<Input>,
    packetizer: Packetizer,
    frame: Vec<u8>,
    samples: Vec<f32>,
    /// The format's channels, which its samples' frames have.
    channels: usize,
    /// The group's samples each of its frames spans.
    frame_samples: u64,
    /// The first sample of its next frame.
    next: u64,
    /// The mr bit its frames carry.
    media_reset: MediaReset,
}

/// A stream's mr bit, which changes as its media clock starts again. A
/// start no frame has carried yet goes out with the one before it: two
/// changes with no frame between would cancel, and a listener would see
/// the timeline jump with the bit as it was.
#[derive(Debug, Default)]
struct MediaReset {
    bit: bool,
    /// Whether a frame sent has carried the bit since it last changed.
    carried: bool,
}

impl MediaReset {
    /// The media clock starts again.
    fn restart(&mut self) {
        if self.carried {
            self.bit = !self.bit;
            self.carried = false;
        }
    }

    /// A frame with the bit went out, to at least one destination.
    fn sent(&mut self) {
        self.carried = true;
    }
}

impl Running {
    fn new(
        mac: MacAddress,
        stream: GroupStream,
        media: Media,
        input: Option<Input>,
        frame_samples: u64,
    ) -> Self {
        let mut frame = vec![0u8; ETHERNET_LEN + media.pdu_length()];
        frame[..ETHERNET_LEN].copy_from_slice(&ethernet_header(mac, &stream));
        let samples =
            vec![0.0f32; usize::from(media.channels()) * usize::from(media.samples_per_frame())];
        Running {
            packetizer: Packetizer::new(media, stream.stream_id),
            stream,
            input,
            frame,
            samples,
            channels: usize::from(media.channels()),
            frame_samples: frame_samples.max(1),
            next: 0,
            media_reset: MediaReset::default(),
        }
    }

    fn frame_samples(&self) -> u64 {
        self.frame_samples
    }

    /// Its next frame's first sample at or after sample `from`.
    fn first_from(&self, from: u64) -> u64 {
        from.div_ceil(self.frame_samples()) * self.frame_samples()
    }

    /// Leaves out its frames before sample `from`, as too late to send,
    /// and gives how many: what comes after stays on the timeline, so a
    /// listener misses those frames alone, where a restart would have it
    /// lock again.
    fn skip_to(&mut self, from: u64) -> u64 {
        let frames = self.first_from(from).saturating_sub(self.next) / self.frame_samples();
        if let Some(input) = &mut self.input {
            input.skip(frames as usize * self.samples.len() / self.channels.max(1));
        }
        self.packetizer.skip(frames);
        self.next += frames * self.frame_samples();
        frames
    }
}

/// The group's clock: sample `s` goes out at gPTP time `origin` plus `s`
/// sample periods, or, following a media clock, where that clock puts it.
struct Timeline {
    rate: u64,
    origin: i64,
    /// The media clock followed, and where the samples fall on it.
    follow: Option<Following>,
    /// The latest line of the media clock to follow, which a restart
    /// takes up.
    wanted: Option<Fit>,
}

/// Samples on a recovered media clock: sample `sample` falls on its event
/// `sub / rate`, and each sample takes `base` of those `1 / rate` events.
#[derive(Debug, Clone, Copy)]
struct Following {
    fit: Fit,
    sample: u64,
    sub: i128,
    base: i128,
}

impl Timeline {
    fn at(&self, sample: u64) -> i64 {
        match &self.follow {
            Some(following) => {
                let samples = i128::from(sample) - i128::from(following.sample);
                following
                    .fit
                    .time_at(following.sub + samples * following.base, self.rate)
            }
            None => self.origin + (u128::from(sample) * NANOS / u128::from(self.rate)) as i64,
        }
    }

    /// The first sample sent at or after gPTP time `time`.
    fn sample_at(&self, time: i64) -> u64 {
        match &self.follow {
            Some(following) => {
                let sub = following.fit.sub_at(time, self.rate);
                let samples = div_ceil(sub - following.sub, following.base);
                (i128::from(following.sample) + samples).max(0) as u64
            }
            None => {
                let since = (time - self.origin).max(0);
                (since as u128 * u128::from(self.rate)).div_ceil(NANOS) as u64
            }
        }
    }

    /// Takes the followed clock's latest line, none while it is not
    /// recovered: the same clock moves the samples with it; whether the
    /// samples must start again on another, or on gPTP time, it tells.
    fn take(&mut self, fit: Option<Fit>) -> bool {
        self.wanted = fit;
        match (fit, self.follow.as_mut()) {
            (Some(fit), Some(following)) if following.fit.generation == fit.generation => {
                following.fit = fit;
                false
            }
            (None, None) => false,
            _ => true,
        }
    }

    /// Starts again so that sample `from`, rounded up to the alignment,
    /// goes out at least a millisecond after `now`: on a millisecond of
    /// gPTP time, or on an event of the media clock followed; gives that
    /// sample.
    fn restart(&mut self, from: u64, now: i64) -> u64 {
        let first = from.div_ceil(ALIGN) * ALIGN;
        let boundary = (now / 1_000_000 + 2) * 1_000_000;
        match self.wanted {
            Some(fit) => {
                let rate = i128::from(self.rate);
                let edge = div_ceil(fit.sub_at(boundary, self.rate), rate) * rate;
                self.follow = Some(Following {
                    fit,
                    sample: first,
                    sub: edge,
                    base: i128::from(fit.base_frequency),
                });
            }
            None => {
                self.follow = None;
                self.origin = boundary - (u128::from(first) * NANOS / u128::from(self.rate)) as i64;
            }
        }
        first
    }
}

/// Takes the streams wanted: those gone stop, new ones start on their
/// next frame a millisecond on, changed ones start again, and those whose
/// destinations moved send there.
fn take_changes(
    config: &GroupConfig,
    shared: &Shared,
    running: &mut [Option<Running>],
    timeline: &mut Timeline,
    clock: &MediaClock,
) {
    let wanted = shared.lock().clone();
    let mut inputs = shared.inputs.lock();
    let idle = running.iter().all(Option::is_none);
    let mut from = None;
    for (place, wanted) in wanted.into_iter().enumerate() {
        let Some(slot) = running.get_mut(place) else {
            continue;
        };
        match (wanted, slot.as_mut()) {
            (None, _) => *slot = None,
            (Some(stream), Some(now)) if same_but_destinations(&stream, &now.stream) => {
                if stream.destinations != now.stream.destinations {
                    now.frame[..ETHERNET_LEN]
                        .copy_from_slice(&ethernet_header(config.mac, &stream));
                    now.stream.destinations = stream.destinations;
                }
            }
            (Some(stream), _) => {
                let input = inputs.get_mut(place).and_then(Option::take);
                let Some((media, frame_samples)) = Media::of(stream.format)
                    .and_then(|media| Some((media, media.frame_samples_at(config.rate)?)))
                else {
                    *slot = None;
                    continue;
                };
                // An audio stream needs its source; a media clock has none.
                if input.is_none() && !media.is_clock() {
                    *slot = None;
                    continue;
                }
                // The first stream after none starts the clock again.
                let start = *from.get_or_insert_with(|| {
                    let now = clock.now();
                    if idle {
                        timeline.restart(timeline.sample_at(now), now)
                    } else {
                        timeline.sample_at(now + 1_000_000)
                    }
                });
                let mut made = Running::new(config.mac, stream, media, input, frame_samples);
                made.next = made.first_from(start);
                *slot = Some(made);
            }
        }
    }
}

/// Whether two streams differ in their destinations alone.
fn same_but_destinations(one: &GroupStream, other: &GroupStream) -> bool {
    GroupStream {
        destinations: Vec::new(),
        ..one.clone()
    } == GroupStream {
        destinations: Vec::new(),
        ..other.clone()
    }
}

fn run(
    config: &GroupConfig,
    shared: &Shared,
    sender: &FrameSender,
    clock: &MediaClock,
    launch: Option<(Duration, u32)>,
    stop: &AtomicBool,
) {
    precise_sleeps();
    shared
        .group
        .realtime
        .store(crate::realtime::raise(), Ordering::Relaxed);
    let mut running: Vec<Option<Running>> = (0..shared.counters.len()).map(|_| None).collect();
    let mut timeline = Timeline {
        rate: u64::from(config.rate.max(1)),
        origin: 0,
        follow: None,
        wanted: None,
    };
    let mut seen = u64::MAX;
    let mut frames: u64 = 0;
    let mut follow: Option<RecoveredClock> = None;
    let mut switched = false;
    let mut generation = clock.generation();
    let mut ahead = launch.map(|(ahead, _)| ahead);
    let mut timing = Timing::start(monotonic_now());
    while !stop.load(Ordering::Relaxed) {
        let changes = shared.changes.load(Ordering::Relaxed);
        if changes != seen {
            seen = changes;
            let wanted = shared.follow.lock().clone();
            // Leaving a clock followed starts again; samples on gPTP time
            // start again once the new clock is recovered.
            if wanted != follow {
                follow = wanted;
                switched = timeline.follow.take().is_some();
            }
            timeline.take(follow.as_ref().and_then(RecoveredClock::fit));
            take_changes(config, shared, &mut running, &mut timeline, clock);
            ahead = fitting(launch, &running, timeline.rate);
        }
        // The media clock followed moves the samples with it; one that
        // comes, goes or starts again starts them again on it, or on gPTP
        // time.
        let retime = timeline.take(follow.as_ref().and_then(RecoveredClock::fit));
        // gPTP time jumped, as to a new grandmaster: the timeline built on
        // the old one is no good, and the streams start again on the new.
        let jumped = clock.generation() != generation;
        generation = clock.generation();
        if (retime || jumped || std::mem::take(&mut switched))
            && running.iter().any(Option::is_some)
        {
            restart(&mut running, &mut timeline, clock);
            shared.group.media_resets.fetch_add(1, Ordering::Relaxed);
            if jumped {
                shared.group.clock_resets.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The stream whose frame is due first.
        let Some((place, due)) = running
            .iter()
            .enumerate()
            .filter_map(|(place, stream)| {
                stream
                    .as_ref()
                    .map(|stream| (place, timeline.at(stream.next)))
            })
            .min_by_key(|(_, due)| *due)
        else {
            std::thread::sleep(LONGEST_SLEEP);
            continue;
        };
        // When the frame should leave, and when the thread hands it over.
        let departure = clock.monotonic_at(due);
        let wake = departure.saturating_sub(ahead.unwrap_or_default());
        let now = monotonic_now();
        // How late the thread woke from its sleep, where timing is kept;
        // none where it was behind already.
        let mut woke = None;
        if wake > now {
            let wait = wake - now;
            std::thread::sleep(wait.min(LONGEST_SLEEP));
            if wait > LONGEST_SLEEP {
                // Too far ahead for the clock to have only drifted: it
                // went back, and the streams start again from it.
                if due - clock.now() > 2 * LONGEST_SLEEP.as_nanos() as i64 {
                    restart(&mut running, &mut timeline, clock);
                    shared.group.media_resets.fetch_add(1, Ordering::Relaxed);
                    shared.group.clock_resets.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            if timing.on() {
                woke = Some(monotonic_now().saturating_sub(wake));
            }
        }
        // Every frame's lateness, slept to or behind, and those left out
        // below as well, where timing is kept.
        let handover = timing.on().then(monotonic_now);
        if let Some(handover) = handover {
            timing.handed(handover, departure, woke);
        }
        let Some(stream) = running[place].as_mut() else {
            continue;
        };
        // A frame sent late still arrives in time while its presentation
        // time is far enough ahead.
        let transit = stream.stream.max_transit_time.as_nanos() as i64;
        let allowed = (transit - CROSSING).max(transit / 2);
        let now = clock.now();
        let counters = &shared.counters[place];
        if now - due > allowed {
            // Too late to arrive in time, as when the thread woke late:
            // the stream goes on from its first frame late by half the
            // allowance at most, past one at least. The other half is left
            // for sending it, which a thread just held up often finds slow
            // as well.
            let from = timeline.sample_at(now - allowed / 2).max(stream.next + 1);
            let skipped = stream.skip_to(from);
            counters.frames_late.fetch_add(skipped, Ordering::Relaxed);
            timing.skipped(skipped);
            continue;
        }
        if let Some(input) = &mut stream.input {
            input.read_as(&mut stream.samples, stream.channels);
        }
        counters.peaks.raise(&stream.samples, stream.channels);
        stream.packetizer.write(
            due + transit,
            stream.media_reset.bit,
            &stream.samples,
            &mut stream.frame[ETHERNET_LEN..],
        );
        // The thread can be held up after the check as well, reading the
        // source or preempted: a frame that would now arrive late is left
        // out, the packetizer having counted past it already.
        if clock.now() - due > allowed {
            stream.next += stream.frame_samples();
            counters.frames_late.fetch_add(1, Ordering::Relaxed);
            timing.skipped(1);
            continue;
        }
        let stamp = departure.max(monotonic_now() + LATE_STAMP).as_nanos() as u64;
        let built = timing.on().then(monotonic_now);
        for destination in &stream.stream.destinations {
            stream.frame[..6].copy_from_slice(&destination.0);
            let sent = match ahead {
                Some(_) => sender.send_at(&stream.frame, stamp),
                None => sender.send(&stream.frame),
            };
            match sent {
                Ok(()) => {
                    counters.frames_sent.fetch_add(1, Ordering::Relaxed);
                    stream.media_reset.sent();
                }
                Err(_) => {
                    counters.send_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let (Some(handover), Some(built)) = (handover, built) {
            let sent = monotonic_now();
            timing.sent(built - handover, sent - built);
        }
        stream.next += stream.frame_samples();
        frames += 1;
        if frames.is_multiple_of(800) {
            // Each tenth of a second at 8000 frames a second, the sources'
            // own counts.
            let (mut made, mut empty, mut drift) = (0, 0, 0);
            for input in running
                .iter()
                .flatten()
                .filter_map(|stream| stream.input.as_ref())
            {
                let (adjusted, short) = input.counts();
                made += adjusted;
                empty += short;
                if drift == 0 {
                    drift = input.drift_ppm();
                }
            }
            shared.group.adjustments.store(made, Ordering::Relaxed);
            shared.group.drift_ppm.store(drift, Ordering::Relaxed);
            shared.group.underruns.store(empty, Ordering::Relaxed);
        }
    }
}

/// The head start a group's frames get: as asked, but short enough that
/// the frames held for their times, every stream's and each copy of one
/// sent to several listeners, fill no more than half of what the queue
/// lets one flow, the group's socket, hold.
fn fitting(
    launch: Option<(Duration, u32)>,
    running: &[Option<Running>],
    rate: u64,
) -> Option<Duration> {
    let (ahead, room) = launch?;
    let per_second: f64 = running
        .iter()
        .flatten()
        .map(|stream| {
            stream.stream.destinations.len() as f64 * rate as f64
                / stream.frame_samples().max(1) as f64
        })
        .sum();
    if per_second <= 0.0 {
        return Some(ahead);
    }
    Some(ahead.min(Duration::from_secs_f64(f64::from(room) / 2.0 / per_second)))
}

/// Starts every stream again on the clock as it is now, which the mr
/// bit tells.
fn restart(running: &mut [Option<Running>], timeline: &mut Timeline, clock: &MediaClock) {
    let now = clock.now();
    let first = timeline.restart(timeline.sample_at(now), now);
    for stream in running.iter_mut().flatten() {
        stream.next = stream.first_from(first);
        stream.media_reset.restart();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples map to gPTP time exactly, every stream's frames on its
    /// interval: 6 samples at 48 kHz each 125 µs, 12 each 250 µs.
    #[test]
    fn the_timeline_counts_samples_on_gptp_time() {
        let mut timeline = Timeline {
            rate: 48_000,
            origin: 0,
            follow: None,
            wanted: None,
        };
        let first = timeline.restart(5, 7_400_000);
        assert_eq!(first, 48);
        assert_eq!(timeline.at(first), 9_000_000);
        assert_eq!(timeline.at(first + 6) - timeline.at(first), 125_000);
        assert_eq!(timeline.at(first + 12) - timeline.at(first), 250_000);
        assert_eq!(timeline.sample_at(timeline.at(first + 6)), first + 6);
        // A time between samples gives the next sample.
        assert_eq!(timeline.sample_at(timeline.at(first) + 1), first + 1);
    }

    /// Following a media clock 50 ppm fast at 48 kHz, a 192 kHz group's
    /// samples start on one of its events and run 50 ppm fast with it,
    /// four to an event; another start of the clock, or its loss, starts
    /// them again.
    #[test]
    fn the_timeline_follows_a_media_clock() {
        // Events 20833.33 ns apart, less 50 ppm, 32.32.
        let period = (1e9 / 48_000.0 / (1.0 + 50e-6) * 4_294_967_296.0) as u64;
        let fit = Fit {
            generation: 3,
            event: 960_000,
            time: 21_000_000_000,
            period,
            base_frequency: 48_000,
        };
        let mut timeline = Timeline {
            rate: 192_000,
            origin: 0,
            follow: None,
            wanted: None,
        };
        assert!(timeline.take(Some(fit)), "a clock to follow starts again");
        let first = timeline.restart(1000, 21_000_000_000);
        assert_eq!(first, 1008);
        let start = timeline.at(first);
        assert!(start >= 21_002_000_000, "{start}");
        // On an event: a whole number of periods from the line's.
        let events = fit.sub_at(start, 1) - i128::from(fit.event);
        assert_eq!(fit.time_at(i128::from(fit.event) + events, 1), start);
        // A second of samples is 50 ppm short of a second.
        let second = timeline.at(first + 192_000) - start;
        assert!((second - 999_950_002).abs() <= 2, "{second}");
        assert_eq!(
            timeline.at(first + 4) - start,
            fit.time_at(i128::from(fit.event) + events + 1, 1) - start
        );
        assert_eq!(timeline.sample_at(timeline.at(first + 7)), first + 7);
        // The same clock's next line moves the samples on.
        let later = Fit {
            event: fit.event + 96,
            time: fit.time_at(i128::from(fit.event) + 96, 1),
            ..fit
        };
        assert!(!timeline.take(Some(later)));
        assert_eq!(timeline.at(first + 192_000) - start, second);
        // A new start of the clock, or none, starts again.
        assert!(timeline.take(Some(Fit {
            generation: 4,
            ..later
        })));
        assert!(timeline.take(None));
        timeline.restart(first + 192_000, 22_500_000_000);
        assert!(timeline.follow.is_none());
        assert!(!timeline.take(None));
    }

    /// Frames too late to send are left out on the stream's timeline: the
    /// next goes out as it would have, its sequence number, presentation
    /// time, samples and mr bit, with only those between missing.
    #[test]
    fn late_frames_are_left_out() {
        let format = StreamFormat(0x0205_0220_0200_6000);
        let media = Media::of(format).unwrap();
        let source = Source::Tone {
            hertz: 1000.0,
            level: 0.5,
        };
        let stream = GroupStream {
            stream_id: 1,
            destinations: Vec::new(),
            vlan_id: 2,
            priority: 3,
            format,
            max_transit_time: Duration::from_millis(2),
            source: source.clone(),
            channels: 8,
        };
        let start = || {
            let input = Input::open(&source, 8, 48_000).unwrap();
            let mut made = Running::new(MacAddress([0; 6]), stream.clone(), media, Some(input), 6);
            made.next = 480;
            made
        };
        let send = |running: &mut Running| {
            let input = running.input.as_mut().unwrap();
            input.read_as(&mut running.samples, running.channels);
            let presentation = 9_000_000 + running.next as i64 * 1_000_000_000 / 48_000;
            running.packetizer.write(
                presentation,
                running.media_reset.bit,
                &running.samples,
                &mut running.frame[ETHERNET_LEN..],
            );
            running.next += running.frame_samples();
            let mut samples = Vec::new();
            let received =
                crate::media::read(&running.frame[ETHERNET_LEN..], &media, &mut samples).unwrap();
            (received, samples)
        };
        let (mut every, mut late) = (start(), start());
        for _ in 0..4 {
            send(&mut every);
        }
        // Sample 500 falls in the fourth frame, so the fifth is next.
        assert_eq!(late.skip_to(500), 4);
        assert_eq!(late.next, every.next);
        let (wanted, wanted_samples) = send(&mut every);
        let (got, samples) = send(&mut late);
        assert_eq!(got, wanted);
        assert_eq!(got.sequence, 4);
        assert!(!got.media_reset);
        assert!(
            samples
                .iter()
                .zip(&wanted_samples)
                .all(|(sample, wanted)| (sample - wanted).abs() < 1e-4),
            "the tone goes on where it would have"
        );
        // A sample the stream is past leaves nothing out.
        assert_eq!(late.skip_to(late.next), 0);
    }

    /// The mr bit changes for a start of the media clock once a frame sent
    /// has carried it as it was: two starts with no frame sent between, as
    /// a late send and then a jump of gPTP time, or a frame no destination
    /// took, change it once, and a start before the first frame not at all.
    #[test]
    fn the_mr_bit_tells_each_start() {
        let mut media_reset = MediaReset::default();
        media_reset.restart();
        assert!(!media_reset.bit, "nothing to tell before the first frame");
        media_reset.sent();
        media_reset.restart();
        media_reset.restart();
        assert!(media_reset.bit, "two starts change the bit once");
        // A frame that went nowhere tells nothing.
        media_reset.restart();
        assert!(media_reset.bit);
        media_reset.sent();
        media_reset.restart();
        assert!(!media_reset.bit);
    }
}
