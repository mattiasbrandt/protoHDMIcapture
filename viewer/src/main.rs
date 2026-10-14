//! protoHDMIcapture's Rust viewer (#10): one program for capture, display
//! and audio. Opt-in beside view.sh until it has every feature (#10).

mod app;
mod capture;
mod card;
mod present;
mod settings;
mod stats;

use std::env;
use std::process::ExitCode;

use settings::Settings;

const USAGE: &str = "usage: protohdmicapture [--settings]
       measurements: [--seconds N] [--monitor NAME] [--stats]";

/// What view.sh does for mpv: MangoHud is an implicit Vulkan layer that a
/// session-wide MANGOHUD=1 injects into every Vulkan app, an overlay draw
/// in the latency path (#5). Called before any thread exists.
fn environment() {
    // Safety: first thing in main, before any thread.
    unsafe { env::set_var("MANGOHUD", "0") };
}

fn view(args: &[String]) -> Result<(), String> {
    let mut opts = app::Options { window: app::WindowMode::Fullscreen, monitor: None, seconds: None, stats: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => opts.seconds = Some(it.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            "--monitor" => opts.monitor = Some(it.next().ok_or(USAGE)?.clone()),
            "--stats" => opts.stats = true,
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
    let format = capture::Format::from_settings(s.get("RES"), s.get("FPS"), s.get("FMT"))?;

    let pins = card::Pins { dev: s.get("DEV"), serial: s.get("CARD_SERIAL"), port: s.get("CARD_PORT"), ignore: s.get("IGNORE_CARDS_RE") };
    let picked = card::pick(&card::list(), &pins, card::describe_dev(pins.dev), &s.config_file.display().to_string())?;
    if let Some(w) = &picked.warning {
        eprintln!("warning: {w}");
    }
    eprintln!("video device: {} ({})", picked.node.path.display(), picked.node.card_type);

    let source = capture::V4l2::open(&picked.node.path, format)?;
    app::run(Box::new(source), opts)
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
    match view(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("protohdmicapture: {e}");
            ExitCode::FAILURE
        }
    }
}
