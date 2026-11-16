//! The setup wizard's run (#13): `--setup`, or a first run. `run` holds
//! `setup.lock` as legacy/lib/setup.sh does (one wizard at a time), writes
//! `setup.pid`, and opens the wizard's windows one after another: the
//! choosing window, and the feed window from the card it chose. How each
//! window ends is the wizard's to hear: the card gone or refused during
//! the feed sends it back to the format stage, a signal ends it unsaved.
//! At the end `setup.done` says whether it saved, and `setup.pid` goes
//! (docs/run-dir.md).
//!
//! The windows and the card come in as `Parts`, so the tests run every
//! path with stand-ins; main.rs hands in `app::run` and `card::open`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::app;
use crate::capture::{Format, FrameSource};
use crate::card::Node;
use crate::dashboard::Live;
use crate::desktop::Desktop;
use crate::feed;
use crate::live_changes::LiveChanges;
use crate::run::{Outcome, Quit};
use crate::session::Session;
use crate::wizard::{self, Phase, Wizard};

pub struct Start {
    /// The run dir (docs/run-dir.md).
    pub run_dir: PathBuf,
    /// The machine the wizard asks (wizard::Machine).
    pub system: Box<dyn wizard::System>,
    /// Where the wizard saves.
    pub config_file: PathBuf,
    /// No settings file yet: the wizard says so, and skipping it starts
    /// the feed.
    pub first_run: bool,
    /// The feed windows' picture settings, windowed.
    pub live_changes: LiveChanges,
    /// For the feed's session (a second launch focuses the running one).
    pub desktop: Desktop,
}

/// A window run: the wizard's own (`wizard::Idle`), or the card's feed.
pub type Window = Box<dyn FnMut(Box<dyn FrameSource>, app::Options, &Quit) -> Outcome>;
/// The card the wizard chose, opened at the mode it chose.
pub type OpenCard = Box<dyn FnMut(&Node, Format) -> Result<Box<dyn FrameSource>, String>>;

pub struct Parts {
    pub window: Window,
    pub open_card: OpenCard,
}

/// The wizard, from its first window to its end. True when the feed
/// should start: saved and asked for, or the first run skipped, and no
/// signal. Err for what ends the wizard itself: another wizard running,
/// the run dir's files, or the choosing window failing.
pub fn run(start: Start, parts: Parts, quit: &Quit) -> Result<bool, String> {
    let Start { run_dir: dir, system, config_file, first_run, live_changes, desktop } = start;
    let Parts { mut window, mut open_card } = parts;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock = std::fs::File::create(dir.join("setup.lock")).map_err(|e| format!("setup.lock: {e}"))?;
    // flock(LOCK_EX | LOCK_NB) on Linux, legacy/lib/setup.sh's lock; LockFileEx on Windows.
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err("another setup wizard is already running".into()),
        Err(std::fs::TryLockError::Error(e)) => return Err(format!("setup.lock: {e}")),
    }
    let wiz = Rc::new(RefCell::new(Wizard::new(system, config_file, first_run, &|k| std::env::var_os(k).is_some())?));
    // After every early return above, so none leaves it behind.
    std::fs::write(dir.join("setup.pid"), format!("{}\n", std::process::id())).map_err(|e| format!("setup.pid: {e}"))?;
    // Each feed window's numbers are its own, and nobody reads them.
    let opts = || app::Options {
        audio: app::AudioSetup::Off("the wizard's".into()),
        wizard: Some(wiz.clone()),
        ..app::Options::new(live_changes.clone(), Arc::new(Live::new()))
    };

    let result = (|| -> Result<(), String> {
        loop {
            if quit.requested() {
                wiz.borrow_mut().abandon();
            }
            if wiz.borrow().outcome().is_some() {
                return Ok(());
            }
            let phase = wiz.borrow().phase();
            wiz.borrow_mut().running = phase;
            let outcome = match phase {
                Phase::Choose => window(Box::new(wizard::Idle), opts(), quit),
                Phase::Feed => {
                    // One video window at a time, with view.sh too.
                    let session = match Session::start(&dir, desktop.clone())? {
                        Ok(s) => s,
                        Err(running) => {
                            let pid = running.pid.map_or(String::new(), |p| format!(" (pid {p})"));
                            wiz.borrow_mut().feed_failed(format!("a feed is running{pid}. Close it first."));
                            continue;
                        }
                    };
                    // The feed owner with the session only: the audio-sync
                    // loopback is the wizard's.
                    let mut feed: feed::Feed = feed::Feed::new(Some(session));
                    let Some((node, format)) = wiz.borrow().feed() else {
                        return Err("the wizard reached the feed without a card".into());
                    };
                    let source = match format.and_then(|f| open_card(&node, f)) {
                        Ok(source) => source,
                        Err(e) => {
                            wiz.borrow_mut().feed_failed(e);
                            continue;
                        }
                    };
                    if let Some(s) = feed.session() {
                        s.announce()?;
                    }
                    let outcome = window(source, opts(), quit);
                    feed.finish();
                    outcome
                }
            };
            match outcome {
                // The wizard moved on, or ended (its window's close button
                // ends it unsaved): its state says what comes next.
                Outcome::Closed | Outcome::TimeUp => {}
                Outcome::Signal => wiz.borrow_mut().abandon(),
                // The card gone mid-check, or the feed's window failing,
                // is the wizard's to say, not the end.
                Outcome::CaptureEnded(e) if phase == Phase::Feed => wiz.borrow_mut().feed_failed(format!("capture: {e}")),
                Outcome::Failed(e) if phase == Phase::Feed => wiz.borrow_mut().feed_failed(e),
                Outcome::CaptureEnded(e) => return Err(format!("capture: {e}")),
                Outcome::Failed(e) => return Err(e),
            }
        }
    })();
    let saved = wiz.borrow().saved();
    // legacy/launch.sh reads a missing setup.done as "not saved".
    if let Err(e) = std::fs::write(dir.join("setup.done"), if saved { "saved\n" } else { "cancelled\n" }) {
        eprintln!("warning: setup.done: {e}; legacy/launch.sh will read the wizard as cancelled");
    }
    let _ = std::fs::remove_file(dir.join("setup.pid"));
    result?;
    Ok(wiz.borrow().outcome() == Some(wizard::Outcome::Play) && !quit.requested())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::tests::{fake, press, signal_ok};
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::path::Path;

    /// One window run: what it does with the wizard, the phase it opened
    /// in, and the run dir; then how it ends.
    type Script = Box<dyn FnOnce(&mut Wizard, Phase, &Path) -> Outcome>;

    struct Seen {
        /// The phase each window opened in.
        windows: Rc<RefCell<Vec<Phase>>>,
        /// The cards opened.
        opened: Rc<Cell<u32>>,
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phc-setup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn start(dir: &Path, first_run: bool) -> Start {
        let config_file = dir.join("config");
        if !first_run {
            std::fs::write(&config_file, "SOURCE=ps5\n").unwrap();
        }
        Start {
            run_dir: dir.to_path_buf(),
            system: Box::new(fake()),
            config_file,
            first_run,
            live_changes: LiveChanges::DEFAULT,
            desktop: Desktop::Generic,
        }
    }

    /// Windows that run `scripts` in turn (a window past the last fails
    /// the test), and a card that opens, or fails with `card_error`.
    fn stand_ins(dir: &Path, scripts: Vec<Script>, card_error: Option<&'static str>) -> (Parts, Seen) {
        let seen = Seen { windows: Rc::default(), opened: Rc::default() };
        let (windows, opened) = (seen.windows.clone(), seen.opened.clone());
        let mut scripts: VecDeque<Script> = scripts.into();
        let dir = dir.to_path_buf();
        let window: Window = Box::new(move |_source, opts, _quit| {
            let wiz = opts.wizard.expect("the wizard's window");
            let phase = wiz.borrow().running;
            windows.borrow_mut().push(phase);
            let script = scripts.pop_front().unwrap_or_else(|| panic!("an unexpected window in {phase:?}"));
            script(&mut wiz.borrow_mut(), phase, &dir)
        });
        let open_card: OpenCard = Box::new(move |_node, format| {
            opened.set(opened.get() + 1);
            match card_error {
                Some(e) => Err(e.into()),
                None => Ok(Box::new(crate::pattern::Pattern::new(format))),
            }
        });
        (Parts { window, open_card }, seen)
    }

    /// The choosing window: through to the signal check, which needs the
    /// feed (SOURCE=ps5 saved, as wizard.rs's tests).
    fn to_the_feed() -> Script {
        Box::new(|w, _, _| {
            press(w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
            assert_eq!(w.phase(), Phase::Feed);
            Outcome::Closed
        })
    }

    /// The choosing window again, saying `why` the feed failed; then closed.
    fn back_saying(why: &'static str) -> Script {
        Box::new(move |w, phase, _| {
            assert_eq!(phase, Phase::Choose);
            assert!(w.lines().iter().any(|l| l == &format!("Failed: {why}")), "{:#?}", w.lines());
            w.abandon();
            Outcome::Closed
        })
    }

    /// setup.done, once setup.pid is gone (every end removes it).
    fn done(dir: &Path) -> Option<String> {
        assert!(!dir.join("setup.pid").exists(), "setup.pid left behind");
        std::fs::read_to_string(dir.join("setup.done")).ok()
    }

    #[test]
    fn a_second_wizard_is_refused_and_leaves_no_files() {
        let d = dir("lock");
        let held = std::fs::File::create(d.join("setup.lock")).unwrap();
        held.try_lock().unwrap();
        let (parts, seen) = stand_ins(&d, vec![], None);
        let e = run(start(&d, false), parts, &Quit::new()).unwrap_err();
        assert_eq!(e, "another setup wizard is already running");
        assert!(seen.windows.borrow().is_empty());
        assert!(!d.join("setup.pid").exists() && done(&d).is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn closed_unsaved_says_cancelled_and_removes_setup_pid() {
        let d = dir("closed");
        let (parts, seen) = stand_ins(
            &d,
            vec![Box::new(|w, _, dir| {
                // setup.pid while the wizard runs.
                assert_eq!(std::fs::read_to_string(dir.join("setup.pid")).unwrap(), format!("{}\n", std::process::id()));
                w.abandon();
                Outcome::Closed
            })],
            None,
        );
        assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(false));
        assert_eq!(*seen.windows.borrow(), [Phase::Choose]);
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        assert!(!d.join("setup.pid").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_signal_ends_the_wizard_unsaved() {
        let d = dir("signal");
        let (parts, seen) = stand_ins(&d, vec![Box::new(|_, _, _| Outcome::Signal)], None);
        assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(false));
        assert_eq!(seen.windows.borrow().len(), 1);
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_choosing_window_failing_ends_the_wizard_with_its_error() {
        for (outcome, error) in
            [(Outcome::Failed("no adapter".into()), "no adapter"), (Outcome::CaptureEnded("gone".into()), "capture: gone")]
        {
            let d = dir("choose-failed");
            let (parts, _) = stand_ins(&d, vec![Box::new(move |_, _, _| outcome)], None);
            assert_eq!(run(start(&d, false), parts, &Quit::new()), Err(error.to_string()));
            // The files are said and cleaned all the same.
            assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
            assert!(!d.join("setup.pid").exists());
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn the_card_refused_goes_back_to_the_format_stage() {
        let d = dir("card-refused");
        let (parts, seen) = stand_ins(&d, vec![to_the_feed(), back_saying("the card is busy")], Some("the card is busy"));
        assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(false));
        assert_eq!(*seen.windows.borrow(), [Phase::Choose, Phase::Choose]);
        assert_eq!(seen.opened.get(), 1);
        // The session it took for the feed is let go.
        assert!(Session::start(&d, Desktop::Generic).unwrap().is_ok());
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_feed_ending_goes_back_to_the_format_stage() {
        for (outcome, why) in [
            (Outcome::CaptureEnded("the card is gone".into()), "capture: the card is gone"),
            (Outcome::Failed("no surface".into()), "no surface"),
        ] {
            let d = dir("feed-ended");
            let feed: Script = Box::new(move |_, phase, dir| {
                assert_eq!(phase, Phase::Feed);
                // The feed is announced while its window runs.
                assert!(dir.join("video.pid").exists());
                outcome
            });
            let (parts, seen) = stand_ins(&d, vec![to_the_feed(), feed, back_saying(why)], None);
            assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(false));
            assert_eq!(*seen.windows.borrow(), [Phase::Choose, Phase::Feed, Phase::Choose]);
            assert!(!d.join("video.pid").exists(), "the feed's session ends with its window");
            assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn a_running_feed_is_said_and_the_card_left_alone() {
        let d = dir("feed-running");
        let _running = Session::start(&d, Desktop::Generic).unwrap().ok().unwrap();
        std::fs::write(d.join("video.pid"), "4242\n").unwrap();
        let (parts, seen) = stand_ins(&d, vec![to_the_feed(), back_saying("a feed is running (pid 4242). Close it first.")], None);
        assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(false));
        assert_eq!(seen.opened.get(), 0);
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_first_run_skipped_plays_unless_a_quit_came() {
        let skip = || -> Script {
            Box::new(|w, _, _| {
                press(w, &["Down", "Enter"]);
                Outcome::Closed
            })
        };
        let d = dir("skip");
        let (parts, _) = stand_ins(&d, vec![skip()], None);
        assert_eq!(run(start(&d, true), parts, &Quit::new()), Ok(true));
        // Skipped is not saved.
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        std::fs::remove_dir_all(&d).unwrap();

        let d = dir("skip-quit");
        let quit = Quit::new();
        let q = quit.clone();
        let (parts, _) = stand_ins(
            &d,
            vec![Box::new(move |w, _, _| {
                press(w, &["Down", "Enter"]);
                q.request();
                Outcome::Closed
            })],
            None,
        );
        assert_eq!(run(start(&d, true), parts, &quit), Ok(false));
        assert_eq!(done(&d).as_deref(), Some("cancelled\n"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn saved_and_play_says_saved_and_starts_the_feed() {
        let d = dir("saved");
        let feed: Script = Box::new(|w, phase, _| {
            assert_eq!(phase, Phase::Feed);
            signal_ok(w);
            // Every later stage at its default, the review saved, then the
            // last page's default: play.
            for _ in 0..60 {
                if w.outcome().is_some() {
                    break;
                }
                press(w, &["Enter"]);
            }
            assert!(w.saved(), "{:#?}", w.lines());
            Outcome::Closed
        });
        let (parts, seen) = stand_ins(&d, vec![to_the_feed(), feed], None);
        assert_eq!(run(start(&d, false), parts, &Quit::new()), Ok(true));
        assert_eq!(*seen.windows.borrow(), [Phase::Choose, Phase::Feed]);
        assert_eq!(done(&d).as_deref(), Some("saved\n"));
        assert!(std::fs::read_to_string(d.join("config")).unwrap().contains("CARD_SERIAL=20000130041415\n"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
