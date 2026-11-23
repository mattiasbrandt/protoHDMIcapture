//! What the display run measures, against #5's go criteria: dequeue ->
//! presented (gating), kernel timestamp -> presented (reported), and how
//! often a refresh went by with no new frame on screen. Each present also
//! goes to the run's dashboard numbers (dashboard.rs).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::dashboard::Live;
use crate::present::Tag;

/// #5's go criteria for dequeue -> presented, ms: the median within one
/// 60 Hz refresh, p99 within two. The summary states them; the dashboard
/// colours the line against them (#76).
pub const GATE_MEDIAN_MS: f64 = 16.7;
pub const GATE_P99_MS: f64 = 33.3;

const WINDOW: Duration = Duration::from_secs(10);
/// This many presents in a row discarded, with as many frame callbacks
/// between them, and the presents are being replaced before display: the
/// latency cannot be read (#72: the openfg layer commits its generated
/// frame on our feedback, then the real one over it).
pub const REPLACED: u32 = 30;

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
    /// GPU time of the convert and the scale pass, where measured, and of
    /// a frame generator's passes in the frames that ran one.
    gpu_convert: Vec<Duration>,
    gpu_scale: Vec<Duration>,
    gpu_generate: Vec<Duration>,
    /// CPU time of drawing the overlay (layout, tessellation, upload), in
    /// the frames that drew one (#53: cache it if this is costly).
    overlay: Vec<Duration>,
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
    /// Refreshes from one present to the next, counted by how many
    /// (`--stats`): a 2:2 source in new-frames-only mode should be all 2s;
    /// 3s next to 1s are a present landing a refresh late, then on time.
    gaps: BTreeMap<u64, u64>,
    /// Presents discarded in a row, and the frame callbacks since the
    /// first of them (`REPLACED`).
    discard_streak: u32,
    done_in_streak: u32,
    /// GPU use while a GPU enhancement is on (gpustat.rs), for the 10 s
    /// lines.
    gpu: Option<Arc<crate::gpustat::Shared>>,
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
            gpu_generate: Vec::new(),
            gpu_scale: Vec::with_capacity(64 * 1024),
            overlay: Vec::with_capacity(64 * 1024),
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
            gaps: BTreeMap::new(),
            discard_streak: 0,
            done_in_streak: 0,
            gpu: None,
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

    pub fn gpu(&mut self, t: crate::gputime::Times) {
        if self.verbose {
            self.gpu_convert.push(t.convert);
            self.gpu_scale.push(t.scale);
            self.gpu_generate.extend(t.generate);
        }
    }

    pub fn overlay(&mut self, cpu: Duration) {
        if self.verbose {
            self.overlay.push(cpu);
        }
    }

    pub fn pace_timeout(&mut self) {
        self.pace_timeouts += 1;
    }

    #[cfg(test)]
    pub fn pace_timeouts(&self) -> u64 {
        self.pace_timeouts
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
        self.discard_streak = self.discard_streak.saturating_add(1);
        if self.discard_streak >= REPLACED && self.done_in_streak >= REPLACED {
            self.live.set_replaced(true);
        }
    }

    /// The compositor's frame callback: it is still taking our presents,
    /// so a run of discards with these is our presents being replaced, not
    /// a window the compositor hides.
    pub fn frame_done(&mut self) {
        if self.discard_streak > 0 {
            self.done_in_streak = self.done_in_streak.saturating_add(1);
        }
    }

    /// GPU use for the 10 s lines while a GPU enhancement is on, none
    /// while it is off.
    pub fn set_gpu(&mut self, gpu: Option<Arc<crate::gpustat::Shared>>) {
        self.gpu = gpu;
    }

    /// Presents counted as replaced before display (`REPLACED`).
    pub fn replaced(&self) -> bool {
        self.discard_streak >= REPLACED && self.done_in_streak >= REPLACED
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
        self.discard_streak = 0;
        self.done_in_streak = 0;
        self.live.set_replaced(false);
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
            // Duplicate frames withheld on purpose (new-frames-only) left
            // their refreshes to the frame before: not repeated.
            let refreshes = ((at - prev).as_secs_f64() / r.as_secs_f64()).round() as u64;
            skipped = refreshes.saturating_sub(1 + u64::from(tag.withheld));
            if self.verbose {
                *self.gaps.entry(refreshes).or_default() += 1;
            }
            self.skipped_refreshes += skipped;
        }
        self.last_present = Some(at);
        // A generated frame carries its new frame's times; the latency is
        // the real frame's, measured on its own present.
        if tag.generated {
            self.live.presented(None, skipped, self.refresh);
            if self.verbose {
                self.window.skipped_refreshes += skipped;
            }
            return;
        }
        // The dashboard's rolling numbers (#16), at every run.
        self.live.presented(Some((d, k)), skipped, self.refresh);
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
            let gpu = self.gpu.as_ref().map_or(String::new(), |g| format!("  | {}", crate::gpustat::window_line(g)));
            eprintln!(
                "{:6.1} s  dequeue->present {}  | kernel->present {}  | repeated refreshes {}{gpu}",
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
        if self.presented == 0 && self.replaced() {
            eprintln!(
                "summary: latency not measurable: all {} presents were replaced before display (a frame-generation layer?)",
                self.discarded
            );
        }
        eprintln!(
            "summary: dequeue->present {}  (gating: median <= {GATE_MEDIAN_MS}, p99 <= {GATE_P99_MS})",
            fmt_full(&mut self.to_present)
        );
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
        if !self.gpu_generate.is_empty() {
            eprintln!("summary: GPU time ({} frames)  frame generation {}", self.gpu_generate.len(), fmt(&mut self.gpu_generate));
        }
        if let Some(line) = gpu_share(&mut self.gpu_convert, &mut self.gpu_scale, &mut self.gpu_generate, self.refresh) {
            eprintln!("{line}");
        }
        if !self.overlay.is_empty() {
            eprintln!("summary: overlay CPU ({} frames)  {}", self.overlay.len(), fmt(&mut self.overlay));
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
        if !self.gaps.is_empty() {
            let gaps: Vec<String> = self.gaps.iter().map(|(r, n)| format!("{r}x{n}")).collect();
            eprintln!("summary: refreshes between presents (refreshes x presents) {}", gaps.join(" "));
        }
    }
}

/// protoHDMIcapture's GPU share of a refresh: the medians of its passes
/// (convert, scale, and the generator where one ran) summed, against the
/// refresh interval. Portable: the window's own timestamp queries.
fn gpu_share(convert: &mut [Duration], scale: &mut [Duration], generate: &mut [Duration], refresh: Option<Duration>) -> Option<String> {
    let r = refresh?;
    let med = |v: &mut [Duration]| percentiles(v).map_or(0.0, |(m, _)| m);
    if convert.is_empty() {
        return None;
    }
    let ms = med(convert) + med(scale) + med(generate);
    Some(format!(
        "summary: GPU share of a refresh {:.1} % ({ms:.2} ms of {:.2} ms, the passes' medians summed)",
        ms / (r.as_secs_f64() * 1e3) * 100.0,
        r.as_secs_f64() * 1e3
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::Live;

    const REFRESH: Duration = Duration::from_micros(16_681);

    #[test]
    fn the_overlays_cpu_time_is_kept_only_for_stats() {
        let mut quiet = Stats::new(Duration::ZERO, false, Arc::new(Live::new()));
        quiet.overlay(Duration::from_micros(800));
        assert!(quiet.overlay.is_empty());
        let mut verbose = Stats::new(Duration::ZERO, true, Arc::new(Live::new()));
        verbose.overlay(Duration::from_micros(800));
        verbose.overlay(Duration::from_micros(1200));
        assert_eq!(verbose.overlay.len(), 2);
    }

    fn present(s: &mut Stats, at: Duration) {
        let tag = Tag {
            captured: at - Duration::from_millis(30),
            dequeued: at - Duration::from_millis(20),
            repaint: false,
            kind: crate::duplicate::Kind::New,
            generated: false,
            withheld: 0,
        };
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

    /// A present of a frame-generation run (framegen.rs, #66).
    fn present_fg(s: &mut Stats, at: Duration, generated: bool, withheld: u32) {
        let tag = Tag {
            captured: at - Duration::from_millis(30),
            dequeued: at - Duration::from_millis(20),
            repaint: false,
            kind: crate::duplicate::Kind::New,
            generated,
            withheld,
        };
        s.presented(tag, at, REFRESH, 0x7);
    }

    #[test]
    fn withheld_duplicates_leave_no_repeated_refreshes() {
        // New-frames-only at 30 fps on a 60 Hz display: a present every
        // other refresh, each after one withheld duplicate.
        let mut s = Stats::new(Duration::ZERO, false, Arc::new(Live::new()));
        for i in 0..10u32 {
            present_fg(&mut s, Duration::from_secs(1) + REFRESH * (2 * i), false, 1);
        }
        assert_eq!(s.skipped_refreshes, 0);
        // A refresh more than the withheld duplicate explains is repeated.
        present_fg(&mut s, Duration::from_secs(1) + REFRESH * 21, false, 1);
        assert_eq!(s.skipped_refreshes, 1);
    }

    #[test]
    fn presents_replaced_before_display_make_the_latency_unmeasurable() {
        // The openfg layer commits its generated frame with our feedback
        // and the real one over it: every present discarded, the frame
        // callbacks still coming (#72).
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, false, live.clone());
        for _ in 0..REPLACED - 1 {
            s.discarded();
            s.frame_done();
        }
        assert!(!s.replaced(), "not yet: a short run of discards is a hidden window's");
        s.discarded();
        s.frame_done();
        s.discarded();
        assert!(s.replaced() && live.replaced_shown());
        // A present shown again: measurable.
        present(&mut s, Duration::from_secs(1));
        assert!(!s.replaced() && !live.replaced_shown());
    }

    #[test]
    fn discards_without_frame_callbacks_are_a_hidden_window_not_replaced_presents() {
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, false, live.clone());
        for _ in 0..REPLACED * 3 {
            s.discarded();
        }
        assert!(!s.replaced());
        assert!(!live.replaced_shown());
    }

    #[test]
    fn the_refreshes_between_presents_are_counted_by_length() {
        let mut s = Stats::new(Duration::ZERO, true, Arc::new(Live::new()));
        let t0 = Duration::from_secs(1);
        for at in [0, 2, 4, 7, 8, 10] {
            present_fg(&mut s, t0 + REFRESH * at, false, 1);
        }
        assert_eq!(s.gaps.iter().map(|(r, n)| (*r, *n)).collect::<Vec<_>>(), [(1, 1), (2, 3), (3, 1)]);
        // A present a refresh late, then one on time: one repeated refresh.
        assert_eq!(s.skipped_refreshes, 1);
    }

    #[test]
    fn the_gpu_share_is_the_passes_medians_against_a_refresh() {
        let ms = |v: f64| Duration::from_secs_f64(v / 1e3);
        let line = gpu_share(&mut [ms(0.03)], &mut [ms(0.2)], &mut [ms(0.44)], Some(REFRESH)).unwrap();
        assert!(line.contains("4.0 %"), "{line}");
        assert!(gpu_share(&mut [], &mut [], &mut [], Some(REFRESH)).is_none());
        assert!(gpu_share(&mut [ms(1.0)], &mut [ms(1.0)], &mut [], None).is_none());
    }

    #[test]
    fn a_generated_frame_counts_its_refresh_but_not_a_latency() {
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, true, live.clone());
        present_fg(&mut s, Duration::from_secs(1), true, 0);
        present_fg(&mut s, Duration::from_secs(1) + REFRESH, false, 0);
        present_fg(&mut s, Duration::from_secs(1) + REFRESH * 2, true, 0);
        assert_eq!(s.skipped_refreshes, 0, "generated and real alternate, a refresh apart");
        assert_eq!((s.presented, s.to_present.len(), s.kernel_to_present.len()), (3, 1, 1), "the real frame's latency only");
        assert_eq!(live.counts().presented, 3);
    }
}
