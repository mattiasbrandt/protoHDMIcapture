//! The feed owner (#22): the resources around a feed's window, ended in one
//! stated order by `finish`, after the window run has ended:
//!
//! 1. the audio stops
//! 2. the dashboard's socket is removed
//! 3. the session ends (session.rs): the refresh rate restored, `video.pid`
//!    removed, `video.lock` released
//!
//! view.sh restores the refresh before it removes its files; here the
//! dashboard's socket goes just before, as `Session` ends in one step. Both
//! go while the lock is held, and the display is back before it is let go.
//!
//! Each step runs whatever happened in the one before; each reports its own
//! failures. Dropping the owner finishes it, so an early return keeps the
//! order. The setup wizard's feed windows hold only the session.

use crate::audio::Loopback;
use crate::dashboard::Server;

/// The session part.
pub type SessionPart = crate::session::Session;

/// What a feed holds besides its window, ended by `finish`. The parts are
/// type parameters so that tests can record the order.
pub struct Feed<A = Loopback, D = Server, S = SessionPart> {
    // Not in the finish order, on purpose: `finish` is the one place that
    // states it, and a test fails if the fields' own drop order is relied on.
    session: Option<S>,
    dashboard: Option<D>,
    audio: Option<A>,
}

impl<A, D, S> Feed<A, D, S> {
    pub fn new(session: Option<S>) -> Self {
        Self { session, dashboard: None, audio: None }
    }

    pub fn session(&mut self) -> Option<&mut S> {
        self.session.as_mut()
    }

    pub fn set_audio(&mut self, audio: A) -> &A {
        self.audio.insert(audio)
    }

    pub fn audio(&self) -> Option<&A> {
        self.audio.as_ref()
    }

    pub fn set_dashboard(&mut self, dashboard: D) {
        self.dashboard = Some(dashboard);
    }

    /// Ends what it holds, in the order above. A second call finds nothing.
    pub fn finish(&mut self) {
        drop(self.audio.take());
        drop(self.dashboard.take());
        drop(self.session.take());
    }
}

impl<A, D, S> Drop for Feed<A, D, S> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    type Log = Rc<RefCell<Vec<&'static str>>>;

    /// A part that records its end; `fails` says so as it ends, as the
    /// real parts report a failure and end anyway.
    struct Part {
        name: &'static str,
        log: Log,
        fails: bool,
    }

    impl Drop for Part {
        fn drop(&mut self) {
            if self.fails {
                eprintln!("warning: {} failed to end cleanly", self.name);
            }
            self.log.borrow_mut().push(self.name);
        }
    }

    fn part(log: &Log, name: &'static str, fails: bool) -> Part {
        Part { name, log: log.clone(), fails }
    }

    /// A viewer's feed: the session first, then the audio and the
    /// dashboard, as view() takes them.
    fn full(log: &Log, fails: bool) -> Feed<Part, Part, Part> {
        let mut feed = Feed::new(Some(part(log, "session", fails)));
        feed.set_audio(part(log, "audio", fails));
        feed.set_dashboard(part(log, "dashboard", fails));
        feed
    }

    #[test]
    fn finish_ends_in_the_stated_order() {
        let log = Log::default();
        let mut feed = full(&log, false);
        feed.finish();
        assert_eq!(*log.borrow(), ["audio", "dashboard", "session"]);
        drop(feed);
        assert_eq!(log.borrow().len(), 3, "a second finish finds nothing");
    }

    #[test]
    fn dropped_it_ends_in_the_same_order() {
        // An early return: the owner dropped, never finished.
        let log = Log::default();
        drop(full(&log, false));
        assert_eq!(*log.borrow(), ["audio", "dashboard", "session"]);
    }

    #[test]
    fn a_failing_step_does_not_stop_the_rest() {
        let log = Log::default();
        full(&log, true).finish();
        assert_eq!(*log.borrow(), ["audio", "dashboard", "session"]);
    }

    #[test]
    fn a_feed_with_some_parts_ends_those() {
        // Before the audio started (the card failed to open), and the
        // wizard's feed windows: the session only.
        let log = Log::default();
        drop(Feed::<Part, Part, Part>::new(Some(part(&log, "session", false))));
        assert_eq!(*log.borrow(), ["session"]);
        let log = Log::default();
        let mut feed = Feed::<Part, Part, Part>::new(Some(part(&log, "session", false)));
        feed.set_dashboard(part(&log, "dashboard", false));
        drop(feed);
        assert_eq!(*log.borrow(), ["dashboard", "session"]);
    }
}
