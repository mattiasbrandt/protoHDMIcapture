//! protoHDMIcapture: one program for capture, display and audio.

mod app;
mod audio;
mod capture;
mod capture_cards;
mod card;
#[cfg(feature = "dev")]
mod clip;
mod dashboard;
mod debuglog;
mod desktop;
mod duplicate;
mod feed;
mod framegen;
mod gamemode;
mod gpustat;
mod gputime;
mod keys;
mod level;
mod live_changes;
mod mjpeg;
mod nis;
mod osd;
mod overlay;
mod pacing;
mod pattern;
mod picture;
mod present;
#[cfg(target_os = "linux")]
mod pw;
mod run;
mod scale;
#[cfg(feature = "dev")]
mod score;
mod screenshot;
mod session;
mod settings;
mod setup;
mod sources;
mod stats;
mod view_sh_only;
mod wizard;

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use settings::Settings;

/// The usage text; a dev build (#64) adds its flags.
macro_rules! usage {
    () => {
        "usage: protohdmicapture [--setup | --settings | --dashboard | -h | --help | -V | --version] [--debug] [--menu]
       measurements: [--seconds N] [--monitor NAME] [--stats] [--test-pattern]

  (none)          the card's feed, with the settings file (--settings shows it)
  --setup         the setup wizard, then the feed
  --settings      each setting, its value and where it came from
  --dashboard     the running viewer's numbers in this terminal
  -h, --help      this text
  -V, --version   the version
  --debug         DEBUG=window: the dashboard opens with the feed
  --menu          as started from the desktop entry: REFRESH=match
                  unless the environment or the settings file sets it
                  (REFRESH=match works on Hyprland only)
  --seconds N     end the feed after N seconds
  --monitor NAME  open the window on monitor NAME (Hyprland only)
  --stats         print the latency numbers every 10 s, and a summary
  --test-pattern  colour bars instead of the card"
    };
}
#[cfg(not(feature = "dev"))]
const USAGE: &str = usage!();
#[cfg(feature = "dev")]
const USAGE: &str = concat!(
    usage!(),
    "

  dev build (#64): [--record NAME --seconds N [--note TEXT]] [--replay FILE] [--framegen MODE]
  dev build (#81): --framegen-score CLIP --framegen MODE [--every N] [--worst K] [--out DIR] [--hud X,Y,W,H]
  --framegen-score CLIP  headless: MODE's generator scored on CLIP,
                  the picture halfway between new frames N0 and N2 against
                  the real N1 (PSNR, tear blocks); every Nth triple, the
                  worst K (5) to DIR as PNGs, the HUD rectangle on its own
  --record NAME   the card's frames (or --test-pattern's) into
                  /var/tmp/protohdmicapture-clips/NAME.clip while the
                  window shows them as in any run, for --seconds or until
                  the window closes; what was recorded into NAME.json
                  beside it. With --framegen the window generates frames;
                  the clip holds the card's frames only
  --note TEXT     a note for NAME.json (\"GTA 6, standing still\")
  --replay FILE   a recorded clip instead of the card, looping
  --framegen MODE frame generation for this run, over FRAMEGEN (#66):
                  ffx or rife (FRAMEGEN's, #73, #74), or copy (the last
                  new frame, a refresh late), blend (the two new frames
                  half and half, #67), new-only (duplicates not presented)
                  or ffx-base (FidelityFX optical flow at half the
                  resolution, ffx's base, #68)"
);

/// Called first thing in main, before any thread exists.
/// - MANGOHUD=0, as view.sh does for mpv: MangoHud is an implicit Vulkan
///   layer that a session-wide MANGOHUD=1 injects into every Vulkan app,
///   an overlay draw in the latency path (#5).
/// - node.dont-fallback for every PipeWire stream: a missing card audio
///   input must not become the microphone (#5, the Codex review).
///   libpipewire adds PIPEWIRE_PROPS to each new stream; WirePlumber
///   honours the key. An existing PIPEWIRE_PROPS is kept, with a warning.
/// - DISABLE_RTKIT=1 for libpipewire's client module-rt (#76): with RTKit
///   it clamps this process's RLIMIT_RTTIME to the portal's (or RTKit's)
///   RTTimeUSecMax, which has been 0 ("clamping rt.time.soft from … to 0
///   because of RTKit"; 200000 after the portal restarted, #90), and an
///   unprivileged process cannot raise a hard limit again. cpal then promotes its audio threads through RTKit
///   itself (audio_thread_priority), which first sets RLIMIT_RTTIME to
///   (one buffer, RTKit's maximum): from 0/0 that is a raise, refused
///   (EPERM), and the sound ran without real-time priority. Without
///   RTKit, module-rt leaves the limit alone and cpal's promotion works.
fn environment() {
    // Safety: no other thread exists yet.
    unsafe { env::set_var("MANGOHUD", "0") };
    #[cfg(target_os = "linux")]
    match env::var("PIPEWIRE_PROPS") {
        Ok(v) if !v.contains("node.dont-fallback") => {
            eprintln!("warning: PIPEWIRE_PROPS is set without node.dont-fallback; the audio may fall back to the default source");
        }
        Ok(_) => {}
        // Safety: as above.
        Err(_) => unsafe { env::set_var("PIPEWIRE_PROPS", "{ node.dont-fallback = true }") },
    }
    // Safety: as above.
    #[cfg(target_os = "linux")]
    unsafe {
        env::set_var("DISABLE_RTKIT", "1")
    };
    // What the refusal note compares with, before any library runs (#90).
    #[cfg(target_os = "linux")]
    audio::note_rttime_at_start();
}

/// The audio loopback the settings ask for, not started yet: None for
/// AUDIO=none. Each start (the first, and the keys' restarts, #29) finds
/// the source again and starts it at the delay asked for; a failed one is
/// reported (the terminal, `live` for the dashboard, a desktop
/// notification) and the feed goes on without sound, as in view.sh.
/// `card`: None with `--test-pattern`, where only SRC can name an input.
fn audio(s: &Settings, card: Option<&card::Node>, live: &Arc<dashboard::Live>) -> Option<audio::Loopback> {
    // AUDIO=mpv is refused before this (view_sh_only).
    if s.get("AUDIO") == "none" {
        return None;
    }
    let src = s.get("SRC").to_string();
    let sink = Some(s.get("SINK").to_string()).filter(|v| !v.is_empty());
    // Validated: DELAY is 0..1 s, LAT_MS 1..1000.
    let delay_ms = live_changes::delay_ms(s.get("DELAY")).unwrap_or(live_changes::DEFAULT_DELAY_MS);
    let buffer_ms = s.get("LAT_MS").parse().unwrap_or(10);
    let card = card.cloned();
    let start: audio::Start = Box::new(move |delay_ms| {
        let source = if src.is_empty() { card_audio_input(card.as_ref())? } else { src.clone() };
        let a = audio::Audio::start(&source, sink.as_deref(), delay_ms, buffer_ms)?;
        Ok(Box::new(a))
    });
    Some(audio::Loopback::new(start, delay_ms, live.clone(), audio::Say::desktop()))
}

/// The card's own audio input (card.rs `audio_input`), by its id: on
/// Windows not by name, which said only "USB Audio Device" for the
/// MS2130's audio function (the #18 VM run). Else SRC must name it.
fn card_audio_input(card: Option<&card::Node>) -> Result<String, String> {
    let input = card::audio_input(card)?;
    eprintln!("audio: the card's input: {}", input.name);
    Ok(input.id)
}

/// The setup wizard (#13), in the viewer window: `--setup`, or a first run
/// (setup.rs). True when the feed should start.
fn setup(first_run: bool, quit: &run::Quit) -> Result<setup::Ended, String> {
    let s = Settings::resolve();
    let desktop = desktop::detect()?;
    // The wizard's feed and loopback start from the saved values; one that
    // is not valid falls back to its default, and says so (view.sh's run
    // refuses it; the wizard is where it gets fixed).
    let fallback = |name: &str, default: &str| eprintln!("warning: {name}={} is not valid; the wizard uses {default}", s.get(name));
    let buffer_ms = s.get("LAT_MS").parse().unwrap_or_else(|_| {
        fallback("LAT_MS", "10");
        10
    });
    let system = wizard::Machine {
        sink: s.get("SINK").to_string(),
        buffer_ms,
        // Linux only, as the Windows adapter had it: DESKTOP=hyprland is
        // read on Windows too.
        refresh: cfg!(target_os = "linux") && matches!(desktop, desktop::Desktop::Hyprland(_)),
    };
    // The feed in the wizard's window: the saved picture settings, or the
    // defaults where they are not valid; windowed, the display left alone.
    let (live_changes, warnings) = live_changes::LiveChanges::read_picture_lenient(&s);
    for w in warnings {
        eprintln!("warning: the wizard's feed: {w}");
    }
    let live_changes = live_changes.with_window(app::WindowMode::Windowed { width: 1280, height: 720 }, (1280, 720));
    let start = setup::Start {
        run_dir: session::run_dir(),
        system: Box::new(system),
        config_file: s.config_file.clone(),
        first_run,
        live_changes,
        desktop,
    };
    let parts = setup::Parts { window: Box::new(app::run), open_card: Box::new(card::open) };
    setup::run(start, parts, quit)
}

/// Why the viewer ends in error. The saved card not plugged in is told
/// apart (#39): a console held open offers the setup for it.
#[derive(Debug)]
enum Fatal {
    CardMissing(card::Missing),
    Other(String),
}

impl From<String> for Fatal {
    fn from(e: String) -> Self {
        Fatal::Other(e)
    }
}

impl From<&str> for Fatal {
    fn from(e: &str) -> Self {
        Fatal::Other(e.to_string())
    }
}

impl From<card::PickError> for Fatal {
    fn from(e: card::PickError) -> Self {
        match e {
            card::PickError::Missing(m) => Fatal::CardMissing(m),
            card::PickError::Other(e) => Fatal::Other(e),
        }
    }
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fatal::CardMissing(m) => m.fmt(f),
            Fatal::Other(e) => f.write_str(e),
        }
    }
}

impl Fatal {
    /// The desktop notification's body (#28).
    fn notice(&self) -> String {
        match self {
            Fatal::CardMissing(m) => m.notice(),
            Fatal::Other(e) => e.clone(),
        }
    }
}

/// Where the pin in effect came from, as the missing card's message says
/// it (#39): CARD_SERIAL, else CARD_PORT.
fn pin_source(s: &Settings) -> String {
    let pin = if s.get("CARD_SERIAL").is_empty() { "CARD_PORT" } else { "CARD_SERIAL" };
    match s.from(pin) {
        settings::From::Env => "from the environment".into(),
        settings::From::Saved => format!("in {}", s.config_file.display()),
        settings::From::Menu => "from the menu's defaults".into(),
        settings::From::Default => "by default".into(),
    }
}

/// The card the settings pin, opened at `format` (card.rs `open`). On
/// Windows DEV may hold the device's friendly name or its symbolic link.
fn open_card(s: &Settings, format: capture::Format) -> Result<(Box<dyn capture::FrameSource>, card::Node), Fatal> {
    let source = pin_source(s);
    let pins = card::Pins {
        dev: s.get("DEV"),
        serial: s.get("CARD_SERIAL"),
        port: s.get("CARD_PORT"),
        ignore: s.get("IGNORE_CARDS_RE"),
        source: &source,
    };
    let picked = card::pick(&card::list(), &pins, card::describe_dev(pins.dev), &s.config_file.display().to_string())?;
    if let Some(w) = &picked.warning {
        eprintln!("warning: {w}");
    }
    eprintln!("video device: {} ({})", picked.node.path.display(), picked.node.card_type);
    Ok((card::open(&picked.node, format)?, picked.node))
}

/// The settings; with `menu`, a menu launch's defaults (`--menu`, #28).
fn settings(menu: bool) -> Settings {
    let s = Settings::resolve();
    if menu { s.with_menu_defaults() } else { s }
}

/// The viewer's own flags (USAGE): the measurement flags, the test
/// pattern, and --debug.
#[derive(Debug, Default, PartialEq)]
struct Args {
    seconds: Option<u64>,
    monitor: Option<String>,
    stats: bool,
    test_pattern: bool,
    /// As view.sh's: DEBUG=window, as if from the environment.
    debug: bool,
    /// --record NAME (dev builds): the clip's name.
    #[cfg(feature = "dev")]
    record: Option<String>,
    /// --replay FILE (dev builds).
    #[cfg(feature = "dev")]
    replay: Option<std::path::PathBuf>,
    /// --note TEXT (dev builds): kept in a recording's metadata.
    #[cfg(feature = "dev")]
    note: Option<String>,
    /// --framegen MODE (dev builds, #66), checked once the flags parse.
    #[cfg(feature = "dev")]
    framegen: Option<String>,
    /// --framegen-score CLIP and its flags (dev builds, #81).
    #[cfg(feature = "dev")]
    score: Score,
}

/// --framegen-score CLIP's flags (dev builds, #81).
#[cfg(feature = "dev")]
#[derive(Debug, Default, PartialEq)]
struct Score {
    clip: Option<std::path::PathBuf>,
    every: Option<usize>,
    worst: Option<usize>,
    out: Option<std::path::PathBuf>,
    hud: Option<String>,
}

/// None for anything USAGE does not allow: an unknown flag, a missing or
/// non-numeric value.
fn parse_args(args: &[String]) -> Option<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--seconds" => a.seconds = Some(it.next()?.parse().ok()?),
            "--monitor" => a.monitor = Some(it.next()?.clone()),
            "--stats" => a.stats = true,
            "--test-pattern" => a.test_pattern = true,
            "--debug" => a.debug = true,
            #[cfg(feature = "dev")]
            "--record" => a.record = Some(it.next()?.clone()),
            #[cfg(feature = "dev")]
            "--replay" => a.replay = Some(it.next()?.into()),
            #[cfg(feature = "dev")]
            "--note" => a.note = Some(it.next()?.clone()),
            #[cfg(feature = "dev")]
            "--framegen" => a.framegen = Some(it.next()?.clone()),
            #[cfg(feature = "dev")]
            "--framegen-score" => a.score.clip = Some(it.next()?.into()),
            #[cfg(feature = "dev")]
            "--every" => a.score.every = Some(it.next()?.parse().ok()?),
            #[cfg(feature = "dev")]
            "--worst" => a.score.worst = Some(it.next()?.parse().ok()?),
            #[cfg(feature = "dev")]
            "--out" => a.score.out = Some(it.next()?.into()),
            #[cfg(feature = "dev")]
            "--hud" => a.score.hud = Some(it.next()?.clone()),
            _ => return None,
        }
    }
    Some(a)
}

/// The dev flags that go together (#64, #79): --note with --record,
/// --record with --seconds, and not with --replay.
#[cfg(feature = "dev")]
fn record_flags(record: Option<&str>, note: Option<&str>, seconds: Option<u64>, replay: Option<&std::path::Path>) -> Result<(), String> {
    match (record, note, seconds, replay) {
        (None, Some(_), _, _) => Err("--note goes with --record".into()),
        (Some(_), _, None, _) => Err("--record needs --seconds N".into()),
        (Some(_), _, _, Some(_)) => Err("--record and --replay: one at a time".into()),
        _ => Ok(()),
    }
}

/// --framegen-score's flags (#81): the score's options, or None for a
/// viewer run. Its own flags go with it only, and it runs alone: no
/// window, no card, so no viewer flag either.
#[cfg(feature = "dev")]
fn score_flags(a: &Args) -> Result<Option<score::Options>, String> {
    let s = &a.score;
    let Some(clip) = &s.clip else {
        if s.every.is_some() || s.worst.is_some() || s.out.is_some() || s.hud.is_some() {
            return Err("--every, --worst, --out and --hud go with --framegen-score".into());
        }
        return Ok(None);
    };
    let viewer = a.seconds.is_some()
        || a.monitor.is_some()
        || a.stats
        || a.test_pattern
        || a.debug
        || a.record.is_some()
        || a.replay.is_some()
        || a.note.is_some();
    if viewer {
        return Err("--framegen-score runs alone, headless: no viewer flag with it".into());
    }
    let mode = a.framegen.as_deref().ok_or("--framegen-score needs --framegen MODE")?;
    Ok(Some(score::Options {
        clip: clip.clone(),
        mode: framegen::Mode::from_flag(mode)?,
        every: s.every.unwrap_or(1),
        worst: s.worst.unwrap_or(score::WORST),
        out: s.out.clone(),
        hud: s.hud.clone(),
    }))
}

/// The frame generation a viewer starts with: a dev build's `--framegen`
/// for this run (`flag`), else FRAMEGEN (#84).
fn start_framegen(flag: Option<framegen::Mode>, live: &live_changes::LiveChanges) -> Option<framegen::Mode> {
    flag.or(live.framegen())
}

fn view(args: &[String], quit: &run::Quit, menu: bool) -> Result<(), Fatal> {
    // First run: no settings file, so the wizard, as view.sh offers it
    // (not for a measurement run, nor for the wizard's own children).
    let unset = |k: &str| env::var_os(k).is_none_or(|v| v.is_empty());
    if args.is_empty()
        && !settings::config_file(&|k| env::var(k).ok()).exists()
        && unset("PROTOHDMI_SETUP_CHILD")
        && unset("NO_SETUP")
        && !setup(true, quit)?.play
    {
        return Ok(());
    }

    // The viewer's numbers, from here on: the dashboard serves these. The
    // wizard's feed windows (--setup, or the first run above) had their own.
    let parsed = parse_args(args).ok_or(USAGE)?;
    // The frame-generation score (#81): headless, and nothing else.
    #[cfg(feature = "dev")]
    if let Some(opts) = score_flags(&parsed)? {
        return Ok(score::run(&opts).map(drop)?);
    }
    let Args {
        seconds,
        monitor,
        stats,
        test_pattern,
        debug,
        #[cfg(feature = "dev")]
        record,
        #[cfg(feature = "dev")]
        replay,
        #[cfg(feature = "dev")]
        note,
        #[cfg(feature = "dev")]
        framegen,
        // --framegen-score's flags, handled above.
        ..
    } = parsed;
    let mut opts =
        app::Options { seconds, monitor, stats, ..app::Options::new(live_changes::LiveChanges::DEFAULT, Arc::new(dashboard::Live::new())) };
    #[cfg(feature = "dev")]
    {
        opts.framegen = framegen.as_deref().map(framegen::Mode::from_flag).transpose()?;
    }

    let s = if debug {
        Settings::resolve_with(&|k| if k == "DEBUG" { Some("window".into()) } else { env::var(k).ok() })
    } else {
        Settings::resolve()
    };
    let s = if menu { s.with_menu_defaults() } else { s };
    for n in &s.notes {
        eprintln!("note: {n}");
    }
    let errors = s.errors();
    if !errors.is_empty() {
        return Err(format!("settings:\n  {}", errors.join("\n  ")).into());
    }
    for n in view_sh_only::check(&s)? {
        eprintln!("note: {n}");
    }
    opts.live_changes = live_changes::LiveChanges::read(&s)?;
    opts.framegen = start_framegen(opts.framegen, &opts.live_changes);
    let format = capture::Format::from_settings(s.get("RES"), s.get("FPS"), s.get("FMT"))?;
    // Dev builds (#64, #79): a clip recorded while the window shows the
    // feed, or a clip replayed at its own format.
    #[cfg(feature = "dev")]
    record_flags(record.as_deref(), note.as_deref(), seconds, replay.as_deref())?;
    // The card's name for the clip's metadata, once the feed has opened it.
    #[cfg(feature = "dev")]
    let card_about: clip::CardAbout = Arc::default();
    #[cfg(feature = "dev")]
    let recorded = match record {
        Some(name) => {
            let settings = clip::SETTINGS.iter().map(|k| (k.to_string(), s.get(k).to_string())).collect();
            let mut about = clip::About { note, settings, ..Default::default() };
            if test_pattern {
                about.card = "the test pattern".into();
            }
            let seconds = seconds.expect("record_flags checked it");
            let (recording, result) =
                clip::Recording::start(std::path::Path::new(clip::DIR), &name, seconds, format, about, card_about.clone())?;
            opts.tap = Some(Box::new(recording));
            Some(result)
        }
        None => None,
    };
    #[cfg(feature = "dev")]
    let (replay, format) = match replay {
        Some(path) => {
            let r = clip::Replay::open(&path)?;
            let f = capture::FrameSource::format(&r);
            (Some(r), f)
        }
        None => (None, format),
    };

    // One video window at a time, with view.sh too (docs/run-dir.md): the
    // feed's start and end, in their order (feed.rs).
    opts.desktop = desktop::detect()?;
    let start = feed::Start {
        run_dir: session::run_dir(),
        settings: s,
        opts,
        format,
        test_pattern,
        #[cfg(feature = "dev")]
        replay,
    };
    #[cfg(feature = "dev")]
    let open_card: feed::OpenCard = Box::new(move |s, f| {
        let (source, node) = open_card(s, f)?;
        *card_about.lock().expect("the card's lock is never poisoned") = Some((node.card_type.clone(), card::usb(&node).map(|u| u.id)));
        Ok((source, node))
    });
    #[cfg(not(feature = "dev"))]
    let open_card: feed::OpenCard = Box::new(open_card);
    let parts =
        feed::Parts { open_card, loopback: Box::new(audio), debug_log: Box::new(debuglog::DebugLog::start), window: Box::new(app::run) };
    let ran = feed::run(start, parts, quit)?;
    // A recording that failed fails the run (the window has closed).
    #[cfg(feature = "dev")]
    if let Some(Some(Err(e))) = recorded.map(|r| r.lock().expect("the recording's result lock is never poisoned").take()) {
        return Err(format!("recording: {e}").into());
    }
    let running = match ran {
        feed::Ran::Ended => return Ok(()),
        // u in the overlay: the feed has let go of the card, so the
        // wizard can open it. The feed comes back with what it saved, or,
        // closed unsaved, with the settings it had; a save then "close",
        // or a quit, ends the viewer. The viewer's flags do not carry over.
        feed::Ran::Setup => {
            let ended = setup(false, quit)?;
            return if ended.play || (!ended.saved && !quit.requested()) { view(&[], quit, menu) } else { Ok(()) };
        }
        feed::Ran::AlreadyRunning(running) => running,
    };
    let pid = running.pid.map_or("unknown".to_string(), |p| p.to_string());
    if running.focused {
        eprintln!("already running (pid {pid}); focused it");
    } else {
        eprintln!("already running (pid {pid})");
        // Started from a launcher: say so on the desktop, as view.sh.
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            desktop::notify("normal", "protoHDMIcapture is already running", "");
        }
    }
    Ok(())
}

/// Whether a fatal error waits for Enter before the viewer ends (#38): a
/// console with this process alone on it was made for it (started from
/// Explorer or a shortcut) and closes when it exits, taking the error
/// with it. Not with a terminal that started it (more processes share the
/// console), nor with stdin redirected (nobody can press Enter).
#[cfg(any(windows, test))]
fn holds_console(processes: u32, stdin_is_console: bool) -> bool {
    processes == 1 && stdin_is_console
}

/// What a held console asks: with the setup offered (the saved card not
/// plugged in, #39), S runs it; Enter closes.
#[cfg(any(windows, test))]
fn hold_prompt(offer_setup: bool) -> &'static str {
    if offer_setup { "Press S to run the setup, Enter to close." } else { "Press Enter to close." }
}

/// A held console's answer to a key.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum Held {
    /// Not an answer: keep waiting.
    Wait,
    Close,
    Setup,
}

/// The answer to `hold_prompt` from one key press, by its virtual-key
/// code, at once (no Enter after it, #39): S runs the setup where it was
/// offered; Enter or Esc closes; any other key, S included where the
/// setup is not offered, is not an answer.
#[cfg(any(windows, test))]
fn on_key(virtual_key: u16, offer_setup: bool) -> Held {
    // VK_RETURN, VK_ESCAPE, and the S key (either case: the key, not the
    // character).
    const ENTER: u16 = 0x0D;
    const ESC: u16 = 0x1B;
    const S: u16 = b'S' as u16;
    match virtual_key {
        ENTER | ESC => Held::Close,
        S if offer_setup => Held::Setup,
        _ => Held::Wait,
    }
}

/// After a fatal error is printed: on Windows, a console that would close
/// with the viewer stays open until Enter or Esc (`holds_console`), or S
/// where the setup is offered. True: run the setup. #28's desktop
/// notification never fires there: stderr is a console.
#[cfg(windows)]
fn hold_console(offer_setup: bool) -> bool {
    use std::io::IsTerminal;
    let mut pids = [0u32; 2];
    // Safety: a buffer it fills up to its length; the count is all the
    // processes on the console, even past the buffer. 0 is a failure.
    let processes = unsafe { windows::Win32::System::Console::GetConsoleProcessList(&mut pids) };
    if processes == 0 {
        eprintln!("warning: the console's processes: {}", std::io::Error::last_os_error());
        return false;
    }
    if !holds_console(processes, std::io::stdin().is_terminal()) {
        return false;
    }
    eprintln!("{}", hold_prompt(offer_setup));
    match read_key_answer(offer_setup) {
        Ok(answer) => answer == Held::Setup,
        Err(e) => {
            eprintln!("warning: waiting for a key: {e}");
            false
        }
    }
}

/// Key presses from the console's input until one answers (`on_key`).
/// Keys pressed before the prompt are dropped first, so a key typed while
/// the feed ran does not answer it.
#[cfg(windows)]
fn read_key_answer(offer_setup: bool) -> windows::core::Result<Held> {
    use windows::Win32::System::Console::{
        FlushConsoleInputBuffer, GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
    };
    // Safety: this process's own console input handle; read, not closed.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }?;
    unsafe { FlushConsoleInputBuffer(input) }?;
    loop {
        let mut records = [INPUT_RECORD::default()];
        let mut read = 0u32;
        // Safety: a one-record buffer and an out-count; blocks for input.
        unsafe { ReadConsoleInputW(input, &mut records, &mut read) }?;
        let r = &records[0];
        if read == 0 || u32::from(r.EventType) != KEY_EVENT {
            continue;
        }
        // Safety: a KEY_EVENT record's union holds its key event.
        let key = unsafe { r.Event.KeyEvent };
        if !key.bKeyDown.as_bool() {
            continue;
        }
        match on_key(key.wVirtualKeyCode, offer_setup) {
            Held::Wait => {}
            answer => return Ok(answer),
        }
    }
}

/// No console is held on Linux: a terminal or a notification has the error.
#[cfg(not(windows))]
fn hold_console(_offer_setup: bool) -> bool {
    false
}

/// `--setup`: the wizard, then the feed if it was saved and asked for.
fn setup_then_view(quit: &run::Quit, menu: bool) -> Result<(), Fatal> {
    if setup(false, quit)?.play { view(&[], quit, menu) } else { Ok(()) }
}

/// `--version`: the release's version, which release-please sets in
/// Cargo.toml.
fn version() -> String {
    format!("protohdmicapture {}", env!("CARGO_PKG_VERSION"))
}

fn main() -> ExitCode {
    environment();
    let mut args: Vec<String> = env::args().skip(1).collect();
    // --menu: started from the desktop entry, which has nowhere to type a
    // setting (launch.sh's defaults, #28); with any other flag.
    let menu = args.iter().any(|a| a == "--menu");
    args.retain(|a| a != "--menu");
    if let [a] = args.as_slice()
        && (a == "-h" || a == "--help")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if let [a] = args.as_slice()
        && (a == "-V" || a == "--version")
    {
        println!("{}", version());
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) == Some("--settings") {
        if args.len() > 1 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        // Each setting, its value and where it came from, then whether they
        // are all valid: the same output as `view.sh --settings`.
        let (text, code) = settings(menu).report();
        print!("{text}");
        return ExitCode::from(code as u8);
    }
    // The running viewer's numbers in this terminal (#16); like --settings,
    // it needs no config.
    if args.first().map(String::as_str) == Some("--dashboard") {
        if args.len() > 1 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        return match dashboard::client(&session::run_dir()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("protohdmicapture: {e}");
                ExitCode::FAILURE
            }
        };
    }
    // First: a signal from here on ends in the normal teardown.
    let quit = match run::Quit::install() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("protohdmicapture: {e}");
            desktop::Notice::Failed(e).send();
            hold_console(false);
            return ExitCode::FAILURE;
        }
    };
    let mut result = if args.first().map(String::as_str) == Some("--setup") {
        if args.len() > 1 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        setup_then_view(&quit, menu)
    } else {
        view(&args, &quit, menu)
    };
    loop {
        let e = match result {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e) => e,
        };
        eprintln!("protohdmicapture: {e}");
        // Started from a menu: nobody reads stderr (view.sh's die).
        desktop::Notice::Failed(e.notice()).send();
        // The saved card not plugged in: a held console offers the setup
        // (#39), and its end is said the same way.
        if !hold_console(matches!(e, Fatal::CardMissing(_))) {
            return ExitCode::FAILURE;
        }
        result = setup_then_view(&quit, menu);
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, Fatal, Held, Settings, card, hold_prompt, holds_console, live_changes, on_key, parse_args, start_framegen, version};

    #[test]
    fn the_version_is_the_crates() {
        let v = version();
        let number = v.strip_prefix("protohdmicapture ").expect("named");
        assert!(number.split('.').count() == 3 && number.split('.').all(|n| n.parse::<u32>().is_ok()), "{v}");
    }

    fn parse(args: &[&str]) -> Option<Args> {
        parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_viewers_flags_parse_as_usage_says() {
        assert_eq!(parse(&[]), Some(Args::default()));
        assert_eq!(
            parse(&["--seconds", "30", "--monitor", "DP-1", "--stats", "--test-pattern", "--debug"]),
            Some(Args {
                seconds: Some(30),
                monitor: Some("DP-1".into()),
                stats: true,
                test_pattern: true,
                debug: true,
                #[cfg(feature = "dev")]
                record: None,
                #[cfg(feature = "dev")]
                replay: None,
                #[cfg(feature = "dev")]
                note: None,
                #[cfg(feature = "dev")]
                framegen: None,
                #[cfg(feature = "dev")]
                score: Default::default(),
            })
        );
        // The last of a repeated flag wins.
        assert_eq!(parse(&["--seconds", "1", "--seconds", "2"]).unwrap().seconds, Some(2));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn a_dev_build_takes_record_and_replay() {
        let a = parse(&["--record", "gta-still", "--seconds", "20"]).unwrap();
        assert_eq!((a.record.as_deref(), a.seconds), (Some("gta-still"), Some(20)));
        assert_eq!(parse(&["--replay", "/x/a.clip"]).unwrap().replay, Some("/x/a.clip".into()));
        assert_eq!(parse(&["--record"]), None);
        assert_eq!(parse(&["--record", "a", "--note", "PS5 Pro"]).unwrap().note.as_deref(), Some("PS5 Pro"));
        assert!(super::USAGE.contains("--record NAME") && super::USAGE.contains("--replay FILE"));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn records_flags_go_together() {
        use std::path::Path;
        let ok = |r, n, sec, rep: Option<&str>| super::record_flags(r, n, sec, rep.map(Path::new));
        assert_eq!(ok(Some("a"), None, Some(20), None), Ok(()));
        assert_eq!(ok(Some("a"), Some("PS5"), Some(20), None), Ok(()));
        assert_eq!(ok(None, None, None, Some("/x/a.clip")), Ok(()));
        assert_eq!(ok(None, Some("PS5"), None, None), Err("--note goes with --record".into()));
        assert_eq!(ok(Some("a"), None, None, None), Err("--record needs --seconds N".into()));
        assert_eq!(ok(Some("a"), None, Some(20), Some("/x/a.clip")), Err("--record and --replay: one at a time".into()));
        // A window records now: --framegen goes with it (#79), and the help says so.
        assert!(super::USAGE.contains("window shows them"));
        assert!(!super::USAGE.contains("no window"));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn the_score_runs_alone_with_its_own_flags() {
        let score = |args: &[&str]| super::score_flags(&parse(args).unwrap());
        // A viewer run: no score.
        assert_eq!(score(&["--framegen", "copy"]), Ok(None));
        let o = score(&["--framegen-score", "/c/a.clip", "--framegen", "copy", "--every", "3", "--out", "/o", "--hud", "1,2,3,4"])
            .unwrap()
            .unwrap();
        assert_eq!((o.clip.to_str(), o.mode, o.every, o.worst), (Some("/c/a.clip"), crate::framegen::Mode::Copy, 3, crate::score::WORST));
        assert_eq!((o.out.as_deref().and_then(|p| p.to_str()), o.hud.as_deref()), (Some("/o"), Some("1,2,3,4")));
        assert_eq!(score(&["--framegen-score", "/c/a.clip"]), Err("--framegen-score needs --framegen MODE".into()));
        for viewer in [&["--stats"][..], &["--seconds", "5"], &["--replay", "/x"], &["--monitor", "DP-1"], &["--record", "n"]] {
            let mut args = vec!["--framegen-score", "/c/a.clip", "--framegen", "copy"];
            args.extend_from_slice(viewer);
            assert_eq!(score(&args), Err("--framegen-score runs alone, headless: no viewer flag with it".into()), "{viewer:?}");
        }
        for own in [&["--every", "2"][..], &["--worst", "1"], &["--out", "/o"], &["--hud", "1,2,3,4"]] {
            assert_eq!(score(own), Err("--every, --worst, --out and --hud go with --framegen-score".into()), "{own:?}");
        }
        assert_eq!(parse(&["--every", "two"]), None);
        assert!(super::USAGE.contains("--framegen-score CLIP"));
    }

    /// Every copyright notice in the source (code or data ported from
    /// another project, #84) is in THIRD-PARTY-NOTICES.md, which the
    /// release archive and the installer ship (packaging/).
    #[test]
    fn every_ported_notice_ships_in_the_third_party_notices() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let notices = std::fs::read_to_string(root.join("THIRD-PARTY-NOTICES.md")).unwrap();
        let shipped: Vec<&str> = notices.lines().filter(|l| l.starts_with("Copyright")).collect();
        let mut dirs = vec![root.join("src")];
        let mut found = 0;
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                // Text files only: the weights are binary.
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for line in text.lines().filter(|l| l.contains("Copyright")) {
                    // The holder, after "Copyright (c) YEAR" or "Copyright(c) YEAR".
                    let Some(at) = line.find("Copyright") else { continue };
                    let words: Vec<&str> = line[at..].split_whitespace().collect();
                    let Some(year) = words.iter().position(|w| w.trim_matches(|c: char| !c.is_ascii_digit()).len() == 4) else {
                        continue;
                    };
                    // Up to the first comma ("hzwer, MIT"; "Devices, Inc.").
                    let mut holder = Vec::new();
                    for w in &words[year + 1..] {
                        holder.push(w.trim_end_matches(','));
                        if w.ends_with(',') {
                            break;
                        }
                    }
                    let holder = holder.join(" ");
                    let holder = holder.trim_end_matches('.');
                    assert!(!holder.is_empty(), "{}: a notice with no holder: {line}", path.display());
                    // The year with it: AMD's 2021 (FSR 1) and 2024 (the SDK) are two.
                    let holder = format!("{} {holder}", words[year].trim_matches(|c: char| !c.is_ascii_digit()));
                    let holder = holder.as_str();
                    assert!(
                        shipped.iter().any(|l| l.contains(holder)),
                        "{}: {holder:?} has no notice in THIRD-PARTY-NOTICES.md",
                        path.display()
                    );
                    found += 1;
                }
            }
        }
        assert!(found >= 7, "the source's notices were read ({found})");
    }

    #[test]
    fn the_viewer_starts_with_framegen_unless_a_dev_flag_names_a_mode() {
        use crate::framegen::Mode;
        let live = |framegen: &'static str| {
            let s = Settings::resolve_with(&move |k| match k {
                "CONFIG_FILE" => Some("/nonexistent/protohdmicapture-test-config".into()),
                "FRAMEGEN" => Some(framegen.into()),
                _ => None,
            });
            live_changes::LiveChanges::read(&s).unwrap()
        };
        assert_eq!(start_framegen(None, &live("rife")), Some(Mode::Rife));
        assert_eq!(start_framegen(None, &live("off")), None);
        assert_eq!(start_framegen(Some(Mode::Blend), &live("rife")), Some(Mode::Blend), "--framegen for this run");
    }

    #[cfg(not(feature = "dev"))]
    #[test]
    fn a_release_build_has_no_dev_flags() {
        assert_eq!(parse(&["--record", "x", "--seconds", "1"]), None);
        assert_eq!(parse(&["--replay", "x"]), None);
        assert_eq!(parse(&["--framegen-score", "x"]), None);
        assert!(!super::USAGE.contains("--record"));
        assert!(!super::USAGE.contains("--framegen-score"));
    }

    #[test]
    fn a_flag_usage_does_not_allow_is_refused() {
        for bad in [&["--seconds"][..], &["--seconds", "ten"], &["--seconds", "-1"], &["--monitor"], &["--bogus"], &["DP-1"]] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_held_console_offers_the_setup_only_for_the_missing_card() {
        assert_eq!(hold_prompt(true), "Press S to run the setup, Enter to close.");
        assert_eq!(hold_prompt(false), "Press Enter to close.");
        // One key answers at once (#39: S waited for an Enter after it).
        let (s, enter, esc, q, shift, space) = (0x53, 0x0D, 0x1B, 0x51, 0x10, 0x20);
        assert_eq!(on_key(s, true), Held::Setup);
        assert_eq!(on_key(enter, true), Held::Close);
        assert_eq!(on_key(esc, true), Held::Close);
        // A stray key is not an answer: the console stays open.
        for k in [q, shift, space] {
            assert_eq!(on_key(k, true), Held::Wait, "{k:#x}");
            assert_eq!(on_key(k, false), Held::Wait, "{k:#x}");
        }
        // Not offered: S is a stray key, Enter and Esc still close.
        assert_eq!(on_key(s, false), Held::Wait);
        assert_eq!(on_key(enter, false), Held::Close);
        assert_eq!(on_key(esc, false), Held::Close);
        // The offer follows the error's kind, not its text.
        let missing = card::Missing { headline: "h".into(), details: vec![], setup: "s".into() };
        assert!(matches!(Fatal::from(card::PickError::Missing(missing)), Fatal::CardMissing(_)));
        let other = Fatal::from("the capture card with serial X is not plugged in".to_string());
        assert!(matches!(other, Fatal::Other(_)));
    }

    #[test]
    fn the_pin_is_said_where_it_came_from() {
        // #39: CARD_SERIAL from the environment was said to be in the file.
        let dir = std::env::temp_dir().join(format!("protohdmi-pin-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(&file, "CARD_SERIAL=20000130041415\nCARD_PORT=pci-b\n").unwrap();
        let config = file.display().to_string();
        let resolve = |env: &[(&str, &str)]| {
            let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            super::Settings::resolve_with(&|k| {
                if k == "CONFIG_FILE" {
                    return Some(config.clone());
                }
                env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
            })
        };
        assert_eq!(super::pin_source(&resolve(&[])), format!("in {config}"));
        assert_eq!(super::pin_source(&resolve(&[("CARD_SERIAL", "20000130041415")])), "from the environment");
        // No serial: the port is the pin, and its own source is said.
        assert_eq!(super::pin_source(&resolve(&[("CARD_SERIAL", ""), ("CARD_PORT", "pci-c")])), "from the environment");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fatal_error_holds_only_a_console_of_its_own() {
        // Started from Explorer or a shortcut: the console closes with it.
        assert!(holds_console(1, true));
        // A terminal started it: the terminal keeps the error.
        assert!(!holds_console(2, true));
        assert!(!holds_console(3, true));
        // Its own console, but stdin redirected: nobody can press Enter.
        assert!(!holds_console(1, false));
    }
}
