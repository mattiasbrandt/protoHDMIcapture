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
//! - **The card pulled out** (#40), with a `Finder`: no signal, the card
//!   unplugged, while it is looked for about once a second. It is opened
//!   only once it is back whole and settled, with a backoff between
//!   opens; opened, the feed goes on from the fresh source.

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
/// How often a card pulled out is looked for (#40): a look only lists the
/// devices; it never opens the card.
const FIND_EVERY: Duration = Duration::from_secs(1);
/// How long the card must be back whole before it is opened (#40): a card
/// opened while it was still coming back most likely got stuck.
const SETTLE: Duration = Duration::from_secs(2);
/// The longest wait between two opens of a card back whole that cannot be
/// opened (#40): 1 s, 2 s, 4 s, then this.
const OPEN_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Finds the card again once it is gone (#40), on the capture thread.
pub trait Finder: Send {
    /// Whether the card is plugged in whole: its video node by its pins
    /// and, where it has one, its audio function. Quiet: it only looks.
    fn whole(&mut self) -> bool;
    /// Opens it at the same format as before; Err: why it cannot be.
    fn open(&mut self) -> Result<Box<dyn FrameSource>, String>;
}

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
    /// The card pulled out is found again and opened (#40); pictures
    /// follow.
    CardBack,
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
    /// The card is unplugged, waited for (#40): the source's error.
    Unplugged(String),
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
            Reason::Unplugged(why) => write!(f, "no signal ({since}): the card is unplugged ({why})"),
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

    /// Waits `d` with no source to wait on (a card pulled out, #40).
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
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
    Policy::new(source.format(), source.stride(), clock.now()).feed(source, clock, quit, dropped, on_event)
}

/// `run`, but a source that fails is the card pulled out (#40): no signal,
/// the card unplugged, while `find` looks for it (`look_for`); opened
/// again, the feed goes on from the fresh source. It ends only at `quit`.
pub fn run_finding(
    mut source: Box<dyn FrameSource>,
    find: &mut dyn Finder,
    clock: &dyn Clock,
    quit: &AtomicBool,
    dropped: &Dropped,
    on_event: &mut dyn FnMut(Event<'_>),
) -> io::Result<()> {
    let mut policy = Policy::new(source.format(), source.stride(), clock.now());
    loop {
        let gone = match policy.feed(&mut *source, clock, quit, dropped, on_event) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        // The old source let go before the card is looked for: a card that
        // is still there (a passing error) is opened again, not twice.
        drop(source);
        policy.unplugged(gone.to_string(), on_event);
        match look_for(find, clock, quit) {
            Some(found) => {
                source = found;
                policy.reopened(source.stride(), clock.now());
                on_event(Event::CardBack);
            }
            None => return Ok(()),
        }
    }
}

/// The card opened again, or None at `quit` (#40). Every `FIND_EVERY` it
/// looks whether the card is back whole; once it has been whole for
/// `SETTLE`, it opens it. An open that fails waits 1 s, 2 s, 4 s, then
/// `OPEN_BACKOFF_MAX` before the next, and is said once for each new
/// reason; a card seen gone again starts over (the settling and 1 s).
fn look_for(find: &mut dyn Finder, clock: &dyn Clock, quit: &AtomicBool) -> Option<Box<dyn FrameSource>> {
    let mut said: Option<String> = None;
    let mut next_look = clock.now();
    let mut whole_since: Option<Duration> = None;
    let mut next_open = Duration::ZERO;
    let mut backoff = FIND_EVERY;
    while !quit.load(Ordering::Relaxed) {
        let now = clock.now();
        if now >= next_look {
            next_look = now + FIND_EVERY;
            if find.whole() {
                whole_since.get_or_insert(now);
            } else {
                whole_since = None;
                backoff = FIND_EVERY;
                next_open = Duration::ZERO;
            }
        }
        if let Some(since) = whole_since
            && now.saturating_sub(since) >= SETTLE
            && now >= next_open
        {
            match find.open() {
                Ok(source) => return Some(source),
                Err(why) => {
                    // Its first line: a device list after it may vary.
                    let first = why.lines().next().unwrap_or_default().to_string();
                    if said.as_ref() != Some(&first) {
                        eprintln!("warning: the card cannot be opened yet: {why}; still waiting for it");
                        said = Some(first);
                    }
                    next_open = clock.now() + backoff;
                    backoff = (backoff * 2).min(OPEN_BACKOFF_MAX);
                }
            }
        }
        clock.sleep(POLL_SLICE);
    }
    None
}

/// How much of the carried lateness leaks away each frame: about two
/// seconds at 60 fps, so a card a little off FPS (the MS2130 runs at
/// 60.08 Hz by the PC's clock) never adds up to a frame. Only a little:
/// from about 0.42 % off, the leak's steady lateness hides a lost frame on
/// a fast card and makes them up on a slow one, and times that wander as
/// the MS2130's do on a card 0.05 % slow make some up (simulated, #19's
/// review). Hence `DRIFT_WARN`, and mf.rs's 0.4 % match.
const LATE_LEAK: f64 = 1.0 / 120.0;

/// A card's measured rate, lost frames counted in, this far off FPS (a
/// fraction) is said once: its lost frames, counted from times, may be
/// wrong. It sees a fast card only: on a slow one the made-up losses fill
/// the rate back to FPS, as real ones would, so the two cannot be told
/// apart by rate.
const DRIFT_WARN: f64 = 0.003;
/// Frames to measure the rate over before saying it: 10 s at 60 fps.
const DRIFT_FRAMES: u64 = 600;

/// Lost frames from sample times (100 ns units), where a source has times
/// but no sequence numbers (Media Foundation). Each step's lateness against
/// the period is carried to the next, so a frame late by one period at
/// once is a lost frame, and times that wander are not: the MS2130's come
/// 0.64 ms early 24 times, then 31.4 ms after the last (#19), which a step
/// alone reads as a lost frame every 25.
#[derive(Default)]
struct TimeGaps {
    first: Option<i64>,
    prev: Option<i64>,
    /// Frames since the first, the lost ones counted in.
    frames: u64,
    /// The rate has been measured (`drift`).
    measured: bool,
    /// How late this frame is, in 100 ns units, on the grid the earlier
    /// frames set (negative: early).
    late: f64,
}

impl TimeGaps {
    /// The frames lost before the sample at `now`.
    fn lost(&mut self, now: i64, fps: f64) -> u32 {
        let period = 1e7 / fps.max(1.0);
        let Some(prev) = self.prev.replace(now) else {
            self.first = Some(now);
            return 0;
        };
        self.late = self.late * (1.0 - LATE_LEAK) + ((now - prev) as f64 - period);
        let lost = (self.late / period).round().max(0.0);
        self.late -= lost * period;
        self.frames += 1 + lost as u64;
        lost as u32
    }

    /// Once, after `DRIFT_FRAMES`: a warning when the card's rate by its
    /// times is over `DRIFT_WARN` off FPS.
    fn drift(&mut self, now: i64, fps: f64) -> Option<String> {
        if self.measured || self.frames < DRIFT_FRAMES {
            return None;
        }
        self.measured = true;
        let elapsed = (now - self.first?) as f64 / 1e7;
        let rate = self.frames as f64 / elapsed;
        let off = rate / fps - 1.0;
        (off.abs() > DRIFT_WARN).then(|| {
            format!(
                "warning: the card's frames come at {rate:.3} fps, {:+.2} % off FPS={fps}; the lost frames counted from their times may be wrong",
                off * 100.0
            )
        })
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
    /// The last mark seen, and the count `Mark::Time` gaps are numbered by.
    last_mark: Option<Mark>,
    counted: u32,
    time_gaps: TimeGaps,
    decoder: Option<mjpeg::Decoder>,
    /// The frame decoded or packed into.
    out: Vec<u8>,
    /// The card found again at this time (#40), no picture since: the
    /// unplugged state stands until a picture, or `NEVER_AFTER`.
    reopened: Option<Duration>,
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
            last_mark: None,
            counted: 0,
            time_gaps: TimeGaps::default(),
            decoder,
            out: vec![0; format.frame_size()],
            reopened: None,
        }
    }

    /// `source`'s stream until `quit` (Ok) or the source fails (Err).
    fn feed(
        &mut self,
        source: &mut dyn FrameSource,
        clock: &dyn Clock,
        quit: &AtomicBool,
        dropped: &Dropped,
        on_event: &mut dyn FnMut(Event<'_>),
    ) -> io::Result<()> {
        let mut stream = source.start()?;
        while !quit.load(Ordering::Relaxed) {
            stream.next(POLL_SLICE, &mut |raw| self.buffer(raw, dropped, on_event))?;
            self.tick(clock.now(), on_event);
        }
        Ok(())
    }

    /// The source failed: the card is unplugged (#40), said once.
    fn unplugged(&mut self, why: String, on_event: &mut dyn FnMut(Event<'_>)) {
        if matches!(self.state, Signal::NoSignal { reason: Reason::Unplugged(_), .. }) {
            return;
        }
        let since = if self.last_picture.is_some() { Since::Stopped } else { Since::Never };
        self.state = Signal::NoSignal { since, reason: Reason::Unplugged(why) };
        on_event(Event::Signal(self.state.clone()));
    }

    /// The card found again at `now` (#40): a fresh stream, whose marks
    /// start over (no gap across the time it was out).
    fn reopened(&mut self, stride: usize, now: Duration) {
        self.stride = stride;
        self.last_mark = None;
        self.time_gaps = TimeGaps::default();
        self.last_drop = None;
        self.reopened = Some(now);
    }

    fn buffer(&mut self, raw: Raw<'_>, dropped: &Dropped, on_event: &mut dyn FnMut(Event<'_>)) {
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
                self.reopened = None;
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
                let lost = self.time_gaps.lost(t, self.format.fps);
                if let Some(w) = self.time_gaps.drift(t, self.format.fps) {
                    eprintln!("{w}");
                }
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
        // The card just found again: its first picture is on the way.
        if self.reopened.is_some_and(|t| now.saturating_sub(t) < NEVER_AFTER) {
            return;
        }
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

        fn sleep(&self, d: Duration) {
            self.set(self.now() + d);
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
        CardBack,
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
                Event::CardBack => Said::CardBack,
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

    /// The lost frames `TimeGaps` finds in `times` (100 ns units) at 60 fps.
    fn lost_in(times: impl IntoIterator<Item = i64>) -> Vec<u32> {
        let mut g = TimeGaps::default();
        times.into_iter().map(|t| g.lost(t, 60.0)).collect()
    }

    #[test]
    fn lost_frames_from_sample_times() {
        let p = 166_667;
        assert_eq!(lost_in([0, p, 2 * p]), [0, 0, 0]);
        assert_eq!(lost_in([0, p * 14 / 10, 2 * p]), [0, 0, 0], "a late frame back on the grid");
        assert_eq!(lost_in([0, 2 * p, 3 * p]), [0, 1, 0]);
        assert_eq!(lost_in([0, 5 * p, 6 * p]), [0, 4, 0]);
    }

    #[test]
    fn a_card_far_off_fps_is_said_once() {
        let at = |period: i64| {
            let mut g = TimeGaps::default();
            (0..2000i64).filter_map(|i| {
                let now = i * period;
                g.lost(now, 60.0);
                g.drift(now, 60.0)
            }).collect::<Vec<_>>()
        };
        assert!(at(166_667).is_empty(), "on time");
        assert!(at(166_833).is_empty(), "59.94 Hz: 0.1 % off");
        let fast = at(165_833);
        assert_eq!(fast.len(), 1, "0.5 % fast, said once: {fast:?}");
        assert!(fast[0].contains("60.302 fps, +0.50 % off FPS=60"), "{}", fast[0]);
        // A slow card's made-up losses fill its rate back: not seen (`DRIFT_WARN`).
        assert!(at(167_500).is_empty());
    }

    #[test]
    fn times_that_wander_lose_nothing_and_a_lost_frame_still_counts() {
        // The MS2130 on Windows (#19): 16.03 ms 24 times, then 31.4 ms.
        let mut t = 0i64;
        let mut times = vec![t];
        for i in 1..=25 * 40 {
            t += if i % 25 == 0 { 314_000 } else { 160_300 };
            times.push(t);
        }
        assert_eq!(lost_in(times.clone()).iter().sum::<u32>(), 0, "no frame lost in 1000");
        // The same with one frame gone mid-run: one more period.
        for x in &mut times[512..] {
            *x += 166_667;
        }
        assert_eq!(lost_in(times).iter().sum::<u32>(), 1);
        // A card a little off FPS never adds up to a lost frame: 59.94 Hz
        // for an hour.
        let slow = (0..216_000i64).map(|i| i * 166_833);
        assert_eq!(lost_in(slow).iter().sum::<u32>(), 0);
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

    /// A finder on a script (#40): the card whole within `whole` (time
    /// windows, end exclusive), its opens failing before `fails_until`,
    /// and quit set by a look at `quit_at`. An open that works gives
    /// pictures from 1 s after it (a card's start-up) until `end`. Each
    /// open's time is kept.
    struct CardScript {
        time: Time,
        quit: Arc<AtomicBool>,
        whole: Vec<(Duration, Duration)>,
        fails_until: Duration,
        end: Duration,
        quit_at: Option<Duration>,
        opens: Arc<std::sync::Mutex<Vec<Duration>>>,
    }

    impl Finder for CardScript {
        fn whole(&mut self) -> bool {
            let now = self.time.now();
            if self.quit_at.is_some_and(|t| now >= t) {
                self.quit.store(true, Ordering::Relaxed);
            }
            self.whole.iter().any(|&(from, to)| now >= from && now < to)
        }

        fn open(&mut self) -> Result<Box<dyn FrameSource>, String> {
            let now = self.time.now();
            self.opens.lock().unwrap().push(now);
            if now < self.fails_until {
                return Err("busy".into());
            }
            let steps = frames(now + ms(1000), self.end, 0).into();
            Ok(Box::new(Scripted { format: FORMAT, stride: 8, time: self.time.clone(), steps, end: self.end, gone: false, quit: self.quit.clone() }))
        }
    }

    const FOREVER: Duration = Duration::from_secs(3600);

    /// `run_finding`: pictures to 1 s, the card pulled out at 1.5 s, then
    /// `CardScript` with `whole`, `fails_until`, `end` and `quit_at`. The
    /// events, and the opens' times.
    fn run_pulled(whole: Vec<(Duration, Duration)>, fails_until: Duration, end: Duration, quit_at: Option<Duration>) -> (Vec<(Duration, Said)>, Vec<Duration>) {
        let time = Time::default();
        let quit = Arc::new(AtomicBool::new(false));
        let first = Scripted { format: FORMAT, stride: 8, time: time.clone(), steps: frames(ms(0), ms(1000), 100).into(), end: ms(1500), gone: true, quit: quit.clone() };
        let opens = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut find = CardScript { time: time.clone(), quit: quit.clone(), whole, fails_until, end, quit_at, opens: opens.clone() };
        let mut said = Vec::new();
        run_finding(Box::new(first), &mut find, &time, &quit, &Dropped::default(), &mut |e| {
            let s = match e {
                Event::Frame(f) => Said::Frame(f.data.to_vec()),
                Event::Signal(s) => Said::Signal(s),
                Event::Gap { from, to, lost } => Said::Gap(from, to, lost),
                Event::CardBack => Said::CardBack,
            };
            said.push((time.now(), s));
        })
        .unwrap();
        let opens = opens.lock().unwrap().clone();
        (said, opens)
    }

    /// `d` within one look and one wait of `want`.
    fn about(d: Duration, want: Duration) -> bool {
        d >= want && d <= want + FIND_EVERY + POLL_SLICE
    }

    #[test]
    fn a_card_pulled_out_is_opened_once_when_back_whole_and_settled() {
        let (said, opens) = run_pulled(vec![(ms(4000), FOREVER)], Duration::ZERO, ms(10_000), None);
        // Opened once, SETTLE after it was seen back whole, not before.
        assert_eq!(opens.len(), 1, "{opens:?}");
        assert!(about(opens[0], ms(4000) + ms(2000)), "2 s after it was whole: {opens:?}");
        // Unplugged once, at once, then the picture back; no "nothing
        // arrives" while the fresh card starts up.
        let s: Vec<Signal> = signals(&said).into_iter().map(|(_, s)| s).collect();
        assert_eq!(s, [no_signal(Since::Stopped, Reason::Unplugged("the card is gone".into())), Signal::Picture]);
        let back = said.iter().position(|(_, x)| *x == Said::CardBack).expect("the card back");
        assert_eq!(said[back].0, opens[0]);
        assert!(matches!(said[back + 1].1, Said::Signal(Signal::Picture)));
        // The fresh source's numbers start over: no gap across the pull.
        assert!(!said.iter().any(|(_, x)| matches!(x, Said::Gap(..))));
        assert!(said[back..].iter().filter(|(_, x)| matches!(x, Said::Frame(_))).count() > 60);
    }

    #[test]
    fn a_card_not_whole_or_whole_for_under_2_s_is_not_opened() {
        // Whole for about a second (its video back, its audio not yet, say),
        // then gone, then back whole for good.
        let (_, opens) = run_pulled(vec![(ms(2400), ms(3600)), (ms(6000), FOREVER)], Duration::ZERO, ms(12_000), None);
        assert_eq!(opens.len(), 1, "{opens:?}");
        assert!(about(opens[0], ms(6000) + ms(2000)), "{opens:?}");
    }

    #[test]
    fn opens_that_fail_back_off_1_2_4_8_8_s() {
        let (_, opens) = run_pulled(vec![(ms(2000), FOREVER)], FOREVER, FOREVER, Some(ms(30_000)));
        let gaps: Vec<Duration> = opens.windows(2).map(|w| w[1] - w[0]).collect();
        let want = [1, 2, 4, 8, 8].map(Duration::from_secs);
        assert!(gaps.len() >= want.len(), "{opens:?}");
        for (got, want) in gaps.iter().zip(want) {
            assert!(*got >= want && *got <= want + POLL_SLICE, "{gaps:?}");
        }
    }

    #[test]
    fn a_card_gone_and_back_again_starts_the_backoff_over() {
        // Back whole at 2 s, its opens failing; gone at 8 s; back at 12 s.
        let (said, opens) = run_pulled(vec![(ms(2000), ms(8000)), (ms(12_000), FOREVER)], ms(15_000), ms(20_000), None);
        let first_after = opens.iter().position(|&t| t >= ms(12_000)).expect("opens after the return");
        assert!(about(opens[first_after], ms(12_000) + ms(2000)), "settled again: {opens:?}");
        // Its next open 1 s on, not where the first round's backoff was.
        let gap = opens[first_after + 1] - opens[first_after];
        assert!(gap >= ms(1000) && gap <= ms(1000) + POLL_SLICE, "{opens:?}");
        assert_eq!(signals(&said).last().map(|(_, s)| s.clone()), Some(Signal::Picture));
    }

    #[test]
    fn quit_while_the_card_is_out_ends_the_run() {
        let (said, opens) = run_pulled(Vec::new(), Duration::ZERO, FOREVER, Some(ms(3000)));
        assert!(opens.is_empty());
        let last = said.last().unwrap().0;
        assert!(last < ms(3000), "nothing after quit: {last:?}");
    }
    #[test]
    fn the_unplugged_state_is_said_as_the_glossary_has_it() {
        let s = no_signal(Since::Stopped, Reason::Unplugged("The video recording device is no longer present.".into()));
        assert_eq!(s.to_string(), "no signal (stopped): the card is unplugged (The video recording device is no longer present.)");
    }
}
