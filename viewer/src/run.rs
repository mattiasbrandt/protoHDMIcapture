//! The window run (#22): one window over one source, from its start to why
//! it ended. A source and a plan go in, an `Outcome` comes out.
//!
//! - **The run owns the lifecycle:** the capture thread and its join, the
//!   run's own end (`End`), the `--seconds` timer, listening for signals
//!   while it runs, and the capture ending. None of them outlives the run,
//!   so nothing of one run can end a later one.
//! - **The window is behind the host seam (`Host`):** winit's (`app::Winit`:
//!   rendering, keys, the overlay, the screenshot before exit), or a script
//!   in tests. The run's threads reach it only through its `Waker`.
//! - **Callers decide what an outcome means:** the viewer ends on any of
//!   them; the setup wizard routes on them.
//!
//! The resources around the window (the audio, the dashboard, the session
//! files) are the feed owner's (feed.rs), not the run's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::capture::{self, Event, Finder, Format, FrameSource, Reason, Signal};
use crate::dashboard::Live;
use crate::present::Tag;

/// Why a window run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The window closed: q, the close button, or the setup wizard moving
    /// on.
    Closed,
    /// Ctrl-C, SIGTERM or SIGHUP (`Quit`).
    Signal,
    /// `--seconds` ran out.
    TimeUp,
    /// The capture ended on its own (the card gone): its error.
    CaptureEnded(String),
    /// The window could not go on: why.
    Failed(String),
}

/// What the run's threads tell the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// A frame is waiting in the slot.
    Frame,
    /// The frame policy's signal state changed (capture/policy.rs).
    Signal(Signal),
    /// The run may have ended: the window checks its `End`.
    End,
}

/// How the run's threads reach the window, from any thread.
pub type Waker = Arc<dyn Fn(Wake) + Send + Sync>;

/// A run's end: the first outcome set is the run's; the window is woken
/// for it. Each run has its own, so an earlier run's end, however late,
/// cannot end a later run.
#[derive(Clone)]
pub struct End {
    outcome: Arc<Mutex<Option<Outcome>>>,
    wake: Waker,
}

impl End {
    pub fn new(wake: Waker) -> Self {
        Self { outcome: Arc::default(), wake }
    }

    /// Ends the run with `outcome`, unless it has ended already: false then.
    pub fn set(&self, outcome: Outcome) -> bool {
        {
            let mut o = self.outcome.lock().expect("the end's lock is never poisoned");
            if o.is_some() {
                return false;
            }
            *o = Some(outcome);
        }
        (self.wake)(Wake::End);
        true
    }

    /// `set`, saying so when the run had already ended otherwise: an error
    /// found after the end is reported, not lost.
    pub fn set_or_warn(&self, outcome: Outcome) {
        let said = match &outcome {
            Outcome::CaptureEnded(e) => Some(format!("capture: {e}")),
            Outcome::Failed(e) => Some(e.clone()),
            _ => None,
        };
        if !self.set(outcome)
            && let Some(e) = said
        {
            eprintln!("warning: {e}");
        }
    }

    pub fn is_set(&self) -> bool {
        self.outcome.lock().expect("the end's lock is never poisoned").is_some()
    }

    fn take(&self) -> Option<Outcome> {
        self.outcome.lock().expect("the end's lock is never poisoned").take()
    }
}

/// The newest captured frame, waiting for the window; one replaced before
/// the window took it is skipped (not a capture gap).
pub struct Slot {
    pub data: Vec<u8>,
    pub tag: Option<Tag>,
    pub superseded: u64,
}

/// The host seam: the window side of a run.
pub trait Host {
    /// Ready for a window: how the run's threads will reach it.
    fn open(&mut self) -> Result<Waker, String>;

    /// The window, until `end` is set. The window sets it itself when it
    /// closes (`Closed`) or cannot go on (`Failed`); the run's threads set
    /// the rest and wake it with `Wake::End`. Frames wait in `slot`.
    fn show(&mut self, format: Format, slot: &Arc<Mutex<Slot>>, end: &End);
}

/// What the run needs beyond its source and window.
pub struct Plan {
    /// `--seconds`: the run ends `TimeUp` after this.
    pub seconds: Option<Duration>,
    /// This run's dashboard numbers (#21).
    pub live: Arc<Live>,
    /// Signal changes on stderr (the wizard's page says them instead).
    pub log_signal: bool,
    /// `--stats`: the capture's summary at the end.
    pub stats: bool,
    /// Finds the card again when it is pulled out (#40): the run waits for
    /// it instead of ending `CaptureEnded`. None: the source failing ends
    /// the run (the setup wizard's feed, which says so on its page).
    pub find: Option<Finder>,
}

/// One window over `source`, until it ends; why it did.
pub fn run(host: &mut dyn Host, mut source: Box<dyn FrameSource>, plan: Plan, quitter: &Quit) -> Outcome {
    // A signal before the run: no window, no capture.
    if quitter.requested() {
        return Outcome::Signal;
    }
    let wake = match host.open() {
        Ok(w) => w,
        Err(e) => return Outcome::Failed(e),
    };
    let end = End::new(wake.clone());
    // Signals stay process-wide and sticky; this run listens while it runs.
    {
        let end = end.clone();
        quitter.on_request(Box::new(move || {
            end.set(Outcome::Signal);
        }));
    }
    // One that came just before the listener did.
    if quitter.requested() {
        end.set(Outcome::Signal);
    }
    // The run's own timer: it ends this run only, and goes with it.
    let timer = plan.seconds.map(|after| {
        let (cancel, cancelled) = mpsc::channel::<()>();
        let end = end.clone();
        let thread = thread::spawn(move || {
            if cancelled.recv_timeout(after) == Err(RecvTimeoutError::Timeout) {
                end.set(Outcome::TimeUp);
            }
        });
        (cancel, thread)
    });

    let format = source.format();
    let slot = Arc::new(Mutex::new(Slot { data: vec![0; format.frame_size()], tag: None, superseded: 0 }));
    // Stops the capture: set once the window has ended.
    let stop = Arc::new(AtomicBool::new(false));
    let capture_thread = {
        let (slot, stop, end, live) = (slot.clone(), stop.clone(), end.clone(), plan.live.clone());
        let log_signal = plan.log_signal;
        let find = plan.find;
        thread::spawn(move || -> (u64, u64) {
            let mut frames = 0u64;
            let mut gaps = 0u64;
            let mut on_event = |e: Event<'_>| match e {
                Event::Frame(f) => {
                    frames += 1;
                    live.frame();
                    {
                        let mut s = slot.lock().expect("the slot's lock is never poisoned");
                        if s.tag.is_some() {
                            s.superseded += 1;
                        }
                        s.data.copy_from_slice(f.data);
                        s.tag = Some(Tag { captured: f.captured, dequeued: f.dequeued, repaint: false });
                    }
                    wake(Wake::Frame);
                }
                Event::Gap { from, to, lost } => {
                    gaps += 1;
                    live.gap(from, to, lost);
                    eprintln!("capture gap: sequence {from} -> {to}, {lost} lost");
                }
                Event::Signal(s) => {
                    live.signal(&s);
                    if log_signal {
                        match &s {
                            Signal::NoSignal { reason: Reason::Unplugged(why), .. } => {
                                eprintln!("capture: the card is gone ({why}); waiting for it to come back")
                            }
                            s => eprintln!("{s}"),
                        }
                    }
                    wake(Wake::Signal(s));
                }
                Event::CardBack => {
                    live.event("the card is back".into());
                    if log_signal {
                        eprintln!("the card is back");
                    }
                }
            };
            let result = match find {
                Some(mut find) => capture::run_finding(source, &mut find, &capture::Monotonic, &stop, &live.dropped, &mut on_event),
                None => capture::run(&mut *source, &capture::Monotonic, &stop, &live.dropped, &mut on_event),
            };
            // The run ends with its capture, unless it ended first.
            match result {
                Err(e) => end.set_or_warn(Outcome::CaptureEnded(e.to_string())),
                Ok(()) if !stop.load(Ordering::Relaxed) => {
                    end.set(Outcome::CaptureEnded("the capture stopped".into()));
                }
                Ok(()) => {}
            }
            (frames, gaps)
        })
    };

    host.show(format, &slot, &end);

    stop.store(true, Ordering::Relaxed);
    let captured = capture_thread.join();
    if let Some((cancel, thread)) = timer {
        drop(cancel);
        if thread.join().is_err() {
            end.set_or_warn(Outcome::Failed("the --seconds timer panicked".into()));
        }
    }
    quitter.stop_listening();
    match captured {
        Ok((frames, gaps)) if plan.stats => eprintln!("summary: captured {frames} frames, {gaps} capture gaps"),
        Ok(_) => {}
        Err(_) => end.set_or_warn(Outcome::Failed("the capture thread panicked".into())),
    }
    end.take().unwrap_or_else(|| Outcome::Failed("the window ended without saying why".into()))
}

/// Ctrl-C, SIGTERM and SIGHUP (ctrlc's termination feature), installed by
/// main before anything is switched, so a signal at any point ends in the
/// normal teardown (the display switched back, the run dir cleaned) rather
/// than leaving it to the next run's recovery. Sticky: once requested, every
/// later run ends `Signal` at once.
#[derive(Clone)]
pub struct Quit {
    flag: Arc<AtomicBool>,
    /// The running window run's listener, while one runs.
    listener: Arc<Mutex<Option<Listener>>>,
}

type Listener = Box<dyn Fn() + Send>;

impl Quit {
    pub fn install() -> Result<Self, String> {
        let q = Self::new();
        let handler = q.clone();
        ctrlc::set_handler(move || handler.request()).map_err(|e| format!("signal handler: {e}"))?;
        Ok(q)
    }

    /// No handler: requested only by `request` (tests).
    fn new() -> Self {
        Self { flag: Arc::default(), listener: Arc::default() }
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::Relaxed);
        if let Some(listener) = self.listener.lock().expect("the quit lock is never poisoned").as_ref() {
            listener();
        }
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn on_request(&self, listener: Listener) {
        *self.listener.lock().expect("the quit lock is never poisoned") = Some(listener);
    }

    fn stop_listening(&self) {
        *self.listener.lock().expect("the quit lock is never poisoned") = None;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::mpsc::{Receiver, Sender};
    use std::time::Instant;

    use super::*;
    use crate::capture::{Mark, Pixels, Raw, Stream};

    const FORMAT: Format = Format { width: 4, height: 2, fps: 60.0, pixels: Pixels::Yuyv };
    /// Longer than any test should wait: a hang fails instead.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// A card: `frames` frames, a millisecond apart, then gone (`fail`) or
    /// quiet.
    struct Card {
        frames: u32,
        fail: bool,
    }

    impl FrameSource for Card {
        fn format(&self) -> Format {
            FORMAT
        }

        fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
            Ok(Box::new(CardStream { left: self.frames, fail: self.fail, seq: 0, data: vec![16; FORMAT.frame_size()] }))
        }
    }

    struct CardStream {
        left: u32,
        fail: bool,
        seq: u32,
        data: Vec<u8>,
    }

    impl Stream for CardStream {
        fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
            if self.left == 0 {
                if self.fail {
                    return Err(io::Error::other("the card is gone"));
                }
                thread::sleep(wait);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(1));
            self.left -= 1;
            self.seq += 1;
            let t = capture::now_monotonic();
            take(Raw { data: &self.data, captured: t, dequeued: t, mark: Mark::Sequence(self.seq), flagged: false });
            Ok(())
        }
    }

    /// What a scripted window does in one run.
    enum Act {
        /// Closes at once, without looking at what woke it.
        Close,
        /// Closes after this many frames.
        CloseAfter(u32),
        /// Closes after this long.
        CloseIn(Duration),
        /// Cannot go on.
        Fail,
        /// Waits for the run to end it.
        Wait,
    }

    /// A window on a script, one act per run. Its wakes go through one
    /// queue kept across runs, as winit's one event loop is: a wake an
    /// earlier run left behind reaches the next run's window.
    struct Scripted {
        acts: VecDeque<Act>,
        queue: (Sender<Wake>, Receiver<Wake>),
        shown: u32,
        frames: u32,
    }

    impl Scripted {
        fn new(acts: impl IntoIterator<Item = Act>) -> Self {
            Self { acts: acts.into_iter().collect(), queue: mpsc::channel(), shown: 0, frames: 0 }
        }
    }

    impl Host for Scripted {
        fn open(&mut self) -> Result<Waker, String> {
            let tx = Mutex::new(self.queue.0.clone());
            Ok(Arc::new(move |w| {
                let _ = tx.lock().unwrap().send(w);
            }))
        }

        fn show(&mut self, _: Format, _: &Arc<Mutex<Slot>>, end: &End) {
            self.shown += 1;
            self.frames = 0;
            let act = self.acts.pop_front().expect("an act for each run");
            let started = Instant::now();
            let close_at = match act {
                Act::Close => {
                    end.set(Outcome::Closed);
                    return;
                }
                Act::Fail => {
                    end.set(Outcome::Failed("no GPU".into()));
                    return;
                }
                Act::CloseIn(d) => Some(started + d),
                Act::CloseAfter(_) | Act::Wait => None,
            };
            loop {
                let wait = close_at.map_or(PATIENCE, |t| t.saturating_duration_since(Instant::now()));
                match self.queue.1.recv_timeout(wait) {
                    Ok(Wake::Frame) => {
                        self.frames += 1;
                        if let Act::CloseAfter(n) = act
                            && self.frames >= n
                        {
                            end.set(Outcome::Closed);
                            return;
                        }
                    }
                    Ok(Wake::Signal(_)) => {}
                    // Only this run's end ends this window.
                    Ok(Wake::End) if end.is_set() => return,
                    Ok(Wake::End) => {}
                    Err(_) if close_at.is_some() => {
                        end.set(Outcome::Closed);
                        return;
                    }
                    Err(e) => panic!("the window waited {PATIENCE:?} for an end: {e}"),
                }
            }
        }
    }

    fn plan(seconds: Option<Duration>) -> Plan {
        Plan { seconds, live: Arc::new(Live::new()), log_signal: false, stats: false, find: None }
    }

    /// `plan`, with a finder that finds nothing `misses` times, then a
    /// fresh card of `frames` frames.
    fn finding(seconds: Option<Duration>, misses: u32, frames: u32) -> Plan {
        let mut left = misses;
        let find: Finder = Box::new(move || {
            if left == 0 {
                return Ok(Some(card(frames, false)));
            }
            left -= 1;
            Ok(None)
        });
        Plan { find: Some(find), ..plan(seconds) }
    }

    fn card(frames: u32, fail: bool) -> Box<dyn FrameSource> {
        Box::new(Card { frames, fail })
    }

    #[test]
    fn each_outcome() {
        let quit = Quit::new();
        let mut host = Scripted::new([Act::CloseAfter(3), Act::Wait, Act::Fail, Act::Wait]);
        assert_eq!(run(&mut host, card(100, false), plan(None), &quit), Outcome::Closed);
        assert_eq!(host.frames, 3);
        assert_eq!(run(&mut host, card(2, true), plan(None), &quit), Outcome::CaptureEnded("the card is gone".into()));
        assert_eq!(run(&mut host, card(0, false), plan(None), &quit), Outcome::Failed("no GPU".into()));
        assert_eq!(run(&mut host, card(0, false), plan(Some(Duration::from_millis(50))), &quit), Outcome::TimeUp);
        let mut host = Scripted::new([Act::Wait]);
        let signal = {
            let quit = quit.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                quit.request();
            })
        };
        assert_eq!(run(&mut host, card(0, false), plan(None), &quit), Outcome::Signal);
        signal.join().unwrap();
    }

    #[test]
    fn with_a_finder_a_card_pulled_out_is_waited_for_not_the_end() {
        let quit = Quit::new();
        // Gone after 2 frames, found again at once: the window sees the
        // fresh card's frames and closes after 20 in all.
        let mut host = Scripted::new([Act::CloseAfter(20)]);
        let p = finding(None, 0, 100);
        let live = p.live.clone();
        assert_eq!(run(&mut host, card(2, true), p, &quit), Outcome::Closed);
        assert_eq!(host.frames, 20);
        assert!(live.counts().frames >= 20);
        // Never found: --seconds and a signal still end the run as before.
        let mut host = Scripted::new([Act::Wait, Act::Wait]);
        assert_eq!(run(&mut host, card(2, true), finding(Some(Duration::from_millis(300)), u32::MAX, 0), &quit), Outcome::TimeUp);
        let signal = {
            let quit = quit.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                quit.request();
            })
        };
        assert_eq!(run(&mut host, card(2, true), finding(None, u32::MAX, 0), &quit), Outcome::Signal);
        signal.join().unwrap();
    }

    #[test]
    fn a_capture_that_ends_closes_only_its_own_window() {
        let quit = Quit::new();
        let mut host = Scripted::new([Act::Wait, Act::Close, Act::CloseAfter(5)]);
        assert_eq!(run(&mut host, card(1, true), plan(None), &quit), Outcome::CaptureEnded("the card is gone".into()));
        // This window closes at once, leaving its end's wake unread.
        assert_eq!(run(&mut host, card(0, false), plan(None), &quit), Outcome::Closed);
        // The next window gets that wake, and goes on to its own end.
        assert_eq!(run(&mut host, card(100, false), plan(None), &quit), Outcome::Closed);
        assert_eq!(host.frames, 5);
    }

    #[test]
    fn a_signal_before_the_run_ends_it_without_a_window() {
        let quit = Quit::new();
        quit.request();
        let mut host = Scripted::new([]);
        let started = Instant::now();
        assert_eq!(run(&mut host, card(100, false), plan(None), &quit), Outcome::Signal);
        assert_eq!(host.shown, 0);
        assert!(started.elapsed() < Duration::from_millis(100));
        // Sticky: the next run too.
        assert_eq!(run(&mut host, card(100, false), plan(None), &quit), Outcome::Signal);
    }

    #[test]
    fn the_seconds_cannot_end_a_later_window() {
        let quit = Quit::new();
        let mut host = Scripted::new([Act::Close, Act::CloseIn(Duration::from_millis(300))]);
        assert_eq!(run(&mut host, card(0, false), plan(Some(Duration::from_millis(100))), &quit), Outcome::Closed);
        assert_eq!(run(&mut host, card(0, false), plan(None), &quit), Outcome::Closed);
        assert!(!quit.requested(), "the timer is the run's, not a quit request");
    }

    #[test]
    fn the_first_end_is_the_runs() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let end = End::new(Arc::new(move |w| tx.lock().unwrap().send(w).unwrap()));
        assert!(end.set(Outcome::Closed));
        assert!(!end.set(Outcome::Signal));
        end.set_or_warn(Outcome::CaptureEnded("late".into()));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [Wake::End]);
        assert_eq!(end.take(), Some(Outcome::Closed));
    }
}
