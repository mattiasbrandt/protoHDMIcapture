//! The feed owner (#22, #44): a viewer's feed from its start to its end.
//! `run` takes what it holds in one stated order:
//!
//! 1. the session (session.rs): `video.lock`, one video window at a time
//! 2. the debug log, with the lock held (DEBUG≠off)
//! 3. the source: the card, or the test pattern
//! 4. the audio sync loopback, from the card that opened
//! 5. --monitor's focus and REFRESH=match: after the audio, before the window
//! 6. `video.pid` (`announce`), then the dashboard's socket
//! 7. the window
//!
//! and `finish` ends what it holds in one stated order, after the window
//! run has ended:
//!
//! 1. the audio stops
//! 2. the dashboard's socket is removed
//! 3. the session ends (session.rs): the refresh rate restored, `video.pid`
//!    removed, `video.lock` released
//!
//! The debug log outlives them: its last line says how the run ended.
//!
//! view.sh restores the refresh before it removes its files; here the
//! dashboard's socket goes just before, as `Session` ends in one step. Both
//! go while the lock is held, and the display is back before it is let go.
//!
//! Each step runs whatever happened in the one before; each reports its own
//! failures. Dropping the owner finishes it, so an early return keeps the
//! order. The setup wizard's feed windows hold only the session, and keep
//! their own short path (main.rs `setup`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Fatal;
use crate::app::{self, AudioSetup};
use crate::audio::Loopback;
use crate::capture::{Format, FrameSource};
use crate::card::Node;
use crate::dashboard::{self, AudioLive, Live, Server};
use crate::debuglog::{self, DebugLog};
use crate::pattern::Pattern;
use crate::run::{Outcome, Quit};
use crate::session::{AlreadyRunning, Session};
use crate::settings::Settings;
use crate::sources;

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

/// What a viewer's feed starts from: what view() resolved.
pub struct Start {
    /// The run dir (docs/run-dir.md).
    pub run_dir: PathBuf,
    /// Resolved and valid.
    pub settings: Settings,
    pub opts: app::Options,
    pub format: Format,
    /// --test-pattern: the pattern instead of the card.
    pub test_pattern: bool,
}

/// Where a viewer's feed gets what it opens: the machine's in main.rs,
/// stand-ins in tests.
pub struct Parts {
    /// The card the settings pin, opened at the format, once the lock is
    /// held.
    pub open_card: OpenCard,
    /// The audio sync loopback for the card that opened (None with the
    /// test pattern), or None with AUDIO=none.
    pub loopback: MakeLoopback,
    /// debug.log at its path, teeing the output into it.
    pub debug_log: OpenLog,
    /// The window run (app::run).
    pub window: Window,
}

/// How a viewer's feed ended.
#[derive(Debug)]
pub enum Ran {
    /// It ran, and its window ended as a viewer's window ends: closed, a
    /// signal, or --seconds.
    Ended,
    /// Another feed holds the lock: nothing was started (main says so).
    AlreadyRunning(AlreadyRunning),
}

/// The viewer's feed: started in the order above, its window run, ended in
/// the order above. Err: why the viewer ends in error, said in the debug
/// log too.
pub fn run(start: Start, parts: Parts, quit: &Quit) -> Result<Ran, Fatal> {
    let Parts { open_card, loopback, debug_log, window } = parts;
    let session = match Session::start(&start.run_dir, start.opts.desktop.clone())? {
        Ok(s) => s,
        Err(running) => return Ok(Ran::AlreadyRunning(running)),
    };
    // DEBUG≠off: the output into debug.log from here, with the lock held
    // (a refused second launch must not truncate the running one's), as
    // view.sh's debug_tee. It outlives the feed's teardown. A log that
    // cannot start costs only the log.
    let mut log = match start.settings.get("DEBUG") {
        "off" => None,
        _ => match debug_log(&start.run_dir.join("debug.log")) {
            Ok(log) => {
                log.header(&start.opts.desktop);
                log.settings(&start.settings);
                Some(log)
            }
            Err(e) => {
                eprintln!("warning: debug.log: {e}; the feed goes on without it");
                None
            }
        },
    };
    let mut feed: Feed = Feed::new(Some(session));
    let result = feed.run(start, open_card, loopback, window, log.as_mut(), quit);
    feed.finish();
    if let Some(log) = log {
        log.ended(&result.as_ref().map(drop).map_err(Fatal::to_string));
    }
    result.map(|()| Ran::Ended)
}

pub type OpenCard = Box<dyn FnOnce(&Settings, Format) -> Result<(Box<dyn FrameSource>, Node), Fatal>>;
pub type MakeLoopback = Box<dyn FnOnce(&Settings, Option<&Node>, &Arc<Live>) -> Option<Loopback>>;
pub type OpenLog = Box<dyn FnOnce(&Path) -> io::Result<DebugLog>>;
pub type Window = Box<dyn FnOnce(Box<dyn FrameSource>, app::Options, &Quit) -> Outcome>;

impl Feed {
    /// The steps after the session and the log, through the window.
    fn run(
        &mut self,
        start: Start,
        open_card: OpenCard,
        loopback: MakeLoopback,
        window: Window,
        log: Option<&mut DebugLog>,
        quit: &Quit,
    ) -> Result<(), Fatal> {
        let Start { run_dir, settings: s, mut opts, format, test_pattern } = start;
        let live = opts.live.clone();
        let (source, node): (Box<dyn FrameSource>, _) = if test_pattern {
            eprintln!("video: the test pattern at {}x{} {} fps", format.width, format.height, format.fps);
            (Box::new(Pattern::new(format)), None)
        } else {
            let (source, node) = open_card(&s, format)?;
            (source, Some(node))
        };
        if let Some(log) = log {
            log.format(format, test_pattern);
            match debuglog::perf_every(std::env::var("DEBUG_PERF_INTERVAL").ok().as_deref()) {
                Ok(every) => log.perf(live.clone(), every),
                Err(e) => {
                    eprintln!("warning: {e}");
                    log.perf(live.clone(), debuglog::perf_every(None).expect("the default is valid"));
                }
            }
        }
        match loopback(&s, node.as_ref(), &live) {
            Some(loopback) => {
                let keys = self.set_audio(loopback).keys();
                keys.start();
                opts.audio = AudioSetup::Loopback(keys);
            }
            None => {
                opts.audio = AudioSetup::Off("none".into());
                live.set_audio(AudioLive::Off("AUDIO=none".into()));
            }
        }
        opts.source = s.get("SOURCE").to_string();
        if !opts.source.is_empty() && sources::get(&opts.source).is_none() {
            eprintln!(
                "warning: SOURCE={} has no profile in sources/; the overlay shows the tips for any HDMI source",
                opts.source
            );
            opts.source.clear();
        }
        // Monitors are known on Hyprland only; decided not on Windows (#38).
        #[cfg(windows)]
        if opts.monitor.take().is_some() {
            eprintln!("note: --monitor is not on Windows; the window opens where Windows puts it");
        }
        if let Some(name) = &opts.monitor {
            opts.refocus = opts.desktop.focused_monitor().filter(|m| m != name);
            opts.desktop.focus_monitor(name);
        }
        // As view.sh: after the audio, before the window.
        #[cfg(target_os = "linux")]
        if s.get("REFRESH") == "match"
            && let Some(session) = self.session()
        {
            session.match_refresh(format.fps, opts.monitor.as_deref());
        }
        // Decided not on Windows (#38): the display's own rate stays.
        #[cfg(windows)]
        if s.get("REFRESH") == "match" {
            eprintln!("note: REFRESH=match is not on Windows; the display's refresh rate is left as it is");
        }
        // DEBUG=window opens the dashboard with the feed, as in view.sh.
        let debug_window = s.get("DEBUG") == "window";
        opts.settings = Some(s);
        let stats = opts.stats;
        if quit.requested() {
            return Ok(());
        }
        // As view.sh writes it once its player is started.
        if let Some(session) = self.session() {
            session.announce()?;
        }
        // The dashboard's socket, with the video lock held; a failure costs
        // only the dashboard.
        // Its Display section reads the monitors and REFRESH=match's record.
        let display = dashboard::Display::new(opts.desktop.clone(), run_dir.join("refresh.restore"));
        match Server::start(&dashboard::socket_path(&run_dir), live, Some(display), dashboard::TICK) {
            Ok(server) => {
                self.set_dashboard(server);
                if debug_window {
                    let before = opts.desktop.dashboards().map_err(|e| eprintln!("warning: dashboard: cannot list the windows ({e}); it opens where the desktop puts it"));
                    match dashboard::open(None) {
                        // Moved off the window's monitor once the window places.
                        Ok(()) => opts.dashboard = before.ok(),
                        Err(e) => eprintln!("warning: DEBUG=window: {e}"),
                    }
                }
            }
            Err(e) => eprintln!("warning: no dashboard: {e}"),
        }
        let outcome = window(source, opts, quit);
        if let (true, Some(a)) = (stats, self.audio()) {
            for line in a.summary() {
                eprintln!("summary: audio {line}");
            }
        }
        // Every way the viewer's window ends is the end of the viewer.
        match outcome {
            Outcome::Closed | Outcome::Signal | Outcome::TimeUp => Ok(()),
            Outcome::CaptureEnded(e) => Err(capture_ended(&e).into()),
            Outcome::Failed(e) => Err(e.into()),
        }
    }
}

/// The viewer's end when its capture ended on its own (#40). It is not
/// opened again: a card reopened as it came back got stuck, and only a
/// power-off cleared it (capture-cards/ms2130.md). Only an error its source
/// marks "(the card is gone?)" is said as the card unplugged; any other end
/// is said as it is.
fn capture_ended(error: &str) -> String {
    const STUCK: &str = "if it then shows no picture, the card is stuck: take out its USB cable and its DC5V power for 10 s.";
    match error.strip_suffix(" (the card is gone?)") {
        Some(error) => format!("capture: the card was unplugged ({error}). Plug it back in and start the viewer again; {STUCK}"),
        None => format!("capture: {error}. Start the viewer again; {STUCK}"),
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
    /// dashboard, as `run` takes them.
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
    #[test]
    fn a_card_pulled_out_ends_the_viewer_with_how_to_start_it_again() {
        // The laptop's card pulled out (#40): Media Foundation's error.
        assert_eq!(
            super::capture_ended(r"\\?\usb#vid_1e4e&pid_7016: The video recording device is no longer present. (0xC00D3EA2) (the card is gone?)"),
            r"capture: the card was unplugged (\\?\usb#vid_1e4e&pid_7016: The video recording device is no longer present. (0xC00D3EA2)). Plug it back in and start the viewer again; if it then shows no picture, the card is stuck: take out its USB cable and its DC5V power for 10 s."
        );
        // Any other end is said as it is, not as an unplug: a source that
        // ended without an error (run.rs), or a stream that ended.
        assert_eq!(
            super::capture_ended("the capture stopped"),
            "capture: the capture stopped. Start the viewer again; if it then shows no picture, the card is stuck: take out its USB cable and its DC5V power for 10 s."
        );
        assert!(super::capture_ended(r"\\?\usb#vid_1e4e&pid_7016: the card's stream ended").starts_with(r"capture: \\?\usb#vid_1e4e&pid_7016: the card's stream ended. Start"));
    }


    /// A viewer's feed run on stand-ins (#44): a temp run dir, the test
    /// pattern or a card that fails, a fake loopback, a window that returns
    /// at once. Unix only: the debug log's stand-in is a pipe.
    #[cfg(unix)]
    mod start {
        use std::cell::RefCell;
        use std::path::{Path, PathBuf};
        use std::rc::Rc;

        use crate::Fatal;
        use crate::app::{self, AudioSetup};
        use crate::audio::{self, LoopbackKeys, Status};
        use crate::dashboard::{self, Live};
        use crate::debuglog::{DebugLog, StandIn};
        use crate::desktop::Desktop;
        use crate::live_changes::LiveChanges;
        use crate::run::{Outcome, Quit};
        use crate::session::{AlreadyRunning, Session};
        use crate::settings::Settings;
        use crate::capture::Format;
        use crate::feed::{self, Parts, Ran, Start};
        use std::sync::Arc;

        const FORMAT: Format = Format { width: 4, height: 2, fps: 60.0, pixels: crate::capture::Pixels::Yuyv };

        fn run_dir(name: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("protohdmi-feed-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn start(dir: &Path, config: &str, test_pattern: bool) -> Start {
            let file = dir.join("config");
            std::fs::write(&file, config).unwrap();
            let path = file.display().to_string();
            let settings = Settings::resolve_with(&move |k| (k == "CONFIG_FILE").then(|| path.clone()));
            assert!(settings.errors().is_empty(), "{:?}", settings.errors());
            let opts = app::Options {
                live_changes: LiveChanges::DEFAULT,
                monitor: None,
                seconds: None,
                stats: false,
                audio: AudioSetup::Off(String::new()),
                desktop: Desktop::Generic,
                source: String::new(),
                settings: None,
                wizard: None,
                live: Arc::new(Live::new()),
                dashboard: None,
                refocus: None,
            };
            Start { run_dir: dir.to_path_buf(), settings, opts, format: FORMAT, test_pattern }
        }

        /// What the stand-ins saw: the order of the steps that left a mark,
        /// the loopback's keys and its starts, and the log's pipe.
        #[derive(Default)]
        struct Seen {
            steps: Rc<RefCell<Vec<&'static str>>>,
            keys: Rc<RefCell<Option<LoopbackKeys>>>,
            starts: Rc<RefCell<Option<audio::fake::Script>>>,
            stand_in: Rc<RefCell<Option<StandIn>>>,
        }

        /// Parts that record into `seen`; `card` opens the card, `window`
        /// is the window run.
        fn parts(
            seen: &Seen,
            card: impl FnOnce() -> Result<(Box<dyn crate::capture::FrameSource>, crate::card::Node), Fatal> + 'static,
            window: impl FnOnce(&app::Options) -> Outcome + 'static,
        ) -> Parts {
            let (steps, script, stand_in) = (seen.steps.clone(), seen.starts.clone(), seen.stand_in.clone());
            let log_steps = seen.steps.clone();
            let card_steps = seen.steps.clone();
            let keys = seen.keys.clone();
            Parts {
                open_card: Box::new(move |_, _| {
                    card_steps.borrow_mut().push("card");
                    card()
                }),
                loopback: Box::new(move |_, _, _| {
                    steps.borrow_mut().push("loopback");
                    let (loopback, s) = audio::fake::loopback(50);
                    *script.borrow_mut() = Some(s);
                    Some(loopback)
                }),
                debug_log: Box::new(move |path| {
                    log_steps.borrow_mut().push("log");
                    let (log, pipe) = DebugLog::on_stand_in(path)?;
                    *stand_in.borrow_mut() = Some(pipe);
                    Ok(log)
                }),
                window: Box::new(move |_, opts, _| {
                    if let AudioSetup::Loopback(k) = &opts.audio {
                        *keys.borrow_mut() = Some(k.clone());
                    }
                    window(&opts)
                }),
            }
        }

        fn last_line(path: &Path) -> String {
            std::fs::read_to_string(path).unwrap().lines().last().unwrap_or_default().to_string()
        }

        fn lock_is_free(dir: &Path) -> bool {
            matches!(Session::start(dir, Desktop::Generic), Ok(Ok(_)))
        }

        #[test]
        fn a_feed_starts_and_ends_in_its_order() {
            let dir = run_dir("order");
            let seen = Seen::default();
            let (d, keys) = (dir.clone(), seen.keys.clone());
            let window = move |_: &app::Options| {
                // The lock is held: a second session is refused.
                let second = Session::start(&d, Desktop::Generic).unwrap().err();
                assert_eq!(second, Some(AlreadyRunning { pid: Some(std::process::id()), focused: false }), "announced, and the lock held");
                assert!(dashboard::socket_path(&d).exists(), "the dashboard's socket");
                assert!(d.join("debug.log").exists());
                let status = keys.borrow().as_ref().map(LoopbackKeys::status);
                assert!(matches!(status, Some(Status::Running { delay_ms: 50, .. })), "{status:?}");
                Outcome::Closed
            };
            let p = parts(&seen, || panic!("the test pattern opens no card"), window);
            let ran = feed::run(start(&dir, "DEBUG=log\n", true), p, &Quit::new());
            assert!(matches!(ran, Ok(Ran::Ended)), "{ran:?}");
            assert_eq!(*seen.steps.borrow(), ["log", "loopback"]);
            assert_eq!(*seen.starts.borrow().as_ref().unwrap().starts.borrow(), [50], "started once");
            // The audio stopped, the socket gone, video.pid removed, the lock free.
            assert!(matches!(seen.keys.borrow().as_ref().unwrap().status(), Status::Stopped(_)));
            assert!(!dashboard::socket_path(&dir).exists());
            assert!(!dir.join("video.pid").exists());
            assert!(lock_is_free(&dir));
            assert!(last_line(&dir.join("debug.log")).ends_with("[dbg] ended"), "{}", last_line(&dir.join("debug.log")));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn the_windows_end_is_the_logs_last_line_and_the_result() {
            let dir = run_dir("ended");
            let seen = Seen::default();
            let p = parts(&seen, || panic!("no card"), |_| Outcome::CaptureEnded("the capture stopped".into()));
            let ran = feed::run(start(&dir, "DEBUG=log\n", true), p, &Quit::new());
            let e = ran.expect_err("a capture that ended is the viewer's error").to_string();
            assert!(e.starts_with("capture: the capture stopped. Start the viewer again"), "{e}");
            assert!(last_line(&dir.join("debug.log")).ends_with(&format!("[dbg] ended: {e}")));
            assert!(lock_is_free(&dir));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_second_launch_is_refused_and_the_running_feed_untouched() {
            let dir = run_dir("second");
            let mut running = Session::start(&dir, Desktop::Generic).unwrap().unwrap();
            running.announce().unwrap();
            let seen = Seen::default();
            let p = parts(&seen, || panic!("refused: no card"), |_| panic!("refused: no window"));
            let ran = feed::run(start(&dir, "DEBUG=log\n", true), p, &Quit::new());
            assert!(matches!(&ran, Ok(Ran::AlreadyRunning(AlreadyRunning { pid: Some(p), .. })) if *p == std::process::id()), "{ran:?}");
            assert!(seen.steps.borrow().is_empty(), "nothing started: {:?}", seen.steps.borrow());
            assert!(!dir.join("debug.log").exists(), "the running feed's log is not truncated");
            assert!(dir.join("video.pid").exists());
            assert!(!lock_is_free(&dir));
            drop(running);
            assert!(lock_is_free(&dir));
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_card_that_does_not_open_ends_what_was_started() {
            let dir = run_dir("card");
            let seen = Seen::default();
            let p = parts(&seen, || Err(Fatal::Other("cannot open /dev/videoN: busy".into())), |_| panic!("no window"));
            let ran = feed::run(start(&dir, "DEBUG=log\n", false), p, &Quit::new());
            assert_eq!(ran.expect_err("the card's error").to_string(), "cannot open /dev/videoN: busy");
            assert_eq!(*seen.steps.borrow(), ["log", "card"], "no loopback started");
            assert!(last_line(&dir.join("debug.log")).ends_with("[dbg] ended: cannot open /dev/videoN: busy"), "{}", std::fs::read_to_string(dir.join("debug.log")).unwrap());
            assert!(!dir.join("video.pid").exists(), "never announced");
            assert!(!dashboard::socket_path(&dir).exists());
            assert!(lock_is_free(&dir));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_quit_before_the_window_ends_cleanly_without_one() {
            let dir = run_dir("quit");
            let seen = Seen::default();
            let p = parts(&seen, || panic!("no card"), |_| panic!("no window after a quit"));
            let quit = Quit::new();
            quit.request();
            let ran = feed::run(start(&dir, "DEBUG=log\n", true), p, &quit);
            assert!(matches!(ran, Ok(Ran::Ended)), "{ran:?}");
            assert!(!dir.join("video.pid").exists(), "never announced");
            assert!(lock_is_free(&dir));
            assert!(last_line(&dir.join("debug.log")).ends_with("[dbg] ended"));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_card_that_opens_opens_before_its_loopback_is_built() {
            let dir = run_dir("opens");
            let seen = Seen::default();
            let card = || {
                let node = crate::card::Node { path: "/dev/videoN".into(), serial: String::new(), port: String::new(), card_type: "a card".into() };
                Ok((Box::new(crate::pattern::Pattern::new(FORMAT)) as Box<dyn crate::capture::FrameSource>, node))
            };
            let p = parts(&seen, card, |_| Outcome::Closed);
            let ran = feed::run(start(&dir, "DEBUG=log\n", false), p, &Quit::new());
            assert!(matches!(ran, Ok(Ran::Ended)), "{ran:?}");
            assert_eq!(*seen.steps.borrow(), ["log", "card", "loopback"]);
            assert!(lock_is_free(&dir));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn audio_none_starts_no_loopback_and_says_so() {
            let dir = run_dir("noaudio");
            let seen = Seen::default();
            let window = |opts: &app::Options| {
                assert!(matches!(&opts.audio, AudioSetup::Off(v) if v == "none"), "the panel's AUDIO");
                assert_eq!(opts.live.audio_said(), ("off".into(), "AUDIO=none".into(), vec![]), "the dashboard's");
                Outcome::Closed
            };
            let mut p = parts(&seen, || panic!("the test pattern opens no card"), window);
            let steps = seen.steps.clone();
            p.loopback = Box::new(move |_, _, _| {
                steps.borrow_mut().push("loopback");
                None
            });
            let ran = feed::run(start(&dir, "DEBUG=off\nAUDIO=none\n", true), p, &Quit::new());
            assert!(matches!(ran, Ok(Ran::Ended)), "{ran:?}");
            assert_eq!(*seen.steps.borrow(), ["loopback"], "asked, none built");
            assert!(seen.keys.borrow().is_none(), "no keys reached the window");
            assert!(lock_is_free(&dir));
            drop(seen);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
