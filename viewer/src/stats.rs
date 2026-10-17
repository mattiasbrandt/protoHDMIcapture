//! What the display run measures, against #5's go criteria: dequeue ->
//! presented (gating), kernel timestamp -> presented (reported), and how
//! often a refresh went by with no new frame on screen. Each present also
//! goes to the run's dashboard numbers (dashboard.rs).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::dashboard::Live;
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
    /// The run's dashboard numbers.
    live: Arc<Live>,
    start: Duration,
    to_present: Vec<Duration>,
    kernel_to_present: Vec<Duration>,
    acquire: Vec<Duration>,
    /// GPU time of the convert and the scale pass, where measured.
    gpu_convert: Vec<Duration>,
    gpu_scale: Vec<Duration>,
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
    pub fn new(start: Duration, verbose: bool, live: Arc<Live>) -> Self {
        Self {
            verbose,
            live,
            start,
            to_present: Vec::with_capacity(64 * 1024),
            kernel_to_present: Vec::with_capacity(64 * 1024),
            acquire: Vec::with_capacity(64 * 1024),
            gpu_convert: Vec::with_capacity(64 * 1024),
            gpu_scale: Vec::with_capacity(64 * 1024),
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

    pub fn gpu(&mut self, convert: Duration, scale: Duration) {
        if self.verbose {
            self.gpu_convert.push(convert);
            self.gpu_scale.push(scale);
        }
    }

    pub fn pace_timeout(&mut self) {
        self.pace_timeouts += 1;
    }

    pub fn not_acquired(&mut self) {
        self.not_acquired += 1;
    }

    /// A frame the compositor never showed: the window is hidden (Hyprland
    /// discards every frame of one in the scratchpad), so the refreshes
    /// until the next present showed no frame of ours to repeat.
    pub fn discarded(&mut self) {
        self.discarded += 1;
        self.last_present = None;
    }

    /// The picture back after no signal (an HDMI pull, the card pulled
    /// out, #40): the refreshes without frames repeated no frame by being
    /// late, so the count starts over, as after a discard. At the return,
    /// not at the loss: the outcomes of the last presents before it come
    /// in after the no-signal state, with their earlier times.
    pub fn picture_back(&mut self) {
        self.last_present = None;
    }

    pub fn presented(&mut self, tag: Tag, at: Duration, refresh: Duration, flags: u32) {
        if tag.repaint {
            return;
        }
        self.presented += 1;
        if self.verbose {
            *self.flags.entry(flags).or_default() += 1;
        }
        if !refresh.is_zero() {
            self.refresh = Some(refresh);
        }
        if at < tag.dequeued {
            self.before_dequeue += 1;
            return;
        }
        let d = at - tag.dequeued;
        let k = at - tag.captured;
        let mut skipped = 0;
        if let (Some(prev), Some(r)) = (self.last_present, self.refresh) {
            // Presents a refresh apart are back to back; each further
            // refresh between them showed the same frame again.
            let refreshes = ((at - prev).as_secs_f64() / r.as_secs_f64()).round() as u64;
            skipped = refreshes.saturating_sub(1);
            self.skipped_refreshes += skipped;
        }
        self.last_present = Some(at);
        // The dashboard's rolling numbers (#16), at every run.
        self.live.presented(d, k, skipped, self.refresh);
        // Samples are kept only to be printed (--stats): a viewer runs for
        // hours.
        if !self.verbose {
            return;
        }
        self.to_present.push(d);
        self.kernel_to_present.push(k);
        self.window.to_present.push(d);
        self.window.kernel_to_present.push(k);
        self.window.skipped_refreshes += skipped;

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
        if !self.gpu_scale.is_empty() {
            eprintln!(
                "summary: GPU time ({} frames)  convert {}  | scale {}",
                self.gpu_scale.len(),
                fmt(&mut self.gpu_convert),
                fmt(&mut self.gpu_scale)
            );
        }
        eprintln!(
            "summary: refresh {}, repeated refreshes {}",
            self.refresh.map_or("unknown".into(), |r| format!("{:.3} Hz", 1.0 / r.as_secs_f64())),
            self.skipped_refreshes
        );
        // wp_presentation_feedback.kind: 1 vsync, 2 hw_clock, 4 hw_completion, 8 zero_copy.
        // DXGI (Windows): 0 unknown, else the presentation mode + 1 (present/dxgi.rs).
        let flags: Vec<String> = self.flags.iter().map(|(f, n)| format!("{f:#x}x{n}")).collect();
        eprintln!("summary: presentation flags {}", flags.join(" "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::Live;

    const REFRESH: Duration = Duration::from_micros(16_681);

    fn present(s: &mut Stats, at: Duration) {
        let tag = Tag { captured: at - Duration::from_millis(30), dequeued: at - Duration::from_millis(20), repaint: false };
        s.presented(tag, at, REFRESH, 0x7);
    }

    #[test]
    fn a_late_present_counts_the_refreshes_between() {
        let mut s = Stats::new(Duration::ZERO, false, Arc::new(Live::new()));
        present(&mut s, Duration::from_secs(1));
        present(&mut s, Duration::from_secs(1) + REFRESH * 3);
        assert_eq!(s.skipped_refreshes, 2);
    }

    #[test]
    fn refreshes_while_the_compositor_discards_are_not_repeats() {
        // Hidden (Hyprland's scratchpad): every frame is discarded, then the
        // window is back 7 s later.
        let mut s = Stats::new(Duration::ZERO, false, Arc::new(Live::new()));
        present(&mut s, Duration::from_secs(1));
        s.discarded();
        present(&mut s, Duration::from_secs(8));
        present(&mut s, Duration::from_secs(8) + REFRESH);
        assert_eq!(s.skipped_refreshes, 0);
    }

    #[test]
    fn refreshes_without_a_picture_are_not_repeats() {
        // The card pulled out at 1 s and back at 6 s (#40): the laptop's
        // perf line said "repeated 595" for the time without frames.
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, false, live.clone());
        present(&mut s, Duration::from_secs(1));
        s.picture_back();
        present(&mut s, Duration::from_secs(6));
        present(&mut s, Duration::from_secs(6) + REFRESH);
        assert_eq!(s.skipped_refreshes, 0);
        // The dashboard's rolling numbers come from the same count.
        assert_eq!(live.counts().repeated, 0);
    }
}
