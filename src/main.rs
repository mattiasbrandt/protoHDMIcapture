//! protoHDMIcapture: one program for capture, display and audio.
// On Windows (#18, #19, #33) the Hyprland adapter, refresh matching and
// the presentation feedback are not wired up yet: their code is Linux's,
// compiled but unused there.
#![cfg_attr(windows, allow(dead_code))]

mod app;
mod audio;
mod capture;
mod capture_cards;
mod card;
mod dashboard;
mod debuglog;
mod desktop;
mod feed;
mod gputime;
mod keys;
mod level;
mod live_changes;
mod mjpeg;
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

const USAGE: &str = "usage: protohdmicapture [--setup | --settings | --dashboard | -h | --help | -V | --version] [--debug] [--menu]
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
  --test-pattern  colour bars instead of the card";

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
            _ => return None,
        }
    }
    Some(a)
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
    let Args { seconds, monitor, stats, test_pattern, debug } = parse_args(args).ok_or(USAGE)?;
    let mut opts =
        app::Options { seconds, monitor, stats, ..app::Options::new(live_changes::LiveChanges::DEFAULT, Arc::new(dashboard::Live::new())) };

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
    let format = capture::Format::from_settings(s.get("RES"), s.get("FPS"), s.get("FMT"))?;

    // One video window at a time, with view.sh too (docs/run-dir.md): the
    // feed's start and end, in their order (feed.rs).
    opts.desktop = desktop::detect()?;
    let start = feed::Start { run_dir: session::run_dir(), settings: s, opts, format, test_pattern };
    let parts = feed::Parts {
        open_card: Box::new(open_card),
        loopback: Box::new(audio),
        debug_log: Box::new(debuglog::DebugLog::start),
        window: Box::new(app::run),
    };
    let running = match feed::run(start, parts, quit)? {
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
    use super::{Args, Fatal, Held, card, hold_prompt, holds_console, on_key, parse_args, version};

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
            Some(Args { seconds: Some(30), monitor: Some("DP-1".into()), stats: true, test_pattern: true, debug: true })
        );
        // The last of a repeated flag wins.
        assert_eq!(parse(&["--seconds", "1", "--seconds", "2"]).unwrap().seconds, Some(2));
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
