//! The frame policy (#20): one capture loop above every adapter, on the
//! capture thread. An adapter only waits for its next buffer and lends it
//! with its times; the policy decides the rest, the same on every platform:
//!
//! - **A picture:** a buffer that turns into the window's YUYV frame. The
//!   size is checked, padded rows are packed, MJPEG is decoded. Any other
//!   buffer is dropped and counted by its reason (`Dropped`).
//! - **The signal state** (GLOSSARY "No signal"): no picture `NEVER_AFTER`
//!   after the feed opened, or `STOPPED_AFTER` after the last one, is no
//!   signal, with the reason; sent again when the reason changes. The
//!   overlay's banner, the wizard's signal check and the dashboard all read
//!   it.
//! - **The capture gaps,** from sequence numbers or sample times; none
//!   before the first picture (the card's start-up).
//! - **Quit,** seen within `POLL_SLICE`: no wait is longer.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use super::{Format, FrameSource, Mark, Pixels, Raw, now_monotonic};
use crate::mjpeg;

/// The longest wait for a buffer: the longest quit can take.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// No picture this long after the feed opened: no signal, "never".
pub const NEVER_AFTER: Duration = Duration::from_secs(5);
/// No picture this long after the last one: no signal, "stopped". Also how
/// recent a dropped buffer must be to be the reason.
pub const STOPPED_AFTER: Duration = Duration::from_secs(2);

/// A picture, valid only inside the callback.
pub struct Frame<'a> {
    /// One YUYV frame of the format's size.
    pub data: &'a [u8],
    pub captured: Duration,
    pub dequeued: Duration,
}

/// What the policy reports.
pub enum Event<'a> {
    /// A picture.
    Frame(Frame<'a>),
    /// The signal state changed, or its reason did.
    Signal(Signal),
    /// Buffers never reached us: `lost` of them between `from` and `to`.
    Gap { from: u32, to: u32, lost: u32 },
}

/// The signal state (GLOSSARY "No signal").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// Pictures arrive (or the feed opened under `NEVER_AFTER` ago).
    Picture,
    NoSignal { since: Since, reason: Reason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// No picture since the feed opened.
    Never,
    /// Pictures came, then stopped.
    Stopped,
}

/// Why there is no picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Nothing arrives.
    Nothing,
    /// MJPEG frames arrive but do not decode: the decoder's error.
    Undecodable(String),
    /// Buffers arrive that cannot be used (short, or flagged by the
    /// driver): what was wrong.
    Unusable(String),
}

impl Reason {
    /// The same kind of reason, whatever its text: a decoder that words
    /// each broken frame differently does not resend the state each frame.
    fn same_kind(&self, other: &Reason) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// The stderr line and the dashboard's event for a change.
impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Signal::NoSignal { since, reason } = self else { return f.write_str("signal back") };
        let since = match since {
            Since::Never => "never",
            Since::Stopped => "stopped",
        };
        match reason {
            Reason::Nothing => write!(f, "no signal ({since}): nothing arrives"),
            Reason::Undecodable(why) => write!(f, "no signal ({since}): MJPEG frames do not decode: {why}"),
            Reason::Unusable(why) => write!(f, "no signal ({since}): frames the viewer cannot use: {why}"),
        }
    }
}

/// Buffers dropped, by reason (the dashboard's counters).
#[derive(Debug, Default)]
pub struct Dropped {
    pub undecodable: AtomicU64,
    pub unusable: AtomicU64,
}

/// The time the policy keeps: `now_monotonic`, or a script in tests.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// `now_monotonic`, the clock the adapters stamp buffers on.
pub struct Monotonic;

impl Clock for Monotonic {
    fn now(&self) -> Duration {
        now_monotonic()
    }
}

/// Runs `source` until `quit` is set, returning within `POLL_SLICE` of it.
/// An error means the source is unusable (the card is gone).
pub fn run(
    source: &mut dyn FrameSource,
    clock: &dyn Clock,
    quit: &AtomicBool,
    dropped: &Dropped,
    on_event: &mut dyn FnMut(Event<'_>),
) -> io::Result<()> {
    let mut policy = Policy::new(source.format(), source.stride(), clock.now());
    let mut stream = source.start()?;
    while !quit.load(Ordering::Relaxed) {
        stream.next(POLL_SLICE, &mut |raw| policy.buffer(raw, dropped, on_event))?;
        policy.tick(clock.now(), on_event);
    }
    Ok(())
}

/// Lost frames from sample times (100 ns units), where a source has times
/// but no sequence numbers (Media Foundation; #34, simulated first in
/// tools/sim/time_gaps.py as `Banded`). Each sample gets a frame number on
/// a line through the recent ones:
///
/// - **The line:** frame number against time; its intercept by least
///   squares over the last `WINDOW` samples, its slope (the card's period)
///   from the turning of the times' phase. A lost frame moves the later
///   times by a whole period and so leaves their phase alone, which keeps
///   the slope free of the frame numbers counted; a least-squares slope,
///   fed by them, settled into the MS2130's short steps once losses were
///   frequent. Until `MIN_BLOCKS` phase centres, the least-squares slope.
/// - **The band:** a card's times keep within a band narrower than a
///   period around the line (the MS2130's wander, 23 short steps and one
///   long: 0.88 of one). A sample is the next frame unless its time lies
///   past the middle of the gap above the band, a frame further for each
///   period past it.
/// - **The phase** is taken in periods of the slope as it stood at each
///   block's start: in FPS's, a card 0.5 % off smears the MS2130's band
///   over the whole turn within one of its cycles.
/// - **The warm-up:** until `WARM` samples, a step counts only what wander
///   cannot be; then a frame is added at each long step where one closes
///   the warm-up's band (`relabel`), and those frames are said then.
/// - **The slope stays within `SLOPE` of FPS's period:** the MS2130's times
///   also fit a line 4 % off FPS, each long step a frame lost; a mode is
///   opened only within 0.5 % of FPS (mf.rs).
///
/// A loss is said at the sample after it (in the warm-up, up to `WARM`
/// samples late). Simulated: no made-up loss and every run right on #34's
/// matrix (flat, jittered and wandering times, the ticket's and the
/// MS2130's as traced, drift -0.5 to +0.5 %; seeds 7, 11 and 13), and so
/// here (`the_matrix_makes_up_no_loss_and_counts_every_run`); on the
/// MS2130's real 10 minute trace, 1344 where its cadence says 1340.3 +-
/// 1.4 (`the_real_trace_counts_its_cadences_losses`).
struct TimeGaps {
    period: f64,
    /// The first sample's time: the others are taken from it, in f64.
    t0: i64,
    /// The last sample's frame number and time; None before the first.
    last: Option<(i64, f64)>,
    /// (frame number, time) of the last `WINDOW` samples.
    pts: VecDeque<(i64, f64)>,
    /// The point the least-squares sums are taken from, for precision.
    origin: (i64, f64),
    /// n, k, t, kk, kt over `pts`, from `origin`.
    sums: [f64; 5],
    /// The phase's block in progress: cos, sin, samples, time sum.
    block: (f64, f64, u32, f64),
    /// (mean time, unwrapped centre) of the last `PHASE_BLOCKS` blocks.
    centres: VecDeque<(f64, f64)>,
    /// The centres' turning a time unit, once `MIN_BLOCKS` are in.
    turn: Option<f64>,
    /// The period the block in progress takes its phases in.
    frame: f64,
    /// Scratch for the band.
    band: Vec<f64>,
}

impl TimeGaps {
    /// Samples the intercept is fitted over: 10 s at 60 fps.
    const WINDOW: usize = 600;
    /// Samples the band is taken over: four of the MS2130's cycles.
    const BAND: usize = 100;
    /// Samples before the line decides: 5 s.
    const WARM: usize = 300;
    /// Periods a warm-up step must reach to count a loss.
    const WARM_STEP: f64 = 2.5;
    /// The most wander adds to a step, in periods (the MS2130's long one,
    /// 0.88): a warm-up step counts only past it.
    const WANDER: f64 = 0.9;
    /// The slope's furthest from FPS's period, a fraction.
    const SLOPE: f64 = 0.006;
    /// Samples a phase centre is taken over.
    const BLOCK: u32 = 50;
    /// Centres kept for the phase's turning: 100 s.
    const PHASE_BLOCKS: usize = 120;
    /// Centres before the turning is used: by the warm-up's end.
    const MIN_BLOCKS: usize = 5;
    /// Periods the warm-up's band must narrow by for its re-label to be taken.
    const TIGHTER: f64 = 0.05;

    fn new(fps: f64) -> Self {
        Self {
            period: 1e7 / fps.max(1.0),
            t0: 0,
            last: None,
            pts: VecDeque::with_capacity(Self::WINDOW + 1),
            origin: (0, 0.0),
            sums: [0.0; 5],
            block: (0.0, 0.0, 0, 0.0),
            centres: VecDeque::with_capacity(Self::PHASE_BLOCKS + 1),
            turn: None,
            frame: 1e7 / fps.max(1.0),
            band: Vec::with_capacity(Self::BAND),
        }
    }

    /// The frames lost before the sample at `now`.
    fn lost(&mut self, now: i64) -> u32 {
        let Some((last_k, last_t)) = self.last else {
            self.t0 = now;
            self.last = Some((0, 0.0));
            self.origin = (0, 0.0);
            self.add(0, 0.0);
            self.phase(0.0);
            return 0;
        };
        let t = (now - self.t0) as f64;
        let mut k = last_k + 1;
        if self.pts.len() < Self::WARM {
            let step = (t - last_t) / self.period;
            if step >= Self::WARM_STEP {
                k += (step - Self::WANDER).floor() as i64;
            }
        } else {
            let (a, b) = self.line();
            let at = |kk: i64, tt: f64| tt - self.origin.1 - (a + b * (kk - self.origin.0) as f64);
            let mut r = at(k, t);
            self.band.clear();
            let from = self.pts.len().saturating_sub(Self::BAND);
            self.band.extend(self.pts.iter().skip(from).map(|&(kk, tt)| at(kk, tt)));
            // The 5th to 95th percentile: one stray time (real cards have a
            // few) would otherwise widen the band to a period and close the gap.
            let trim = self.band.len() / 20;
            let lo = *self.band.select_nth_unstable_by(trim, f64::total_cmp).1;
            let top = self.band.len() - 1 - trim;
            let hi = *self.band.select_nth_unstable_by(top, f64::total_cmp).1;
            let edge = hi + (b - (hi - lo)) / 2.0;
            while r > edge {
                k += 1;
                r -= b;
            }
        }
        let mut lost = k - last_k - 1;
        self.last = Some((k, t));
        self.add(k, t);
        self.phase(t);
        if self.pts.len() == Self::WARM {
            lost += self.relabel();
        }
        lost as u32
    }

    fn sum(&mut self, k: i64, t: f64, sign: f64) {
        let (k, t) = ((k - self.origin.0) as f64, t - self.origin.1);
        let s = &mut self.sums;
        s[0] += sign;
        s[1] += sign * k;
        s[2] += sign * t;
        s[3] += sign * k * k;
        s[4] += sign * k * t;
    }

    fn rebase(&mut self) {
        self.origin = self.pts[0];
        self.sums = [0.0; 5];
        for i in 0..self.pts.len() {
            let (k, t) = self.pts[i];
            self.sum(k, t, 1.0);
        }
    }

    fn add(&mut self, k: i64, t: f64) {
        self.pts.push_back((k, t));
        self.sum(k, t, 1.0);
        if self.pts.len() > Self::WINDOW
            && let Some((k0, t0)) = self.pts.pop_front()
        {
            self.sum(k0, t0, -1.0);
        }
        if self.pts[0].0 - self.origin.0 > 4 * Self::WINDOW as i64 {
            self.rebase();
        }
    }

    /// This sample into the phase's blocks. A block's phases are taken in
    /// periods of the slope as it stood at the block's start (`frame`), not
    /// FPS's: off by 0.5 %, FPS's period smears the MS2130's 0.88-wide band
    /// over the whole turn within one cycle, and its circular mean then
    /// means nothing. Each centre is turned back into FPS's periods for
    /// keeping, and unwrapped against where the slope puts it.
    fn phase(&mut self, t: f64) {
        if self.block.2 == 0 {
            self.frame = if self.sums[0] >= 2.0 { self.line().1 } else { self.period };
        }
        let a = std::f64::consts::TAU * t / self.frame;
        self.block.0 += a.cos();
        self.block.1 += a.sin();
        self.block.2 += 1;
        self.block.3 += t;
        if self.block.2 < Self::BLOCK {
            return;
        }
        let tm = self.block.3 / Self::BLOCK as f64;
        // In the frame's periods, then FPS's: phi_p = phi_f - t (1/f - 1/p).
        let turn = 1.0 / self.period - 1.0 / self.frame;
        let mut c = self.block.1.atan2(self.block.0) / std::f64::consts::TAU + tm * turn;
        if let Some(&(lt, lc)) = self.centres.back() {
            let pred = lc + turn * (tm - lt);
            c = pred + ((c - pred + 0.5).rem_euclid(1.0) - 0.5);
        }
        self.centres.push_back((tm, c));
        if self.centres.len() > Self::PHASE_BLOCKS {
            self.centres.pop_front();
        }
        self.block = (0.0, 0.0, 0, 0.0);
        self.turn = self.turning();
    }

    /// The median of the slopes between centres half the kept ones apart
    /// (Theil-Sen at one lag): a stray centre cannot tilt it.
    fn turning(&self) -> Option<f64> {
        let n = self.centres.len();
        if n < Self::MIN_BLOCKS {
            return None;
        }
        let lag = n / 2;
        let mut slopes: Vec<f64> = (0..n - lag)
            .map(|j| {
                let (a, b) = (self.centres[j], self.centres[j + lag]);
                (b.1 - a.1) / (b.0 - a.0)
            })
            .collect();
        let mid = slopes.len() / 2;
        Some(*slopes.select_nth_unstable_by(mid, f64::total_cmp).1)
    }

    /// The slope, the phase's or else the least-squares, within `SLOPE`.
    fn slope(&self, ls: impl FnOnce() -> f64) -> f64 {
        let b = match self.turn {
            Some(turn) => 1.0 / (1.0 / self.period - turn),
            None => ls(),
        };
        b.clamp(self.period * (1.0 - Self::SLOPE), self.period * (1.0 + Self::SLOPE))
    }

    /// The line's time at `origin`'s frame number (from `origin`), and its slope.
    fn line(&self) -> (f64, f64) {
        let [n, sk, st, skk, skt] = self.sums;
        let b = self.slope(|| (n * skt - sk * st) / (n * skk - sk * sk));
        ((st - b * sk) / n, b)
    }

    /// The warm-up's frame numbers found again. A frame lost and missed
    /// splits the band a period apart; a frame added where it went closes
    /// it. Greedily: one frame more at the long step (1.5 periods or more:
    /// where a frame can have gone) that narrows the band most, again while
    /// one narrows it by `TIGHTER`. The band's width is the narrower under
    /// either slope, the phase's (a free one over a few seconds takes a
    /// frame lost in them for drift: a period in 300 samples is 0.33 %) or
    /// the least-squares (the phase's from a few blocks is shakiest at the
    /// widest drift). The frames beyond the steps' count, said now; none:
    /// the steps' numbers stay (said already, they cannot be taken back).
    fn relabel(&mut self) -> i64 {
        let (k0, t0) = self.pts[0];
        let ts: Vec<f64> = self.pts.iter().map(|&(_, t)| t - t0).collect();
        let old: Vec<i64> = self.pts.iter().map(|&(k, _)| k - k0).collect();
        let kinds: &[bool] = if self.turn.is_some() { &[true, false] } else { &[false] };
        let width_of = |ks: &[i64]| kinds.iter().map(|&phase| self.width(ks, &ts, phase)).fold(f64::INFINITY, f64::min);
        let longs: Vec<usize> = (1..ts.len()).filter(|&i| ts[i] - ts[i - 1] >= 1.5 * self.period).collect();
        let mut ks = old.clone();
        let mut best = width_of(&ks);
        // The first of the narrowest, as the simulator's min() takes it.
        while let Some((w, c)) = longs
            .iter()
            .map(|&i| ks.iter().enumerate().map(|(j, &k)| k + i64::from(j >= i)).collect::<Vec<i64>>())
            .map(|c| (width_of(&c), c))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            && w <= best - Self::TIGHTER * self.period
        {
            (best, ks) = (w, c);
        }
        let extra = ks[ks.len() - 1] - old[old.len() - 1];
        if extra <= 0 {
            return 0;
        }
        self.pts = ks.iter().zip(&ts).map(|(&k, &t)| (k + k0, t + t0)).collect();
        self.rebase();
        let (k, t) = self.pts[self.pts.len() - 1];
        self.last = Some((k, t));
        extra
    }

    /// A line through (ks, ts): the phase's slope or the least-squares one,
    /// within `SLOPE`; (time at frame 0, slope).
    fn fit(&self, ks: &[i64], ts: &[f64], phase: bool) -> (f64, f64) {
        let n = ks.len() as f64;
        let sk: f64 = ks.iter().map(|&k| k as f64).sum();
        let st: f64 = ts.iter().sum();
        let ls = || {
            let skk: f64 = ks.iter().map(|&k| (k * k) as f64).sum();
            let skt: f64 = ks.iter().zip(ts).map(|(&k, &t)| k as f64 * t).sum();
            (n * skt - sk * st) / (n * skk - sk * sk)
        };
        let b = match (phase, self.turn) {
            (true, Some(turn)) => 1.0 / (1.0 / self.period - turn),
            _ => ls(),
        };
        let b = b.clamp(self.period * (1.0 - Self::SLOPE), self.period * (1.0 + Self::SLOPE));
        ((st - b * sk) / n, b)
    }

    /// The full width of the places of `ts` against a line fitted to them
    /// with these frame numbers. Untrimmed, unlike the live band: a frame
    /// lost a few samples before the warm-up's end leaves fewer than a
    /// trim's worth past the split; and a frame added lowers every later
    /// place by a period, which no stray time can make look right.
    fn width(&self, ks: &[i64], ts: &[f64], phase: bool) -> f64 {
        let (a, b) = self.fit(ks, ts, phase);
        let places = ks.iter().zip(ts).map(|(&k, &t)| t - (a + b * k as f64));
        let (lo, hi) = places.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
        hi - lo
    }
}

struct Policy {
    format: Format,
    stride: usize,
    opened: Duration,
    last_picture: Option<Duration>,
    /// The latest buffer that was no picture: when, and why.
    last_drop: Option<(Duration, Reason)>,
    state: Signal,
    /// When the last buffer of any kind arrived.
    last_buffer: Option<Duration>,
    /// The last mark seen, and the count `Mark::Time` gaps are numbered by.
    last_mark: Option<Mark>,
    counted: u32,
    time_gaps: TimeGaps,
    decoder: Option<mjpeg::Decoder>,
    /// The frame decoded or packed into.
    out: Vec<u8>,
}

impl Policy {
    fn new(format: Format, stride: usize, opened: Duration) -> Self {
        let decoder = (format.pixels == Pixels::Mjpeg).then(|| mjpeg::Decoder::new(format.width, format.height));
        Self {
            format,
            stride,
            opened,
            last_picture: None,
            last_drop: None,
            state: Signal::Picture,
            last_buffer: None,
            last_mark: None,
            counted: 0,
            time_gaps: TimeGaps::new(format.fps),
            decoder,
            out: vec![0; format.frame_size()],
        }
    }

    fn buffer(&mut self, raw: Raw<'_>, dropped: &Dropped, on_event: &mut dyn FnMut(Event<'_>)) {
        // Nothing arrived for STOPPED_AFTER: the signal stopped (an HDMI
        // pull, the source asleep). The frames that never came then are no
        // capture losses (#20: a 26 s pull was said as 1553 lost), so the
        // counting starts afresh: no step across the silence, and a new
        // grid for sample times.
        if self.last_buffer.is_some_and(|t| raw.dequeued.saturating_sub(t) >= STOPPED_AFTER) {
            self.last_mark = None;
            self.time_gaps = TimeGaps::new(self.format.fps);
        }
        self.last_buffer = Some(raw.dequeued);
        if let Some(gap) = self.gap(raw.mark) {
            on_event(gap);
        }
        let (w, h) = (self.format.width as usize, self.format.height as usize);
        let size = self.format.frame_size();
        let picture: Result<&[u8], Reason> = if raw.flagged {
            Err(Reason::Unusable("flagged as bad by the driver".into()))
        } else if let Some(decoder) = &mut self.decoder {
            match decoder.decode(raw.data, &mut self.out) {
                Ok(()) => Ok(&self.out),
                Err(e) => Err(Reason::Undecodable(e)),
            }
        } else if self.stride == w * 2 && raw.data.len() == size {
            Ok(raw.data)
        } else if self.stride > w * 2 && raw.data.len() >= self.stride * (h - 1) + w * 2 {
            // Padded rows: packed tight, as the window takes them.
            for (row, out) in self.out.chunks_exact_mut(w * 2).enumerate() {
                out.copy_from_slice(&raw.data[row * self.stride..row * self.stride + w * 2]);
            }
            Ok(&self.out)
        } else {
            Err(Reason::Unusable(format!("{} bytes, not a {w}x{h} YUYV frame", raw.data.len())))
        };
        match picture {
            Ok(data) => {
                self.last_picture = Some(raw.dequeued);
                if self.state != Signal::Picture {
                    self.state = Signal::Picture;
                    on_event(Event::Signal(Signal::Picture));
                }
                on_event(Event::Frame(Frame { data, captured: raw.captured, dequeued: raw.dequeued }));
            }
            Err(reason) => {
                let n = match reason {
                    Reason::Undecodable(_) => &dropped.undecodable,
                    _ => &dropped.unusable,
                };
                n.fetch_add(1, Ordering::Relaxed);
                self.last_drop = Some((raw.dequeued, reason));
            }
        }
    }

    /// The gap before this buffer, if any; none before the first picture.
    fn gap(&mut self, mark: Mark) -> Option<Event<'static>> {
        let prev = self.last_mark.replace(mark);
        let gap = match (prev, mark) {
            (Some(Mark::Sequence(prev)), Mark::Sequence(seq)) => {
                let step = seq.wrapping_sub(prev);
                (step != 1).then(|| Event::Gap { from: prev, to: seq, lost: step.wrapping_sub(1) })
            }
            // The first time too: it sets the grid.
            (_, Mark::Time(t)) => {
                let lost = self.time_gaps.lost(t);
                let from = self.counted;
                self.counted = self.counted.wrapping_add(lost);
                (lost > 0).then(|| Event::Gap { from, to: from.wrapping_add(lost + 1), lost })
            }
            _ => None,
        };
        if let Mark::Time(_) = mark {
            self.counted = self.counted.wrapping_add(1);
        }
        gap.filter(|_| self.last_picture.is_some())
    }

    /// Time passing: the signal state, sent when it or its reason changes.
    fn tick(&mut self, now: Duration, on_event: &mut dyn FnMut(Event<'_>)) {
        let since = match self.last_picture {
            Some(t) if now.saturating_sub(t) < STOPPED_AFTER => return,
            Some(_) => Since::Stopped,
            None if now.saturating_sub(self.opened) < NEVER_AFTER => return,
            None => Since::Never,
        };
        let reason = match &self.last_drop {
            Some((t, r)) if now.saturating_sub(*t) < STOPPED_AFTER => r.clone(),
            _ => Reason::Nothing,
        };
        let changed = match &self.state {
            Signal::Picture => true,
            Signal::NoSignal { since: s, reason: r } => *s != since || !r.same_kind(&reason),
        };
        if changed {
            self.state = Signal::NoSignal { since, reason };
            on_event(Event::Signal(self.state.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::Stream;
    use super::*;

    /// 60 fps in ns.
    const P: u64 = 16_666_667;

    /// Scripted time, shared by the clock and the scripted adapter.
    #[derive(Clone, Default)]
    struct Time(Arc<AtomicU64>);

    impl Time {
        fn set(&self, t: Duration) {
            self.0.store(t.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    impl Clock for Time {
        fn now(&self) -> Duration {
            Duration::from_nanos(self.0.load(Ordering::Relaxed))
        }
    }

    /// A buffer the script hands over at `at`.
    struct Step {
        at: Duration,
        data: Vec<u8>,
        mark: Mark,
        flagged: bool,
    }

    /// An adapter on a script: each buffer at its time, nothing between,
    /// quit set at `end`, or the card gone there with `gone`.
    struct Scripted {
        format: Format,
        stride: usize,
        time: Time,
        steps: std::collections::VecDeque<Step>,
        end: Duration,
        gone: bool,
        quit: Arc<AtomicBool>,
    }

    impl FrameSource for Scripted {
        fn format(&self) -> Format {
            self.format
        }

        fn stride(&self) -> usize {
            self.stride
        }

        fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
            Ok(Box::new(ScriptedStream(self)))
        }
    }

    struct ScriptedStream<'a>(&'a mut Scripted);

    impl Stream for ScriptedStream<'_> {
        fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
            let s = &mut *self.0;
            let now = s.time.now();
            if let Some(b) = s.steps.pop_front_if(|b| b.at <= now + wait) {
                s.time.set(b.at.max(now));
                take(Raw { data: &b.data, captured: b.at, dequeued: b.at, mark: b.mark, flagged: b.flagged });
                return Ok(());
            }
            s.time.set(now + wait);
            if s.time.now() >= s.end {
                if s.gone {
                    return Err(io::Error::other("the card is gone"));
                }
                s.quit.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    const FORMAT: Format = Format { width: 4, height: 2, fps: 60.0, pixels: Pixels::Yuyv };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Good YUYV frames at 60 fps from `from` to before `to`, numbered on
    /// from `seq`.
    fn frames(from: Duration, to: Duration, seq: u32) -> Vec<Step> {
        let n = ((to - from).as_nanos() as u64).div_ceil(P);
        (0..n)
            .map(|i| Step {
                at: from + Duration::from_nanos(i * P),
                data: vec![16; FORMAT.frame_size()],
                mark: Mark::Sequence(seq + i as u32),
                flagged: false,
            })
            .collect()
    }

    /// What the policy said, each with the scripted time: frames as
    /// "frame", the rest as their Debug.
    #[derive(Debug, PartialEq)]
    enum Said {
        Frame(Vec<u8>),
        Signal(Signal),
        Gap(u32, u32, u32),
    }

    fn run_script(format: Format, stride: usize, steps: Vec<Step>, end: Duration, gone: bool) -> (Vec<(Duration, Said)>, Dropped, io::Result<()>) {
        let time = Time::default();
        let quit = Arc::new(AtomicBool::new(false));
        let mut source = Scripted { format, stride, time: time.clone(), steps: steps.into(), end, gone, quit: quit.clone() };
        let dropped = Dropped::default();
        let mut said = Vec::new();
        let result = run(&mut source, &time, &quit, &dropped, &mut |e| {
            let s = match e {
                Event::Frame(f) => Said::Frame(f.data.to_vec()),
                Event::Signal(s) => Said::Signal(s),
                Event::Gap { from, to, lost } => Said::Gap(from, to, lost),
            };
            said.push((time.now(), s));
        });
        (said, dropped, result)
    }

    fn signals(said: &[(Duration, Said)]) -> Vec<(Duration, Signal)> {
        said.iter()
            .filter_map(|(t, s)| match s {
                Said::Signal(s) => Some((*t, s.clone())),
                _ => None,
            })
            .collect()
    }

    fn no_signal(since: Since, reason: Reason) -> Signal {
        Signal::NoSignal { since, reason }
    }

    #[test]
    fn nothing_arriving_is_no_signal_never_at_5_s() {
        let (said, dropped, result) = run_script(FORMAT, 8, Vec::new(), ms(8000), false);
        result.unwrap();
        // Seen within one wait of the moment.
        assert_eq!(signals(&said), [(NEVER_AFTER, no_signal(Since::Never, Reason::Nothing))], "once, and not before 5 s");
        assert_eq!(dropped.unusable.load(Ordering::Relaxed) + dropped.undecodable.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pictures_that_stop_are_no_signal_stopped_2_s_on_and_come_back() {
        let mut steps = frames(ms(0), ms(1000), 0);
        let last = steps.last().unwrap().at;
        steps.extend(frames(ms(4000), ms(4100), 60));
        let (said, _, result) = run_script(FORMAT, 8, steps, ms(5000), false);
        result.unwrap();
        let s = signals(&said);
        assert_eq!(s.len(), 2, "{s:?}");
        let (at, stopped) = &s[0];
        assert_eq!(*stopped, no_signal(Since::Stopped, Reason::Nothing));
        assert!(*at >= last + STOPPED_AFTER && *at < last + STOPPED_AFTER + POLL_SLICE, "{at:?}");
        // Back: the state first, then the frame.
        let back = said.iter().position(|(_, x)| *x == Said::Signal(Signal::Picture)).unwrap();
        assert_eq!(said[back].0, ms(4000));
        assert!(matches!(said[back + 1].1, Said::Frame(_)));
        assert_eq!(said.iter().filter(|(_, x)| matches!(x, Said::Frame(_))).count(), 60 + 6);
        // The sequence went on from 59 to 60: no gap across the silence.
        assert!(!said.iter().any(|(_, x)| matches!(x, Said::Gap(..))));
    }

    #[test]
    fn gaps_from_sequence_numbers_none_before_the_first_picture() {
        let mut steps = frames(ms(0), ms(100), 10);
        // The card's start-up: a flagged buffer, then a jump, before any picture.
        steps[0].flagged = true;
        steps[1].mark = Mark::Sequence(20);
        for (i, s) in steps.iter_mut().enumerate().skip(2) {
            s.mark = Mark::Sequence(19 + i as u32);
        }
        // 22 -> 25 once pictures came: 2 lost.
        for s in steps.iter_mut().skip(4) {
            s.mark = match s.mark {
                Mark::Sequence(n) => Mark::Sequence(n + 2),
                m => m,
            };
        }
        let (said, dropped, result) = run_script(FORMAT, 8, steps, ms(200), false);
        result.unwrap();
        let gaps: Vec<&Said> = said.iter().map(|(_, s)| s).filter(|s| matches!(s, Said::Gap(..))).collect();
        assert_eq!(gaps, [&Said::Gap(22, 25, 2)]);
        assert_eq!(dropped.unusable.load(Ordering::Relaxed), 1, "the flagged one");
    }

    #[test]
    fn gaps_from_sample_times() {
        let p = 166_667; // 60 fps in 100 ns units
        let step = |i: u64, t: i64| Step { at: Duration::from_nanos(i * P), data: vec![16; FORMAT.frame_size()], mark: Mark::Time(t), flagged: false };
        let steps = vec![step(0, 0), step(1, p * 14 / 10), step(2, 2 * p), step(3, 5 * p), step(4, 6 * p)];
        let (said, _, result) = run_script(FORMAT, 8, steps, ms(200), false);
        result.unwrap();
        let gaps: Vec<&Said> = said.iter().map(|(_, s)| s).filter(|s| matches!(s, Said::Gap(..))).collect();
        // Jitter under 1.5 periods is no gap; 2p -> 5p skips 2.
        assert_eq!(gaps, [&Said::Gap(3, 6, 2)]);
    }

    /// The gaps said in `said`.
    fn gaps_in(said: &[(Duration, Said)]) -> Vec<&Said> {
        said.iter().map(|(_, s)| s).filter(|s| matches!(s, Said::Gap(..))).collect()
    }

    #[test]
    fn the_frames_that_never_came_while_the_signal_stopped_are_no_gap() {
        // #20: the HDMI pulled for 26 s, then back. Sample times (Windows):
        // the times go on 26 s later, on a grid of their own.
        let p: i64 = 166_667;
        let timed = |i: u64, at_ms: u64, t: i64| Step { at: ms(at_ms) + Duration::from_nanos(i * P), data: vec![16; FORMAT.frame_size()], mark: Mark::Time(t), flagged: false };
        let mut steps: Vec<Step> = (0..60).map(|i| timed(i, 0, i as i64 * p)).collect();
        let back = 27_000_u64;
        steps.extend((0..60).map(|i| timed(i, back, (back as i64 * 10_000) + 12_345 + i as i64 * p)));
        let (said, _, result) = run_script(FORMAT, 8, steps, ms(back + 1100), false);
        result.unwrap();
        assert_eq!(gaps_in(&said), Vec::<&Said>::new(), "no 1553 lost across the pull");
        // A real loss after it is still said: 3 frames out of the new grid.
        let mut steps: Vec<Step> = (0..60).map(|i| timed(i, 0, i as i64 * p)).collect();
        steps.extend((0..60).map(|i| timed(i, back, (back as i64 * 10_000) + if i < 30 { i as i64 * p } else { (i as i64 + 3) * p })));
        let (said, _, _) = run_script(FORMAT, 8, steps, ms(back + 1100), false);
        assert_eq!(gaps_in(&said).len(), 1, "{:?}", gaps_in(&said));
        assert!(matches!(gaps_in(&said)[0], Said::Gap(_, _, 3)), "{:?}", gaps_in(&said));
        // Sequence numbers (Linux) that jump across the silence: no gap
        // either; a jump without a silence still is one.
        let mut steps = frames(ms(0), ms(1000), 0);
        steps.extend(frames(ms(4000), ms(4100), 500));
        let (said, _, _) = run_script(FORMAT, 8, steps, ms(5000), false);
        assert_eq!(gaps_in(&said), Vec::<&Said>::new());
        let mut steps = frames(ms(0), ms(1000), 0);
        steps.extend(frames(ms(1000) + Duration::from_nanos(P), ms(1100), 70));
        let (said, _, _) = run_script(FORMAT, 8, steps, ms(1200), false);
        assert_eq!(gaps_in(&said), [&Said::Gap(59, 70, 10)]);
    }

    /// The lost frames `TimeGaps` finds in `times` (100 ns units) at 60 fps.
    fn lost_in(times: impl IntoIterator<Item = i64>) -> Vec<u32> {
        let mut g = TimeGaps::new(60.0);
        times.into_iter().map(|t| g.lost(t)).collect()
    }

    /// `n` times on the 60 fps grid, `losses` (index, frames) taken out:
    /// each moves the later times on that many periods.
    fn grid(n: usize, losses: &[(usize, i64)]) -> Vec<i64> {
        let mut times: Vec<i64> = (0..n as i64).map(|i| i * 166_667).collect();
        for &(at, frames) in losses {
            for t in &mut times[at..] {
                *t += frames * 166_667;
            }
        }
        times
    }

    /// Where the losses were said: (index, frames).
    fn said(lost: &[u32]) -> Vec<(usize, u32)> {
        lost.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| (i, *n)).collect()
    }

    #[test]
    fn lost_frames_from_sample_times() {
        // After the warm-up: at the sample after them.
        assert_eq!(said(&lost_in(grid(1000, &[]))), []);
        assert_eq!(said(&lost_in(grid(1000, &[(400, 1), (700, 4)]))), [(400, 1), (700, 4)]);
        // A late frame back on the grid is none.
        let mut late = grid(1000, &[]);
        late[500] += 166_667 * 4 / 10;
        assert_eq!(said(&lost_in(late)), []);
        // In the warm-up: what wander cannot be at once (a step of 4 periods
        // is 3 lost even with 0.9 of wander in it); a frame wander could
        // hide, at the warm-up's end (WARM samples), when its frame numbers
        // are found again.
        assert_eq!(said(&lost_in(grid(1000, &[(100, 3)]))), [(100, 3)]);
        assert_eq!(said(&lost_in(grid(1000, &[(100, 1)]))), [(TimeGaps::WARM - 1, 1)]);
    }

    /// The MS2130's wander on Windows: 23 steps of 16.028 ms, one of 31.285
    /// (the 10 minute trace's clean cycles, 60.0005 Hz), as 100 ns times.
    fn ms2130(n: usize, period_scale: f64) -> Vec<i64> {
        let mut t = 0.0;
        (0..n)
            .map(|i| {
                if i > 0 {
                    t += if i.is_multiple_of(24) { 312_849.0 } else { 160_276.5 } * period_scale;
                }
                t as i64
            })
            .collect()
    }

    #[test]
    fn times_that_wander_lose_nothing_and_a_lost_frame_still_counts() {
        assert_eq!(lost_in(ms2130(3000, 1.0)).iter().sum::<u32>(), 0, "no frame lost in 3000");
        // The same with frames gone mid-run, one at a wander step's start
        // and two in a short step: said where they went.
        let mut times = ms2130(3000, 1.0);
        for (at, frames) in [(1200, 1), (2005, 2)] {
            for t in &mut times[at..] {
                *t += frames * 166_665;
            }
        }
        assert_eq!(said(&lost_in(times)), [(1200, 1), (2005, 2)]);
        // A card 0.5 % off FPS, either way, loses nothing it did not.
        for scale in [0.995, 1.005] {
            assert_eq!(lost_in(ms2130(3000, scale)).iter().sum::<u32>(), 0, "{scale}");
        }
        // 59.94 Hz for ten minutes.
        assert_eq!(lost_in((0..36_000i64).map(|i| i * 166_833)).iter().sum::<u32>(), 0);
    }

    /// splitmix64, for the matrix's cards and losses.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn gauss(&mut self) -> f64 {
            let (u, v) = (self.unit().max(1e-12), self.unit());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// tools/sim/time_gaps.py's `card`: flat on the grid, jittered by 5 % of
    /// a period, the ticket's wander (16.03 ms 24 times, 31.4 ms once), or
    /// the MS2130's as traced (23 steps of 16.028 ms, one of 31.285);
    /// `drift` % slow (negative: fast).
    fn card(n: usize, drift: f64, shape: &str, rng: &mut Rng) -> Vec<f64> {
        let p = 1e7 / 60.0;
        let m = (24.0 * 160_300.0 + 314_000.0) / 25.0;
        let m24 = (23.0 * 160_276.5 + 312_849.0) / 24.0;
        let phase = rng.below(25);
        let mut t = rng.unit() * 1e6;
        let mut out = vec![t];
        for i in 1..n {
            let d = match shape {
                "saw" if (i + phase).is_multiple_of(25) => 314_000.0 / m * p,
                "saw" => 160_300.0 / m * p,
                "ms24" if (i + phase).is_multiple_of(24) => 312_849.0 / m24 * p,
                "ms24" => 160_276.5 / m24 * p,
                _ => p,
            };
            t += d * (1.0 + drift / 100.0);
            out.push(t);
        }
        if shape == "jit" {
            for x in &mut out {
                *x += rng.gauss() * p * 0.05;
            }
        }
        out
    }

    /// Each (index, frames) loses that many frames before times[index].
    fn drop_frames(times: &[f64], losses: &[(usize, u32)]) -> Vec<i64> {
        let mean = (times[times.len() - 1] - times[0]) / (times.len() - 1) as f64;
        let mut out = times.to_vec();
        for &(at, frames) in losses {
            for t in &mut out[at..] {
                *t += frames as f64 * mean;
            }
        }
        out.into_iter().map(|t| t as i64).collect()
    }

    /// #34's matrix: flat, jittered and wandering times (the ticket's model
    /// and the MS2130's as traced); drift -0.5 to +0.5 %; 1 to 4 losses of
    /// 1 to 3 frames, from in the warm-up on. The counter it replaced made
    /// up 435 + 291 losses on it and counted 256 of 648 + 168 of 216 runs
    /// wrong (the simulator's figures, its own seed).
    ///
    /// In a debug build (tools/check's `cargo test`) the counter is ~30
    /// times slower: only the first of each three cards is run, 288 runs,
    /// with every card and loss still drawn, so they are the release
    /// build's runs, the same numbers.
    #[test]
    fn the_matrix_makes_up_no_loss_and_counts_every_run() {
        let checked = if cfg!(debug_assertions) { 1 } else { 3 };
        let mut rng = Rng(7);
        let (mut phantom, mut wrong, mut runs) = (0, Vec::new(), 0);
        for shape in ["flat", "saw", "jit", "ms24"] {
            for drift in [-0.5, -0.3, -0.13, -0.05, 0.0, 0.05, 0.13, 0.3, 0.5] {
                for nth in 0..3 {
                    let base = card(2000, drift, shape, &mut rng);
                    if nth < checked {
                        phantom += lost_in(base.iter().map(|&t| t as i64)).iter().sum::<u32>();
                    }
                    for _ in 0..8 {
                        let mut at: Vec<usize> = (0..1 + rng.below(4)).map(|_| 200 + rng.below(1600)).collect();
                        at.sort_unstable();
                        at.dedup();
                        let losses: Vec<(usize, u32)> = at.into_iter().map(|i| (i, 1 + rng.below(3) as u32)).collect();
                        if nth >= checked {
                            continue;
                        }
                        let want: u32 = losses.iter().map(|l| l.1).sum();
                        let got: u32 = lost_in(drop_frames(&base, &losses)).iter().sum();
                        runs += 1;
                        if got != want {
                            wrong.push((shape, drift, losses, got));
                        }
                    }
                }
            }
        }
        assert_eq!(runs, 288 * checked);
        assert_eq!(phantom, 0, "losses made up");
        assert!(wrong.is_empty(), "{} of {runs} counted wrong: {wrong:?}", wrong.len());
    }

    /// The MS2130's 10 minute trace on the laptop (#34), idle, so with about
    /// two frames a second really lost: the frames its cadence says were
    /// sent less those received. Its 24-frame cycles with no step over 1.5
    /// periods but the closing long one give the card's period (60.0005 Hz),
    /// 1340.3 +- 1.4 lost at two sigma. The counter says 1344 (the
    /// simulator too): the few over are steps of 1.87-1.94 periods, where
    /// the wander's long step (1.877) and a frame lost in a short one (1.96)
    /// are 0.05-0.09 apart; a line of the cadence's own slope says 1342.
    #[test]
    fn the_real_trace_counts_its_cadences_losses() {
        const TRACE: &str = include_str!("../../../tools/fixtures/ms2130-windows/sample-times-600s.csv");
        let mut t = 0i64;
        let mut times = vec![0i64];
        for line in TRACE.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())) {
            t += line.split(',').next().and_then(|s| s.parse::<i64>().ok()).expect("a step");
            times.push(t);
        }
        assert_eq!(times.len(), 34_658);
        let p = 1e7 / 60.0;
        let steps: Vec<f64> = times.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
        let longs: Vec<usize> = (0..steps.len()).filter(|&i| steps[i] >= 1.7 * p).collect();
        let cycles: Vec<f64> = longs
            .windows(2)
            .filter(|w| w[1] - w[0] == 24 && steps[w[0] + 1..w[1]].iter().all(|&s| s < 1.5 * p) && steps[w[1]] < 2.2 * p)
            .map(|w| steps[w[0] + 1..=w[1]].iter().sum::<f64>() / 24.0)
            .collect();
        let period = cycles.iter().sum::<f64>() / cycles.len() as f64;
        let sent = (times[times.len() - 1] - times[0]) as f64 / period + 1.0;
        let truth = sent - times.len() as f64;
        assert!((truth - 1340.3).abs() < 0.5, "the cadence's count: {truth}");
        let counted: u32 = lost_in(times).iter().sum();
        assert!((counted as f64 - truth).abs() <= 5.0, "counted {counted}, the cadence says {truth:.1}");
    }

    #[test]
    fn mjpeg_that_does_not_decode_is_no_signal_with_the_decoders_error() {
        let format = Format { pixels: Pixels::Mjpeg, ..FORMAT };
        let mut steps = frames(ms(0), ms(6000), 0);
        for s in &mut steps {
            s.data = b"not a jpeg".to_vec();
        }
        let n = steps.len() as u64;
        let (said, dropped, result) = run_script(format, 0, steps, ms(6000), false);
        result.unwrap();
        let s = signals(&said);
        assert_eq!(s.len(), 1, "once, not once per broken frame: {s:?}");
        assert!(s[0].0 >= NEVER_AFTER && s[0].0 < NEVER_AFTER + POLL_SLICE, "{:?}", s[0].0);
        let Signal::NoSignal { since: Since::Never, reason: Reason::Undecodable(why) } = &s[0].1 else { panic!("{s:?}") };
        assert!(!why.is_empty());
        assert!(!said.iter().any(|(_, x)| matches!(x, Said::Frame(_))), "no picture");
        assert_eq!(dropped.undecodable.load(Ordering::Relaxed), n);
    }

    #[test]
    fn short_buffers_are_no_signal_unusable_and_the_reason_change_is_sent() {
        // Short, by a byte more or less: the same kind of reason in other
        // words is not sent again each frame.
        let mut steps = frames(ms(0), ms(6000), 0);
        for (i, s) in steps.iter_mut().enumerate() {
            s.data.truncate(10 + i % 2);
        }
        // Then nothing at all: the reason changes to nothing arriving.
        let (said, dropped, result) = run_script(FORMAT, 8, steps, ms(9000), false);
        result.unwrap();
        let s = signals(&said);
        assert_eq!(
            s.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>(),
            [
                no_signal(Since::Never, Reason::Unusable("10 bytes, not a 4x2 YUYV frame".into())),
                no_signal(Since::Never, Reason::Nothing),
            ]
        );
        let last = Duration::from_nanos(359 * P);
        assert!(s[1].0 >= last + STOPPED_AFTER && s[1].0 < last + STOPPED_AFTER + POLL_SLICE, "{:?}", s[1].0);
        assert_eq!(dropped.unusable.load(Ordering::Relaxed), 360);
        assert_eq!(s[0].1.to_string(), "no signal (never): frames the viewer cannot use: 10 bytes, not a 4x2 YUYV frame");
        assert_eq!(s[1].1.to_string(), "no signal (never): nothing arrives");
    }

    #[test]
    fn padded_rows_are_packed() {
        // 4x2 YUYV, rows 8 bytes, padded to 12.
        let mut data = Vec::new();
        for row in 0..2u8 {
            data.extend((0..8).map(|i| row * 10 + i));
            data.extend([0xEE; 4]);
        }
        let steps = vec![Step { at: ms(0), data, mark: Mark::Sequence(0), flagged: false }];
        let (said, _, result) = run_script(FORMAT, 12, steps, ms(100), false);
        result.unwrap();
        assert_eq!(said[0].1, Said::Frame(vec![0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 13, 14, 15, 16, 17]));
    }

    #[test]
    fn the_card_gone_ends_the_run_with_its_error() {
        let (_, _, result) = run_script(FORMAT, 8, frames(ms(0), ms(100), 0), ms(300), true);
        assert_eq!(result.unwrap_err().to_string(), "the card is gone");
    }
}
