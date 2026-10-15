//! protoHDMIcapture's Rust viewer (#10): one program for capture, display
//! and audio. Opt-in beside view.sh until it has every feature (#10).
// On Windows (#18) the setup wizard, the Hyprland adapter, refresh
// matching and the presentation feedback are not wired up yet: their code
// is Linux's, compiled but unused there.
#![cfg_attr(windows, allow(dead_code))]

mod app;
mod audio;
mod capture;
mod capture_cards;
mod card;
mod dashboard;
mod desktop;
mod gputime;
mod keys;
mod level;
mod mjpeg;
mod osd;
mod overlay;
mod pattern;
mod picture;
mod present;
#[cfg(target_os = "linux")]
mod pw;
mod scale;
mod screenshot;
mod session;
mod settings;
mod sources;
mod stats;
mod wizard;

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use settings::Settings;

const USAGE: &str = "usage: protohdmicapture [--setup | --settings | --dashboard]
       measurements: [--seconds N] [--monitor NAME] [--stats] [--test-pattern]";

/// Called first thing in main, before any thread exists.
/// - MANGOHUD=0, as view.sh does for mpv: MangoHud is an implicit Vulkan
///   layer that a session-wide MANGOHUD=1 injects into every Vulkan app,
///   an overlay draw in the latency path (#5).
/// - node.dont-fallback for every PipeWire stream: a missing card audio
///   input must not become the microphone (#5, the Codex review).
///   libpipewire adds PIPEWIRE_PROPS to each new stream; WirePlumber
///   honours the key. An existing PIPEWIRE_PROPS is kept, with a warning.
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
}

/// The audio loopback the settings ask for: None for AUDIO=none, Err with
/// why it could not start. A failed start is reported and the feed goes on
/// without sound, as in view.sh.
/// `card`: None with `--test-pattern`, where only SRC can name an input.
fn audio(s: &Settings, card: Option<&card::Node>) -> Result<Option<audio::Audio>, String> {
    match s.get("AUDIO") {
        "none" => return Ok(None),
        "mpv" => eprintln!("note: AUDIO=mpv means mpv's own audio in view.sh; the Rust viewer uses its loopback"),
        _ => {}
    }
    let source = match s.get("SRC") {
        "" => card_audio_input(card)?,
        src => src.to_string(),
    };
    let sink = Some(s.get("SINK")).filter(|v| !v.is_empty());
    // Validated: DELAY is 0..1 s, LAT_MS 1..1000.
    let delay_ms = (s.get("DELAY").parse::<f64>().unwrap_or(0.05) * 1000.0).round() as u32;
    let buffer_ms = s.get("LAT_MS").parse().unwrap_or(10);
    let a = audio::Audio::start(&source, sink, delay_ms, buffer_ms)?;
    eprintln!("{}", a.describe());
    Ok(Some(a))
}

/// The card's own audio input, found as view.sh finds it: the PipeWire
/// source on the card's USB device.
#[cfg(target_os = "linux")]
fn card_audio_input(card: Option<&card::Node>) -> Result<String, String> {
    let card = card.ok_or("no card to find the audio input of (--test-pattern); set SRC=<name from pactl list short sources>")?;
    let usb = card::usb_dir(&card.path).ok_or(
        "the card is not on USB, so its audio input cannot be found; set SRC=<name from pactl list short sources>",
    )?;
    let usb = usb.to_string_lossy();
    pw::snapshot()?
        .card_source(usb.strip_prefix("/sys").unwrap_or(&usb))
        .ok_or_else(|| "the card has no PipeWire source; set SRC=<name from pactl list short sources>".to_string())
}

/// The card's own audio input on Windows (#18): the one input whose name
/// holds the card's (a UVC card's audio endpoint is "Digital Audio
/// Interface (<card>)"-like); else SRC must name it.
#[cfg(windows)]
fn card_audio_input(card: Option<&card::Node>) -> Result<String, String> {
    let inputs = audio::input_names()?;
    let set_src = || format!("set SRC to one of: {}", inputs.join(" | "));
    let card = card.ok_or_else(|| format!("no card to find the audio input of (--test-pattern); {}", set_src()))?;
    let matching: Vec<&String> = inputs.iter().filter(|n| n.contains(card.card_type.as_str())).collect();
    match matching.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!("no audio input is named after the card ({}); {}", card.card_type, set_src())),
        _ => Err(format!("more than one audio input is named after the card ({}); {}", card.card_type, set_src())),
    }
}

/// The setup wizard (#13), in the viewer window: `--setup`, or a first run.
/// Holds setup.lock as lib/setup.sh does (one wizard at a time) and leaves
/// setup.done. True when the feed should start: saved and asked for, or
/// the first run skipped.
#[cfg(target_os = "linux")]
fn setup(first_run: bool, quit: &app::Quit) -> Result<bool, String> {
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    use std::rc::Rc;
    use wizard::{Outcome, Phase, Wizard};

    let dir = session::run_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock = std::fs::File::create(dir.join("setup.lock")).map_err(|e| format!("setup.lock: {e}"))?;
    // Safety: flock on an fd this File owns.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another setup wizard is already running".into());
    }

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
    let sys = wizard::Linux {
        sink: s.get("SINK").to_string(),
        buffer_ms,
        refresh: matches!(desktop, desktop::Desktop::Hyprland(_)),
    };
    let wiz = Rc::new(RefCell::new(Wizard::new(Box::new(sys), s.config_file.clone(), first_run, &|k| env::var_os(k).is_some())?));
    // After every early return above, so none leaves it behind.
    std::fs::write(dir.join("setup.pid"), format!("{}\n", std::process::id())).map_err(|e| format!("setup.pid: {e}"))?;
    // The feed in the wizard's window: the saved picture settings, or the
    // defaults where they are not valid; windowed, the display left alone.
    let picture = picture::Picture::from_settings(s.get("LEVELS"), s.get("CONTRAST"), s.get("BRIGHTNESS"), s.get("GAMMA"), s.get("SATURATION"))
        .unwrap_or_else(|e| {
            eprintln!("warning: {e}; the wizard's feed uses the default picture");
            picture::Picture::DEFAULT
        });
    let scaling = scale::Scaling::from_settings(s.get("SCALING")).unwrap_or_else(|_| {
        fallback("SCALING", "balanced");
        scale::Scaling::Balanced
    });
    let opts = || app::Options {
        window: app::WindowMode::Windowed { width: 1280, height: 720 },
        monitor: None,
        seconds: None,
        stats: false,
        picture,
        scaling,
        audio: app::AudioSetup::Off("the wizard's".into()),
        window_size: (1280, 720),
        desktop: desktop::Desktop::Generic,
        source: String::new(),
        settings: None,
        wizard: Some(wiz.clone()),
        // Each feed window's numbers are its own, and nobody reads them.
        live: Arc::new(dashboard::Live::new()),
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
            match phase {
                Phase::Choose => app::run(Box::new(wizard::Idle), opts(), quit.clone())?,
                Phase::Feed => {
                    // One video window at a time, with view.sh too.
                    let mut session = match session::Session::start(&dir, desktop.clone())? {
                        Ok(s) => s,
                        Err(running) => {
                            let pid = running.pid.map_or(String::new(), |p| format!(" (pid {p})"));
                            wiz.borrow_mut().feed_failed(format!("a feed is running{pid}. Close it first."));
                            continue;
                        }
                    };
                    let Some((node, format)) = wiz.borrow().feed() else {
                        return Err("the wizard reached the feed without a card".into());
                    };
                    let source = match format.and_then(|f| capture::V4l2::open(&node.path, f)) {
                        Ok(source) => source,
                        Err(e) => {
                            let e = if e.contains("busy") { format!("the card is busy: a feed is probably running. Close it first. ({e})") } else { e };
                            wiz.borrow_mut().feed_failed(e);
                            continue;
                        }
                    };
                    session.announce()?;
                    // The card gone mid-check is the wizard's to say, not the end.
                    if let Err(e) = app::run(Box::new(source), opts(), quit.clone()) {
                        wiz.borrow_mut().feed_failed(e);
                        continue;
                    }
                }
            }
            // The window closed with the stage still wanting it: the window's
            // close button, or a signal.
            if !wiz.borrow().wants_exit() {
                wiz.borrow_mut().abandon();
            }
        }
    })();
    let saved = wiz.borrow().saved();
    // launch.sh reads a missing setup.done as "not saved".
    if let Err(e) = std::fs::write(dir.join("setup.done"), if saved { "saved\n" } else { "cancelled\n" }) {
        eprintln!("warning: setup.done: {e}; launch.sh will read the wizard as cancelled");
    }
    let _ = std::fs::remove_file(dir.join("setup.pid"));
    result?;
    Ok(wiz.borrow().outcome() == Some(Outcome::Play) && !quit.requested())
}

/// No setup wizard on Windows yet (#18): it reads V4L2, sysfs and PipeWire.
#[cfg(windows)]
fn setup(_: bool, _: &app::Quit) -> Result<bool, String> {
    Err(format!(
        "the setup wizard is not on Windows yet; write the settings by hand in {}",
        settings::config_file(&|k| env::var(k).ok()).display()
    ))
}

/// The card the settings pin, opened at `format`: V4L2 on Linux, Media
/// Foundation on Windows (#18), where DEV may hold the device's friendly
/// name or its symbolic link.
fn open_card(s: &Settings, format: capture::Format) -> Result<(Box<dyn capture::FrameSource>, card::Node), String> {
    let pins = card::Pins { dev: s.get("DEV"), serial: s.get("CARD_SERIAL"), port: s.get("CARD_PORT"), ignore: s.get("IGNORE_CARDS_RE") };
    let picked = card::pick(&card::list(), &pins, card::describe_dev(pins.dev), &s.config_file.display().to_string())?;
    if let Some(w) = &picked.warning {
        eprintln!("warning: {w}");
    }
    eprintln!("video device: {} ({})", picked.node.path.display(), picked.node.card_type);
    #[cfg(target_os = "linux")]
    let source = capture::V4l2::open(&picked.node.path, format)?;
    #[cfg(windows)]
    let source = capture::MediaFoundation::open(&picked.node.path, format)?;
    Ok((Box::new(source), picked.node))
}

fn view(args: &[String], quit: app::Quit) -> Result<(), String> {
    // First run: no settings file, so the wizard, as view.sh offers it
    // (not for a measurement run, nor for the wizard's own children). On
    // Windows there is no wizard yet: the defaults, said once.
    #[cfg(target_os = "linux")]
    let unset = |k: &str| env::var_os(k).is_none_or(|v| v.is_empty());
    #[cfg(windows)]
    if !settings::config_file(&|k| env::var(k).ok()).exists() {
        eprintln!("note: no settings file yet, so the defaults; the setup wizard is not on Windows yet");
    }
    #[cfg(target_os = "linux")]
    if args.is_empty()
        && !settings::config_file(&|k| env::var(k).ok()).exists()
        && unset("PROTOHDMI_SETUP_CHILD")
        && unset("NO_SETUP")
        && !setup(true, &quit)?
    {
        return Ok(());
    }

    // The viewer's numbers, from here on: the dashboard serves these. The
    // wizard's feed windows (--setup, or the first run above) had their own.
    let live = Arc::new(dashboard::Live::new());
    let mut opts = app::Options {
        window: app::WindowMode::Fullscreen,
        monitor: None,
        seconds: None,
        stats: false,
        picture: picture::Picture::DEFAULT,
        scaling: scale::Scaling::Balanced,
        audio: app::AudioSetup::Off(String::new()),
        window_size: (1280, 720),
        desktop: desktop::Desktop::Generic,
        source: String::new(),
        settings: None,
        wizard: None,
        live: live.clone(),
    };
    let mut test_pattern = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => opts.seconds = Some(it.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            "--monitor" => opts.monitor = Some(it.next().ok_or(USAGE)?.clone()),
            "--stats" => opts.stats = true,
            "--test-pattern" => test_pattern = true,
            _ => return Err(USAGE.into()),
        }
    }

    let s = Settings::resolve();
    for n in &s.notes {
        eprintln!("note: {n}");
    }
    let errors = s.errors();
    if !errors.is_empty() {
        return Err(format!("settings:\n  {}", errors.join("\n  ")));
    }
    opts.window = app::WindowMode::from_settings(s.get("WINDOW"), s.get("WINDOW_SIZE"))?;
    opts.window_size = app::WindowMode::from_settings("windowed", s.get("WINDOW_SIZE")).map(|m| match m {
        app::WindowMode::Windowed { width, height } => (width, height),
        _ => unreachable!("windowed is Windowed"),
    })?;
    opts.picture = picture::Picture::from_settings(
        s.get("LEVELS"),
        s.get("CONTRAST"),
        s.get("BRIGHTNESS"),
        s.get("GAMMA"),
        s.get("SATURATION"),
    )?;
    opts.scaling = scale::Scaling::from_settings(s.get("SCALING"))?;
    let format = capture::Format::from_settings(s.get("RES"), s.get("FPS"), s.get("FMT"))?;

    // One video window at a time, with view.sh too (docs/run-dir.md); on
    // Windows there is no view.sh to share the card with, and no session
    // files yet (#18).
    let desktop = desktop::detect()?;
    #[cfg(target_os = "linux")]
    let mut session = match session::Session::start(&session::run_dir(), desktop.clone())? {
        Ok(s) => s,
        Err(running) => {
            let pid = running.pid.map_or("unknown".to_string(), |p| p.to_string());
            if running.focused {
                eprintln!("already running (pid {pid}); focused it");
            } else {
                eprintln!("already running (pid {pid})");
                // Started from a launcher: say so on the desktop, as view.sh.
                if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                    desktop::notify("normal", "protoHDMIcapture is already running");
                }
            }
            return Ok(());
        }
    };
    opts.desktop = desktop;

    let (source, node): (Box<dyn capture::FrameSource>, _) = if test_pattern {
        eprintln!("video: the test pattern at {}x{} {} fps", format.width, format.height, format.fps);
        (Box::new(pattern::Pattern::new(format)), None)
    } else {
        let (source, node) = open_card(&s, format)?;
        (source, Some(node))
    };
    let sound = match audio(&s, node.as_ref()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("warning: no audio: {e}");
            live.set_audio(dashboard::AudioLive::Failed(e.clone()));
            opts.audio = app::AudioSetup::Failed(e);
            None
        }
    };
    match &sound {
        Some(a) => {
            opts.audio = app::AudioSetup::Running(a.controls());
            let (source, sink, buffer_ms) = a.link();
            live.set_audio(dashboard::AudioLive::Running {
                source: source.into(),
                sink: sink.into(),
                buffer_ms,
                counters: a.counters.clone(),
            });
        }
        None if s.get("AUDIO") == "none" => {
            opts.audio = app::AudioSetup::Off("none".into());
            live.set_audio(dashboard::AudioLive::Off("AUDIO=none".into()));
        }
        None => {}
    }
    opts.source = s.get("SOURCE").to_string();
    if !opts.source.is_empty() && sources::get(&opts.source).is_none() {
        eprintln!(
            "warning: SOURCE={} has no profile in sources/; the overlay shows the tips for any HDMI source",
            opts.source
        );
        opts.source.clear();
    }
    // As view.sh: after the audio, before the window.
    #[cfg(target_os = "linux")]
    if s.get("REFRESH") == "match" {
        session.match_refresh(format.fps);
    }
    #[cfg(windows)]
    if s.get("REFRESH") == "match" {
        eprintln!("note: REFRESH=match is not on Windows yet; the display's refresh rate is left as it is");
    }
    // DEBUG=window opens the dashboard with the feed, as in view.sh.
    let debug_window = s.get("DEBUG") == "window";
    opts.settings = Some(s);
    let stats = opts.stats;
    if quit.requested() {
        return Ok(());
    }
    // As view.sh writes it once its player is started.
    #[cfg(target_os = "linux")]
    session.announce()?;
    // The dashboard's socket, with the video lock held; a failure costs
    // only the dashboard.
    let _dashboard = match dashboard::Server::start(&dashboard::socket_path(&session::run_dir()), live, dashboard::TICK) {
        Ok(server) => {
            if debug_window && let Err(e) = dashboard::open(None) {
                eprintln!("warning: DEBUG=window: {e}");
            }
            Some(server)
        }
        Err(e) => {
            eprintln!("warning: no dashboard: {e}");
            None
        }
    };
    let result = app::run(source, opts, quit);
    if let (true, Some(a)) = (stats, &sound) {
        eprintln!("summary: audio {}", a.granted());
        eprintln!("summary: audio {}", a.status());
    }
    result
}

fn main() -> ExitCode {
    environment();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--settings") {
        if args.len() > 1 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        // Each setting, its value and where it came from, then whether they
        // are all valid: the same output as `view.sh --settings`.
        let (text, code) = Settings::resolve().report();
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
    let quit = match app::Quit::install() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("protohdmicapture: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = if args.first().map(String::as_str) == Some("--setup") {
        if args.len() > 1 {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        setup(false, &quit).and_then(|play| if play { view(&[], quit) } else { Ok(()) })
    } else {
        view(&args, quit)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("protohdmicapture: {e}");
            ExitCode::FAILURE
        }
    }
}
