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
/// kernel -> presented as measured on the reference setup (docs/testing.md
/// "The baseline": MS2130 1080p60, DP-1 at 59.97 Hz). Not a rule for every
/// card and monitor, but the user chose it as the dashboard's threshold
/// (#76); frame generation adds a refresh (`FRAMEGEN_EXTRA_MS`).
pub const BASELINE_KERNEL_MEDIAN_MS: f64 = 44.9;
pub const BASELINE_KERNEL_P99_MS: f64 = 53.7;
/// What frame generation adds by design: the generated frame takes the
/// duplicate's refresh, so the real one shows a refresh later.
pub const FRAMEGEN_EXTRA_MS: f64 = 16.7;

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
    /// The GPU the run measures on (gpustat::describe), the summary's
    /// first line (#87).
    machine: Option<String>,
    start: Duration,
    to_present: Vec<Duration>,
    kernel_to_present: Vec<Duration>,
    acquire: Vec<Duration>,
    /// GPU time of the convert and the scale pass, where measured, and of
    /// a frame generator's passes in the frames that ran one, each with
    /// when it was read (#82: when a pass ran slow).
    gpu_times: Vec<GpuSample>,
    /// The frame-generation mode the measured frame was submitted in: the
    /// one readback on its way (gputime.rs) is that frame's, and it comes
    /// back a frame or more later, after a `g` switch maybe (#86).
    measured: Option<&'static str>,
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
    /// The run's stretches by frame-generation mode (#86: a dev build's
    /// `g` switches mid-run; `--stats` only).
    stretches: Vec<Stretch>,
    /// The frame-generation counts at the last present that ran it.
    last_counts: [u64; 4],
    /// When refreshes were repeated, and how many (`--stats` only): the
    /// bursts line up with the source's beat against the display (#86).
    repeats: Vec<(Duration, usize)>,
}

/// A stretch of the run in one frame-generation mode (None: off), from
/// `from` (the run's time), with the counts and repeated refreshes at its
/// start (#86).
struct Stretch {
    mode: Option<&'static str>,
    from: Duration,
    counts: [u64; 4],
    repeated: u64,
}

impl Stats {
    pub fn new(start: Duration, verbose: bool, live: Arc<Live>) -> Self {
        Self {
            verbose,
            live,
            machine: None,
            start,
            to_present: Vec::with_capacity(64 * 1024),
            kernel_to_present: Vec::with_capacity(64 * 1024),
            acquire: Vec::with_capacity(64 * 1024),
            gpu_times: Vec::with_capacity(64 * 1024),
            measured: None,
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
            stretches: Vec::new(),
            last_counts: [0; 4],
            repeats: Vec::new(),
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

    /// A measured frame was submitted (`GpuTimer::submitted`): its times,
    /// read back later, count in the mode it ran in.
    pub fn gpu_submitted(&mut self) {
        if self.verbose {
            self.measured = self.live.framegen_counts().map(|(m, _)| m);
        }
    }

    /// The measured frame's pass times, read back at `at` (the monotonic
    /// clock).
    pub fn gpu(&mut self, t: crate::gputime::Times, at: Duration) {
        if self.verbose {
            self.gpu_times.push(GpuSample { at: at.saturating_sub(self.start), times: t, mode: self.measured });
        }
    }

    /// Opens a new stretch when the frame-generation mode changed (#86).
    /// The counts do not move while it is off, so a stretch's end is the
    /// next one's start, or the run's last counts.
    fn track_mode(&mut self, at: Duration) {
        let now = self.live.framegen_counts();
        if let Some((_, counts)) = now {
            self.last_counts = counts;
        }
        let mode = now.map(|(m, _)| m);
        if self.stretches.last().map(|s| s.mode) != Some(mode) {
            self.stretches.push(Stretch {
                mode,
                from: at.saturating_sub(self.start),
                counts: self.last_counts,
                repeated: self.skipped_refreshes,
            });
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

    /// The GPU the run measures on, named first in the summary (#87).
    pub fn set_machine(&mut self, machine: String) {
        self.machine = Some(machine);
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
            self.track_mode(at);
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
            if self.verbose && skipped > 0 {
                self.repeats.push((at.saturating_sub(self.start), skipped as usize));
            }
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
        if let Some(m) = &self.machine {
            eprintln!("summary: GPU {m}");
        }
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
        let passes = gpu_passes(&self.gpu_times);
        let [convert, scale, generate] = &passes;
        let all = |p: &Pass| p.iter().map(|(_, d)| *d).collect::<Vec<Duration>>();
        let (mut gpu_convert, mut gpu_scale, mut gpu_generate) = (all(convert), all(scale), all(generate));
        if !gpu_scale.is_empty() {
            eprintln!("summary: GPU time ({} frames)  convert {}  | scale {}", gpu_scale.len(), fmt(&mut gpu_convert), fmt(&mut gpu_scale));
        }
        let switched = self.stretches.len() > 1;
        // A switching run's generators each get their own line (`mode_lines`).
        if !gpu_generate.is_empty() && !switched {
            eprintln!("summary: GPU time ({} frames)  frame generation {}", gpu_generate.len(), fmt(&mut gpu_generate));
        }
        for line in gpu_timing_lines(&passes, !switched) {
            eprintln!("{line}");
        }
        if switched {
            let end = self.last_present.map_or(Duration::ZERO, |p| p.saturating_sub(self.start));
            for line in mode_lines(&self.stretches, &self.gpu_times, end, self.last_counts, self.skipped_refreshes) {
                eprintln!("{line}");
            }
        } else if let Some(line) = framegen_line(&self.live) {
            eprintln!("{line}");
        }
        if let Some(line) = repeat_line(&self.repeats) {
            eprintln!("{line}");
        }
        if let Some(line) = gpu_share(&mut gpu_convert, &mut gpu_scale, &mut gpu_generate, self.refresh) {
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

/// One frame's GPU pass times and when they were read, from the run's start.
struct GpuSample {
    at: Duration,
    times: crate::gputime::Times,
    /// The frame-generation mode it ran in (#86); None: off.
    mode: Option<&'static str>,
}

/// One pass's samples: (when, how long), in the order they were read.
type Pass = Vec<(Duration, Duration)>;

/// The passes' samples: convert, scale, and frame generation (only the
/// frames that ran it).
fn gpu_passes(samples: &[GpuSample]) -> [Pass; 3] {
    let convert = samples.iter().map(|s| (s.at, s.times.convert)).collect();
    let scale = samples.iter().map(|s| (s.at, s.times.scale)).collect();
    let generate = samples.iter().filter_map(|s| s.times.generate.map(|g| (s.at, g))).collect();
    [convert, scale, generate]
}

/// The first seconds of a run (the window opening, a refresh switch,
/// resizes, GameMode registering) that the settled p99 leaves out (#82).
const SETTLING: Duration = Duration::from_secs(2);
/// A pass ran slow: over this many times its median (#82)...
const SLOW: f64 = 1.5;
/// ...and over its median by at least this much, so a 20 µs pass's timer
/// jitter is not a slow frame.
const SLOW_FLOOR: Duration = Duration::from_micros(20);
/// Slow samples closer together than this are one range.
const SLOW_JOIN: Duration = Duration::from_millis(500);
/// Ranges named per pass; the rest are counted.
const SLOW_RANGES: usize = 5;

/// When a pass ran slow (#82): the samples over `SLOW` times its median as
/// time ranges (from, to, how many), in the order they happened.
fn slow_ranges(pass: &Pass) -> Vec<(Duration, Duration, usize)> {
    let mut all: Vec<Duration> = pass.iter().map(|(_, d)| *d).collect();
    let Some((median_ms, _)) = percentiles(&mut all) else { return Vec::new() };
    let median = Duration::from_secs_f64(median_ms / 1e3);
    let limit = median.mul_f64(SLOW).max(median + SLOW_FLOOR);
    ranges(pass.iter().filter(|(_, d)| *d > limit).map(|(at, _)| (*at, 1)), SLOW_JOIN)
}

/// Events (when, how many) closer together than `join` as one range
/// (from, to, how many), in the order they happened.
fn ranges(events: impl Iterator<Item = (Duration, usize)>, join: Duration) -> Vec<(Duration, Duration, usize)> {
    let mut out: Vec<(Duration, Duration, usize)> = Vec::new();
    for (at, n) in events {
        match out.last_mut() {
            Some((_, to, total)) if at.saturating_sub(*to) <= join => {
                *to = at;
                *total += n;
            }
            _ => out.push((at, at, n)),
        }
    }
    out
}

/// Ranges as `--stats` names them: the first `cap`, the rest counted.
fn named_ranges(r: &[(Duration, Duration, usize)], cap: usize) -> String {
    let mut named: Vec<String> =
        r.iter().take(cap).map(|(from, to, n)| format!("{:.1}–{:.1} s ×{n}", from.as_secs_f64(), to.as_secs_f64())).collect();
    if r.len() > cap {
        let rest = &r[cap..];
        named.push(format!("+{} more in {} ranges", rest.iter().map(|x| x.2).sum::<usize>(), rest.len()));
    }
    named.join(", ")
}

/// `--stats`' lines on when the GPU ran slow (#82): each pass's p99 after
/// the first `SETTLING`, and the time ranges of its slow samples. A window
/// run's p99 is twice its median while a headless test's is not; these say
/// whether the slow frames are the start, a moment (a screenshot) or spread.
fn gpu_timing_lines(passes: &[Pass; 3], generation_ranges: bool) -> Vec<String> {
    let names = ["convert", "scale", "frame generation"];
    let mut settled: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    for (name, pass) in names.iter().zip(passes) {
        // Modes switched mid-run: one p99 or median across generators says
        // nothing; `mode_lines` gives each its own.
        if pass.is_empty() || (*name == "frame generation" && !generation_ranges) {
            continue;
        }
        let mut after: Vec<Duration> = pass.iter().filter(|(at, _)| *at >= SETTLING).map(|(_, d)| *d).collect();
        settled.push(match percentiles(&mut after) {
            Some((_, p99)) => format!("{name} {p99:.2} ms"),
            None => format!("{name} no samples"),
        });
        let ranges = slow_ranges(pass);
        if ranges.is_empty() {
            continue;
        }
        let total: usize = ranges.iter().map(|r| r.2).sum();
        lines.push(format!(
            "summary: slow GPU frames, {name} (over {SLOW}x its median): {total} of {}: {}",
            pass.len(),
            named_ranges(&ranges, SLOW_RANGES)
        ));
    }
    if !settled.is_empty() {
        lines.insert(0, format!("summary: GPU p99 after the first {} s  {}", SETTLING.as_secs(), settled.join("  | ")));
    }
    lines
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

/// Repeated refreshes closer together than this are one burst (#86).
const REPEAT_JOIN: Duration = Duration::from_secs(2);
/// Bursts named; the rest are counted.
const REPEAT_RANGES: usize = 10;

/// `--stats`' line on when refreshes were repeated (#86): bursts that line
/// up with the source's beat against the display (a 60.003 Hz card on a
/// 59.973 Hz display slips a frame every ~33 s) show as evenly spaced.
fn repeat_line(repeats: &[(Duration, usize)]) -> Option<String> {
    let r = ranges(repeats.iter().copied(), REPEAT_JOIN);
    (!r.is_empty()).then(|| format!("summary: repeated refreshes by time: {}", named_ranges(&r, REPEAT_RANGES)))
}

/// `--stats`' lines for a run whose frame-generation mode changed (#86,
/// `g`): per mode, in the order first used, its time, its
/// counts, its repeated refreshes, and its generator's GPU time and slow
/// ranges against its own median.
fn mode_lines(stretches: &[Stretch], gpu: &[GpuSample], end: Duration, last_counts: [u64; 4], last_repeated: u64) -> Vec<String> {
    let mut order: Vec<Option<&'static str>> = Vec::new();
    for s in stretches {
        if !order.contains(&s.mode) {
            order.push(s.mode);
        }
    }
    order
        .into_iter()
        .map(|mode| {
            let (mut secs, mut counts, mut repeated) = (0.0, [0u64; 4], 0u64);
            for (i, s) in stretches.iter().enumerate().filter(|(_, s)| s.mode == mode) {
                let (to, c, r) = stretches.get(i + 1).map_or((end, last_counts, last_repeated), |n| (n.from, n.counts, n.repeated));
                secs += to.saturating_sub(s.from).as_secs_f64();
                for k in 0..4 {
                    counts[k] += c[k].saturating_sub(s.counts[k]);
                }
                repeated += r.saturating_sub(s.repeated);
            }
            let name = mode.unwrap_or("off");
            let Some(_) = mode else {
                return format!("summary: frame generation off ({secs:.1} s): repeated refreshes {repeated}");
            };
            let [generated, missed, real, withheld] = counts;
            let pass: Pass = gpu.iter().filter(|g| g.mode == mode).filter_map(|g| g.times.generate.map(|d| (g.at, d))).collect();
            let mut times: Vec<Duration> = pass.iter().map(|(_, d)| *d).collect();
            let gpu_part = match percentiles(&mut times) {
                Some((m, p99)) => {
                    let slow = slow_ranges(&pass);
                    let slow = if slow.is_empty() { String::new() } else { format!(", slow {}", named_ranges(&slow, SLOW_RANGES)) };
                    format!(" | GPU median {m:.2} ms p99 {p99:.2} ms{slow}")
                }
                None => String::new(),
            };
            format!(
                "summary: frame generation {name} ({secs:.1} s): {generated} generated, {missed} missed, {real} shown real, {withheld} withheld | repeated refreshes {repeated}{gpu_part}"
            )
        })
        .collect()
}

/// The summary's frame-generation counts (#78): every new frame is
/// generated, missed or shown real; none while it does not run.
fn framegen_line(live: &Live) -> Option<String> {
    let (mode, [generated, missed, real, withheld]) = live.framegen_counts()?;
    Some(format!("summary: frame generation {mode}: {generated} generated, {missed} missed, {real} shown real, {withheld} withheld"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::Live;

    const REFRESH: Duration = Duration::from_micros(16_681);

    /// A run's GPU samples, one a refresh for `secs`, whose scale pass
    /// takes `slow` where `is_slow(at)` holds and 0.1 ms elsewhere.
    fn run(secs: u64, slow: Duration, is_slow: impl Fn(Duration) -> bool) -> Vec<GpuSample> {
        let n = (Duration::from_secs(secs).as_nanos() / REFRESH.as_nanos()) as u32;
        (0..n)
            .map(|i| {
                let at = REFRESH * i;
                let scale = if is_slow(at) { slow } else { Duration::from_micros(100) };
                GpuSample { at, times: crate::gputime::Times { convert: Duration::from_micros(30), scale, generate: None }, mode: None }
            })
            .collect()
    }

    #[test]
    fn a_slow_start_is_left_out_of_the_settled_p99_and_named() {
        // The first 2 s at twice the time, as a window's start can run (#82).
        let samples = run(30, Duration::from_micros(200), |at| at < Duration::from_secs(2));
        let passes = gpu_passes(&samples);
        let mut scale: Vec<Duration> = passes[1].iter().map(|(_, d)| *d).collect();
        let (_, p99) = percentiles(&mut scale).unwrap();
        assert!((p99 - 0.2).abs() < 1e-9, "the whole run's p99 shows the doubling: {p99}");
        let lines = gpu_timing_lines(&passes, true);
        assert_eq!(lines[0], "summary: GPU p99 after the first 2 s  convert 0.03 ms  | scale 0.10 ms", "{lines:?}");
        assert_eq!(lines[1], "summary: slow GPU frames, scale (over 1.5x its median): 120 of 1798: 0.0–2.0 s ×120", "{lines:?}");
        assert_eq!(lines.len(), 2, "the convert pass never ran slow: {lines:?}");
    }

    #[test]
    fn a_burst_mid_run_is_named_where_it_happened() {
        // The run script's screenshots at 10–11 s (#82).
        let burst = |at: Duration| at >= Duration::from_secs(10) && at < Duration::from_secs(11);
        let samples = run(30, Duration::from_micros(200), burst);
        let lines = gpu_timing_lines(&gpu_passes(&samples), true);
        assert_eq!(lines[1], "summary: slow GPU frames, scale (over 1.5x its median): 60 of 1798: 10.0–11.0 s ×60", "{lines:?}");
        // A 30 µs pass at 48 µs is 1.6x its median but 18 µs over it:
        // timer jitter, not slow (`SLOW_FLOOR`).
        let mut jitter = run(30, Duration::from_micros(100), |_| false);
        for s in jitter.iter_mut().filter(|s| burst(s.at)) {
            s.times.convert = Duration::from_micros(48);
        }
        assert_eq!(gpu_timing_lines(&gpu_passes(&jitter), true).len(), 1, "only the settled p99 line");
    }

    #[test]
    fn slow_ranges_past_the_cap_are_counted() {
        let every_two_s = |at: Duration| at.as_millis() % 2000 < 20;
        let samples = run(30, Duration::from_micros(200), every_two_s);
        let lines = gpu_timing_lines(&gpu_passes(&samples), true);
        // 15 bursts, 2 s apart: 5 named, the other 10 counted.
        assert_eq!(lines[1].matches(" s ×").count(), 5, "{lines:?}");
        assert!(lines[1].ends_with(" more in 10 ranges"), "{lines:?}");
    }

    fn secs(x: f64) -> Duration {
        Duration::from_secs_f64(x)
    }

    /// A generator sample at `at` taking `ms`, in `mode`.
    fn fg_sample(at: f64, ms: f64, mode: Option<&'static str>) -> GpuSample {
        let t = crate::gputime::Times {
            convert: Duration::from_micros(30),
            scale: Duration::from_micros(200),
            generate: mode.map(|_| secs(ms / 1e3)),
        };
        GpuSample { at: secs(at), times: t, mode }
    }

    #[test]
    fn a_run_that_switches_modes_is_counted_per_mode() {
        // off 0–10 s, blend 10–20 s, ffx 20–30 s, off 30–35 s, blend 35–40 s.
        let st = |mode, from: f64, counts: [u64; 4], repeated| Stretch { mode, from: secs(from), counts, repeated };
        let stretches = [
            st(None, 0.0, [0; 4], 0),
            st(Some("blend"), 10.0, [0; 4], 1),
            st(Some("ffx"), 20.0, [290, 2, 8, 0], 1),
            st(None, 30.0, [570, 12, 18, 0], 21),
            st(Some("blend"), 35.0, [570, 12, 18, 0], 21),
        ];
        let mut gpu = Vec::new();
        for i in 0..1200 {
            let at = 10.0 + i as f64 / 60.0;
            let mode = if at < 20.0 || at >= 35.0 {
                Some("blend")
            } else if at < 30.0 {
                Some("ffx")
            } else {
                None
            };
            // ffx at 3 ms, twice that from 25 to 26 s; blend at 0.02 ms.
            let ms = match mode {
                Some("ffx") if (25.0..26.0).contains(&at) => 6.0,
                Some("ffx") => 3.0,
                _ => 0.02,
            };
            gpu.push(fg_sample(at, ms, mode));
        }
        let lines = mode_lines(&stretches, &gpu, secs(40.0), [710, 13, 20, 0], 23);
        assert_eq!(
            lines,
            [
                "summary: frame generation off (15.0 s): repeated refreshes 1",
                "summary: frame generation blend (15.0 s): 430 generated, 3 missed, 10 shown real, 0 withheld | repeated refreshes 2 | GPU median 0.02 ms p99 0.02 ms",
                "summary: frame generation ffx (10.0 s): 280 generated, 10 missed, 10 shown real, 0 withheld | repeated refreshes 20 | GPU median 3.00 ms p99 6.00 ms, slow 25.0–26.0 s ×60",
            ],
            "{lines:#?}"
        );
    }

    #[test]
    fn a_switch_opens_a_stretch_with_the_counts_so_far() {
        use crate::duplicate::Kind;
        use crate::framegen::Show;
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, true, live.clone());
        present(&mut s, secs(1.0));
        live.framegen("ffx");
        live.framegen_step(Show::Generated, false, Kind::New);
        present(&mut s, secs(2.0));
        live.framegen_step(Show::Generated, false, Kind::New);
        present(&mut s, secs(3.0));
        live.framegen_off();
        present(&mut s, secs(4.0));
        let modes: Vec<_> = s.stretches.iter().map(|x| (x.mode, x.from, x.counts[0])).collect();
        assert_eq!(modes, [(None, secs(1.0), 0), (Some("ffx"), secs(2.0), 1), (None, secs(4.0), 2)]);
    }

    #[test]
    fn a_sample_read_back_after_a_switch_counts_in_the_mode_it_ran_in() {
        let live = Arc::new(Live::new());
        let mut s = Stats::new(Duration::ZERO, true, live.clone());
        let t =
            |ms: f64| crate::gputime::Times { convert: Duration::from_micros(30), scale: Duration::ZERO, generate: Some(secs(ms / 1e3)) };
        // rife submitted, read back after `g` went on to blend.
        live.framegen("rife");
        s.gpu_submitted();
        live.framegen("blend");
        s.gpu(t(11.0), secs(1.0));
        // blend's own frame.
        s.gpu_submitted();
        s.gpu(t(0.02), secs(1.1));
        // blend submitted, read back after frame generation went off.
        s.gpu_submitted();
        live.framegen_off();
        s.gpu(t(0.02), secs(1.2));
        let modes: Vec<_> = s.gpu_times.iter().map(|g| g.mode).collect();
        assert_eq!(modes, [Some("rife"), Some("blend"), Some("blend")]);
    }

    #[test]
    fn repeated_refreshes_are_named_where_they_happened() {
        assert_eq!(repeat_line(&[]), None);
        let r = [(secs(10.0), 2), (secs(10.5), 3), (secs(43.3), 1)];
        assert_eq!(repeat_line(&r).as_deref(), Some("summary: repeated refreshes by time: 10.0–10.5 s ×5, 43.3–43.3 s ×1"));
    }

    #[test]
    fn a_switching_run_leaves_the_mixed_generation_ranges_and_p99_out() {
        // Two thirds blend: one median across both makes every ffx frame "slow".
        let gpu: Vec<GpuSample> = (0..600)
            .map(|i| fg_sample(i as f64 / 60.0, if i < 400 { 0.02 } else { 3.0 }, Some(if i < 400 { "blend" } else { "ffx" })))
            .collect();
        let passes = gpu_passes(&gpu);
        assert!(gpu_timing_lines(&passes, true).iter().any(|l| l.contains("slow GPU frames, frame generation")));
        // Nor its p99: the settled line names convert and scale only.
        assert_eq!(gpu_timing_lines(&passes, false), ["summary: GPU p99 after the first 2 s  convert 0.03 ms  | scale 0.20 ms"]);
    }

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
    fn the_summary_counts_frame_generation_only_when_it_runs() {
        use crate::duplicate::Kind;
        use crate::framegen::Show;
        let live = Live::new();
        assert_eq!(framegen_line(&live), None);
        live.framegen("ffx");
        live.framegen_step(Show::Generated, false, Kind::New);
        live.framegen_step(Show::Held, true, Kind::New);
        live.framegen_step(Show::Real, false, Kind::New);
        live.framegen_step(Show::Held, false, Kind::Duplicate);
        assert_eq!(framegen_line(&live).as_deref(), Some("summary: frame generation ffx: 1 generated, 1 missed, 1 shown real, 0 withheld"));
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
