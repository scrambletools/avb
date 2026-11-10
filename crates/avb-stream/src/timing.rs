//! How the talker's thread keeps time, a second at a time, where
//! `AVB_STREAM_TIMING` names a file to write it to: how late it woke from
//! each sleep to a frame's handover, how long building the frame and
//! sending it took, and how often it was already behind. The thread keeps
//! each second's counts itself and hands them over without waiting; a
//! thread of ordinary priority writes them.

use std::io::Write;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, SystemTime};

/// The variable naming the file.
const VARIABLE: &str = "AVB_STREAM_TIMING";

/// The buckets' upper ends, in microseconds; the last holds the rest.
const BUCKETS: [u64; 6] = [20, 50, 100, 200, 500, 1000];

/// Counts of times in microseconds, in [`BUCKETS`], and the longest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Spread {
    counts: [u32; BUCKETS.len() + 1],
    longest: u64,
}

impl Spread {
    fn add(&mut self, time: Duration) {
        let micros = time.as_micros() as u64;
        let bucket = BUCKETS
            .iter()
            .position(|&end| micros < end)
            .unwrap_or(BUCKETS.len());
        self.counts[bucket] += 1;
        self.longest = self.longest.max(micros);
    }

    fn text(&self) -> String {
        let counts: Vec<String> = self.counts.iter().map(u32::to_string).collect();
        format!("{} max {}", counts.join("/"), self.longest)
    }
}

/// One second's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Second {
    /// When it ended, as the wall clock says.
    ended: Option<SystemTime>,
    /// From the end of each sleep's due time to the thread running.
    woke: Spread,
    /// Frames due when the thread came to them without sleeping.
    behind: u32,
    /// Reading the source and building the frame.
    built: Spread,
    /// Sending it to every destination.
    sent: Spread,
}

/// The talker thread's side.
pub(crate) struct Timing {
    out: Option<SyncSender<Second>>,
    second: Second,
    since: Duration,
}

impl Timing {
    /// Keeps time where `AVB_STREAM_TIMING` names a file, whose writer
    /// starts now; does nothing otherwise.
    pub(crate) fn start(now: Duration) -> Self {
        let out = std::env::var_os(VARIABLE).and_then(|path| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            let (out, seconds) = mpsc::sync_channel(8);
            std::thread::Builder::new()
                .name("avb-timing".into())
                .spawn(move || write_all(file, seconds))
                .ok()?;
            Some(out)
        });
        Self {
            out,
            second: Second::default(),
            since: now,
        }
    }

    pub(crate) fn on(&self) -> bool {
        self.out.is_some()
    }

    /// A frame handed over at `now`: woken `woke` after its due time from
    /// a sleep, or already `None` behind, built in `built` and sent in
    /// `sent`.
    pub(crate) fn frame(
        &mut self,
        now: Duration,
        woke: Option<Duration>,
        built: Duration,
        sent: Duration,
    ) {
        let Some(out) = &self.out else {
            return;
        };
        match woke {
            Some(woke) => self.second.woke.add(woke),
            None => self.second.behind += 1,
        }
        self.second.built.add(built);
        self.second.sent.add(sent);
        if now.saturating_sub(self.since) >= Duration::from_secs(1) {
            self.second.ended = Some(SystemTime::now());
            let _ = out.try_send(std::mem::take(&mut self.second));
            self.since = now;
        }
    }
}

/// Writes each second as a line: its end in UTC, then the counts in each
/// bucket and the longest, in microseconds.
fn write_all(mut file: std::fs::File, seconds: Receiver<Second>) {
    let _ = writeln!(
        file,
        "# woke, built and sent: counts under 20/50/100/200/500/1000 us and over, max us"
    );
    for second in seconds {
        let ended = second
            .ended
            .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
            .unwrap_or_default();
        let of_day = ended.as_secs() % 86_400;
        let line = format!(
            "{:02}:{:02}:{:02}.{:03} woke {} behind {} built {} sent {}\n",
            of_day / 3600,
            of_day / 60 % 60,
            of_day % 60,
            ended.subsec_millis(),
            second.woke.text(),
            second.behind,
            second.built.text(),
            second.sent.text(),
        );
        if file.write_all(line.as_bytes()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_fall_in_their_buckets_and_the_longest_stays() {
        let mut spread = Spread::default();
        for micros in [5, 20, 49, 150, 999, 1000, 4000] {
            spread.add(Duration::from_micros(micros));
        }
        assert_eq!(spread.counts, [1, 2, 0, 1, 0, 1, 2]);
        assert_eq!(spread.longest, 4000);
        assert_eq!(spread.text(), "1/2/0/1/0/1/2 max 4000");
    }

    #[test]
    fn without_the_variable_it_keeps_nothing() {
        // The variable is not set in tests.
        let mut timing = Timing {
            out: None,
            second: Second::default(),
            since: Duration::ZERO,
        };
        timing.frame(
            Duration::from_secs(2),
            Some(Duration::from_micros(30)),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(timing.second, Second::default());
        assert!(!timing.on());
    }
}
