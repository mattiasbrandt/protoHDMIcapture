//! Frame pacing (#46): when the window draws and when it wakes next. A
//! card frame or a repaint goes out when the compositor has taken the last
//! one (its frame callback, `present::Outcome::FrameDone`), or once that
//! is `PACE_TIMEOUT` late; a held frame goes out fresh or overdue, and
//! waits for the next card frame in between. The presentation numbers
//! (stats.rs) are counted here too, as each step happens: submits, the
//! feedback, pace timeouts, and the repeated-refresh count started over
//! when the picture comes back.
//!
//! Every time is on the capture clock (`now_monotonic`, the one buffers
//! are stamped on), passed in; the window converts its own deadlines to
//! delays from now, and the wake back.

use std::time::Duration;

use crate::capture::Signal;
use crate::present::Outcome;
use crate::stats::Stats;

/// A frame callback this late counts as lost: pacing stops waiting for it.
pub const PACE_TIMEOUT: Duration = Duration::from_millis(100);
/// A held frame this fresh goes out on the compositor's frame callback
/// instead of waiting to be replaced: it came within the capture's jitter
/// of the callback. With none, every phase crossing of the capture and
/// display clocks (one each ~37 s at 60.00 against 59.97 Hz) dropped
/// frames in bursts, about 4.7 % of them; 5 ms leaves about one per two
/// minutes for 3.5 ms more latency (MS2130, 2026-10-16, decided with the
/// user).
pub const HOLD: Duration = Duration::from_millis(5);
/// How often the device is polled while a screenshot comes back.
pub const READBACK_POLL: Duration = Duration::from_millis(10);

/// The window's pacing and its presentation numbers.
pub struct Pacing {
    stats: Stats,
    /// FPS: a held frame older than one and a half frame times is overdue.
    fps: f64,
    /// A card frame came on this wake.
    arrived: bool,
    /// Draw again without a new frame (the overlay changed).
    repaint: bool,
    /// When what the window shows next changes on its own.
    deadline: Option<Duration>,
    /// Presents the compositor has not taken yet, and the last one's time.
    in_flight: u32,
    last_submit: Duration,
    /// The picture is gone (no signal): its return starts the repeat count
    /// over.
    no_signal: bool,
}

/// What the window holds as it picks its next wake.
#[derive(Debug, Clone, Copy, Default)]
pub struct Waiting {
    /// When what it shows next changes on its own, from now (the overlay's
    /// and the wizard's next change).
    pub page: Option<Duration>,
    /// A card frame held back by pacing.
    pub held: bool,
    /// The window has no area: nothing is drawn into it.
    pub hidden: bool,
    /// A screenshot on its way back, taken by polling the device.
    pub reading: bool,
}

impl Pacing {
    pub fn new(start: Duration, fps: f64, stats: Stats) -> Self {
        Self { stats, fps, arrived: false, repaint: false, deadline: None, in_flight: 0, last_submit: start, no_signal: false }
    }

    /// The numbers it counts, for what the window measures itself (the
    /// acquire wait, the GPU times) and `--stats`' summary.
    pub fn stats(&mut self) -> &mut Stats {
        &mut self.stats
    }

    /// A card frame came.
    pub fn arrived(&mut self) {
        self.arrived = true;
    }

    /// Something shown changed: draw again even without a card frame.
    pub fn repaint(&mut self) {
        self.repaint = true;
    }

    pub fn repaint_wanted(&self) -> bool {
        self.repaint
    }

    /// The frame policy's signal state: the picture back after no signal
    /// (an HDMI pull) starts the repeated-refresh count over (#40), as
    /// `Stats::picture_back` says.
    pub fn signal(&mut self, s: &Signal) {
        let gone = matches!(s, Signal::NoSignal { .. });
        if !gone && self.no_signal {
            self.stats.picture_back();
        }
        self.no_signal = gone;
        self.repaint = true;
    }

    /// The window woke: whether it sends now. A frame came on this wake,
    /// the overlay changed (or its deadline passed), or a held frame is
    /// fresh (`HOLD`) or overdue (held for longer than one and a half frame
    /// times: the next one is late). One in between waits for the next card
    /// frame to replace it. `held`: when the held frame was dequeued.
    pub fn wake(&mut self, now: Duration, held: Option<Duration>) -> bool {
        // Something shown changed on its own (a message went, the banner
        // came): draw again even without a frame.
        if self.deadline.is_some_and(|d| d <= now) {
            self.repaint = true;
        }
        let held = held.map(|dequeued| now.saturating_sub(dequeued));
        let overdue = Duration::from_secs_f64(1.5 / self.fps.max(1.0));
        let send = self.arrived || self.repaint || held.is_some_and(|age| age < HOLD || age > overdue);
        self.arrived = false;
        send
    }

    /// Whether a present may go out now: paced, only once the compositor
    /// has taken the previous one (its frame callback), or once that is
    /// `PACE_TIMEOUT` late, counted; never into a window with no area.
    /// `paced`: the feedback paces (Wayland's frame callback; DXGI's
    /// swapchain paces itself).
    pub fn may_present(&mut self, now: Duration, paced: bool, hidden: bool) -> bool {
        if paced && self.in_flight > 0 {
            if now - self.last_submit < PACE_TIMEOUT {
                return false;
            }
            self.stats.pace_timeout();
            self.in_flight = 0;
        }
        !hidden
    }

    /// A present went out: a new card frame, or a repaint of the last one.
    pub fn presented(&mut self, now: Duration, new_frame: bool) {
        self.in_flight += 1;
        self.last_submit = now;
        self.repaint = false;
        if new_frame {
            self.stats.submitted();
        }
    }

    /// The compositor's feedback on a present.
    pub fn feedback(&mut self, o: Outcome) {
        match o {
            Outcome::Presented { tag, at, refresh, flags } => self.stats.presented(tag, at, refresh, flags),
            Outcome::Discarded => self.stats.discarded(),
            Outcome::FrameDone => self.in_flight = self.in_flight.saturating_sub(1),
        }
    }

    /// When to wake next, from now: for the next change, and while a
    /// repaint or a held frame waits on pacing (no frame from the card may
    /// come to wake us; if the card stops now, nothing else would wake the
    /// loop for it); not while the window has no area, which nothing is
    /// drawn into. None: on the next event only.
    pub fn next_wake(&mut self, now: Duration, w: Waiting) -> Option<Duration> {
        self.deadline = w.page.map(|p| now + p);
        let soon = if w.reading {
            Some(READBACK_POLL)
        } else if (self.repaint || w.held) && !w.hidden {
            Some(PACE_TIMEOUT)
        } else {
            None
        };
        match (w.page, soon) {
            (Some(p), Some(s)) => Some(p.min(s)),
            (p, s) => p.or(s),
        }
    }

    #[cfg(test)]
    pub fn drew(&mut self) {
        self.repaint = false;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capture::{Reason, Since};
    use crate::dashboard::Live;
    use crate::present::Tag;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    /// A pacing at 60 fps, started at 0, and the numbers it writes.
    fn pacing() -> (Pacing, Arc<Live>) {
        let live = Arc::new(Live::new());
        (Pacing::new(Duration::ZERO, 60.0, Stats::new(Duration::ZERO, false, live.clone())), live)
    }

    #[test]
    fn a_held_frame_waits_for_the_next_one_unless_it_is_fresh_or_overdue() {
        // Sent on the compositor's frame callback alone, a held frame kept
        // every later one a refresh behind (55 ms kernel -> presented, not
        // 41, on the MS2130 2026-10-16): the next card frame replaces it.
        let (mut p, _) = pacing();
        let now = MS(1000);
        assert!(p.wake(now, Some(now - MS(2))), "within HOLD of the callback: sent, not dropped");
        assert!(!p.wake(now, Some(now - MS(10))), "a stale held frame waits for the next");
        assert!(!p.wake(now, None), "nothing to send");
        p.arrived();
        assert!(p.wake(now, Some(now - MS(10))), "a frame that came on this wake goes out");
        assert!(!p.wake(now, None), "once: the next wake has no new frame");
        p.repaint();
        assert!(p.wake(now, None), "an overlay change goes out");
        p.presented(now, false);
        assert!(p.wake(now, Some(now - MS(30))), "past 1.5 frame times (25 ms at 60 fps) the card has stopped: sent alone");
    }

    #[test]
    fn one_in_flight_holds_until_its_frame_callback_or_the_timeout() {
        let (mut p, _) = pacing();
        assert!(p.may_present(MS(0), true, false), "nothing in flight");
        p.presented(MS(0), true);
        assert!(!p.may_present(MS(50), true, false), "the compositor has not taken it");
        assert!(!p.may_present(MS(99), true, false));
        p.feedback(Outcome::FrameDone);
        assert!(p.may_present(MS(50), true, false), "its frame callback frees the way");
        p.presented(MS(50), true);
        assert!(p.may_present(MS(50) + PACE_TIMEOUT, true, false), "a callback PACE_TIMEOUT late counts as lost");
        assert_eq!(p.in_flight, 0);
        assert_eq!(p.stats().pace_timeouts(), 1);
        p.presented(MS(200), true);
        assert!(p.may_present(MS(210), false, false), "unpaced (DXGI): the swapchain paces itself");
        assert_eq!(p.stats().pace_timeouts(), 1);
    }

    #[test]
    fn a_window_with_no_area_draws_nothing_and_wakes_for_nothing() {
        let (mut p, _) = pacing();
        assert!(!p.may_present(MS(0), true, true));
        p.repaint();
        assert_eq!(p.next_wake(MS(0), Waiting { held: true, hidden: true, ..Waiting::default() }), None, "no PACE_TIMEOUT wake");
        assert_eq!(p.next_wake(MS(0), Waiting { held: true, ..Waiting::default() }), Some(PACE_TIMEOUT), "shown, it waits on pacing");
    }

    #[test]
    fn the_wake_is_the_earliest_of_the_page_pacing_and_readback() {
        let (mut p, _) = pacing();
        let at = |p: &mut Pacing, w: Waiting| p.next_wake(MS(1000), w);
        assert_eq!(at(&mut p, Waiting::default()), None, "nothing waits: the next event");
        assert_eq!(at(&mut p, Waiting { page: Some(MS(700)), ..Waiting::default() }), Some(MS(700)));
        assert_eq!(at(&mut p, Waiting { page: Some(MS(700)), held: true, ..Waiting::default() }), Some(PACE_TIMEOUT));
        assert_eq!(at(&mut p, Waiting { page: Some(MS(40)), held: true, ..Waiting::default() }), Some(MS(40)));
        assert_eq!(at(&mut p, Waiting { held: true, reading: true, ..Waiting::default() }), Some(READBACK_POLL), "a readback polls sooner");
        assert_eq!(at(&mut p, Waiting { reading: true, hidden: true, ..Waiting::default() }), Some(READBACK_POLL), "hidden or not");
        p.repaint();
        assert_eq!(at(&mut p, Waiting::default()), Some(PACE_TIMEOUT), "a repaint waiting on pacing");
    }

    #[test]
    fn a_page_deadline_passed_draws_again() {
        let (mut p, _) = pacing();
        p.next_wake(MS(1000), Waiting { page: Some(MS(300)), ..Waiting::default() });
        assert!(!p.wake(MS(1299), None), "not yet");
        assert!(p.wake(MS(1300), None), "the message went: drawn again");
    }

    #[test]
    fn the_refreshes_without_a_picture_are_not_counted_as_repeats() {
        // The laptop (#40): "repeated refreshes 1614" after the picture was
        // gone. The last presents' outcomes before it came in after the
        // no-signal state, with their earlier times; the first present after
        // the return counted every refresh since. An HDMI pull, here.
        let refresh = Duration::from_micros(16_681);
        let present = |p: &mut Pacing, at: Duration| {
            let tag = Tag { captured: at - MS(30), dequeued: at - MS(20), repaint: false };
            p.feedback(Outcome::Presented { tag, at, refresh, flags: 0 });
        };
        let gone = Signal::NoSignal { since: Since::Stopped, reason: Reason::Nothing };
        let at = Duration::from_secs;
        let (mut p, live) = pacing();
        present(&mut p, at(1));
        p.signal(&gone);
        // An outcome arriving after the no-signal state, from before it.
        present(&mut p, at(1) + refresh);
        p.signal(&Signal::Picture);
        present(&mut p, at(26));
        present(&mut p, at(26) + refresh);
        assert_eq!(live.counts().repeated, 0);
        // Without the return, the gap is counted: the test can fail.
        let (mut p, live) = pacing();
        present(&mut p, at(1));
        present(&mut p, at(26));
        assert!(live.counts().repeated > 0);
    }
}
