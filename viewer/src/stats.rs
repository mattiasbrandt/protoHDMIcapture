//! What the display run measures, against #5's go criteria: dequeue ->
//! presented (gating), kernel timestamp -> presented (reported), and how
//! often a refresh went by with no new frame on screen.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::present::Tag;

const WINDOW: Duration = Duration::from_secs(10);

/// Median and p99 of a set of durations, in milliseconds.
pub fn percentiles(samples: &mut [Duration]) -> Option<(f64, f64)> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    Some((at(0.5), at(0.99)))
}

fn fmt(samples: &mut [Duration]) -> String {
    match percentiles(samples) {
        Some((med, p99)) => format!("median {med:6.2} ms  p99 {p99:6.2} ms"),
        None => "no samples".into(),
    }
}

/// `fmt` plus the extremes and p1: the shape of the spread, not just its middle.
fn fmt_full(samples: &mut [Duration]) -> String {
    if samples.is_empty() {
        return "no samples".into();
    }
    let s = fmt(samples); // sorts
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let p1 = samples[((samples.len() - 1) as f64 * 0.01).round() as usize];
    format!("{s}  min {:6.2}  p1 {:6.2}  max {:6.2} ms", ms(samples[0]), ms(p1), ms(samples[samples.len() - 1]))
}

#[derive(Default)]
struct Window {
    to_present: Vec<Duration>,
    kernel_to_present: Vec<Duration>,
    skipped_refreshes: u64,
}

pub struct Stats {
    /// Print each 10 s window (measurements, --stats).
    verbose: bool,
    start: Duration,
    to_present: Vec<Duration>,
    kernel_to_present: Vec<Duration>,
    acquire: Vec<Duration>,
    submitted: u64,
    presented: u64,
    discarded: u64,
    not_acquired: u64,
    /// Pacing waited `PACE_TIMEOUT` for feedback that never came.
    pace_timeouts: u64,
    /// Presented before it was dequeued: a clock or bookkeeping error.
    before_dequeue: u64,
    /// Refreshes that passed between two presents with nothing new shown.
    skipped_refreshes: u64,
    last_present: Option<Duration>,
    refresh: Option<Duration>,
    flags: BTreeMap<u32, u64>,
    window: Window,
    window_start: Duration,
}

impl Stats {
    pub fn new(start: Duration, verbose: bool) -> Self {
        Self {
            verbose,
            start,
            to_present: Vec::with_capacity(64 * 1024),
            kernel_to_present: Vec::with_capacity(64 * 1024),
            acquire: Vec::with_capacity(64 * 1024),
            submitted: 0,
            presented: 0,
            discarded: 0,
            not_acquired: 0,
            pace_timeouts: 0,
            before_dequeue: 0,
            skipped_refreshes: 0,
            last_present: None,
            refresh: None,
            flags: BTreeMap::new(),
            window: Window::default(),
            window_start: start,
        }
    }

    pub fn submitted(&mut self) {
        self.submitted += 1;
    }

    pub fn acquired(&mut self, wait: Duration) {
        if self.verbose {
            self.acquire.push(wait);
        }
    }

    pub fn pace_timeout(&mut self) {
        self.pace_timeouts += 1;
    }

    pub fn not_acquired(&mut self) {
        self.not_acquired += 1;
    }

    pub fn discarded(&mut self) {
        self.discarded += 1;
    }

    pub fn presented(&mut self, tag: Tag, at: Duration, refresh: Duration, flags: u32) {
        self.presented += 1;
        // Samples are kept only to be printed (--stats): a viewer runs for
        // hours. The dashboard (#16) gets its own rolling numbers.
        if !self.verbose {
            return;
        }
        *self.flags.entry(flags).or_default() += 1;
        if !refresh.is_zero() {
            self.refresh = Some(refresh);
        }
        if at < tag.dequeued {
            self.before_dequeue += 1;
            return;
        }
        let d = at - tag.dequeued;
        let k = at - tag.captured;
        self.to_present.push(d);
        self.kernel_to_present.push(k);
        self.window.to_present.push(d);
        self.window.kernel_to_present.push(k);

        if let (Some(prev), Some(r)) = (self.last_present, self.refresh) {
            // Presents a refresh apart are back to back; each further
            // refresh between them showed the same frame again.
            let refreshes = ((at - prev).as_secs_f64() / r.as_secs_f64()).round() as u64;
            let skipped = refreshes.saturating_sub(1);
            self.skipped_refreshes += skipped;
            self.window.skipped_refreshes += skipped;
        }
        self.last_present = Some(at);

        if at - self.window_start >= WINDOW {
            let w = &mut self.window;
            eprintln!(
                "{:6.1} s  dequeue->present {}  | kernel->present {}  | repeated refreshes {}",
                (at - self.start).as_secs_f64(),
                fmt(&mut w.to_present),
                fmt(&mut w.kernel_to_present),
                w.skipped_refreshes
            );
            self.window = Window::default();
            self.window_start = at;
        }
    }

    pub fn summary(&mut self, superseded: u64) {
        eprintln!(
            "summary: submitted {}, presented {}, discarded {}, not acquired {}, superseded before upload {}, presented before dequeue {}, pace timeouts {}",
            self.submitted, self.presented, self.discarded, self.not_acquired, superseded, self.before_dequeue, self.pace_timeouts
        );
        eprintln!("summary: dequeue->present {}  (gating: median <= 16.7, p99 <= 33.3)", fmt_full(&mut self.to_present));
        eprintln!("summary: kernel->present  {}", fmt_full(&mut self.kernel_to_present));
        eprintln!("summary: get_current_texture wait {}", fmt(&mut self.acquire));
        eprintln!(
            "summary: refresh {}, repeated refreshes {}",
            self.refresh.map_or("unknown".into(), |r| format!("{:.3} Hz", 1.0 / r.as_secs_f64())),
            self.skipped_refreshes
        );
        // wp_presentation_feedback.kind: 1 vsync, 2 hw_clock, 4 hw_completion, 8 zero_copy.
        let flags: Vec<String> = self.flags.iter().map(|(f, n)| format!("{f:#x}x{n}")).collect();
        eprintln!("summary: presentation flags {}", flags.join(" "));
    }
}
