//! protoHDMIcapture's Rust viewer (#10): one program for capture, display
//! and audio. Opt-in beside view.sh until it has every feature (#10).

mod app;
mod audio;
mod capture;
mod card;
mod gputime;
mod picture;
mod present;
mod pw;
mod scale;
mod screenshot;
mod settings;
mod stats;

use std::env;
use std::process::ExitCode;

use settings::Settings;

const USAGE: &str = "usage: protohdmicapture [--settings]
       measurements: [--seconds N] [--monitor NAME] [--stats]";

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
    match env::var("PIPEWIRE_PROPS") {
        Ok(v) if !v.contains("node.dont-fallback") => {
            eprintln!("warning: PIPEWIRE_PROPS is set without node.dont-fallback; the audio may fall back to the default source");
        }
        Ok(_) => {}
        // Safety: as above.
        Err(_) => unsafe { env::set_var("PIPEWIRE_PROPS", "{ node.dont-fallback = true }") },
    }
}

/// The audio loopback the settings ask for, or why there is none. A failed
/// start is reported and the feed goes on without sound, as in view.sh.
fn audio(s: &Settings, card: &card::Node) -> Option<audio::Audio> {
    match s.get("AUDIO") {
        "none" => return None,
        "mpv" => eprintln!("note: AUDIO=mpv means mpv's own audio in view.sh; the Rust viewer uses its loopback"),
        _ => {}
    }
    let source = match s.get("SRC") {
        "" => {
            let Some(usb) = card::usb_dir(&card.path) else {
                eprintln!("warning: no audio: the card is not on USB, so its audio input cannot be found; set SRC=<name from pactl list short sources>");
                return None;
            };
            let usb = usb.to_string_lossy();
            let found = pw::snapshot()
                .map_err(|e| eprintln!("warning: no audio: {e}"))
                .ok()?
                .card_source(usb.strip_prefix("/sys").unwrap_or(&usb));
            match found {
                Some(name) => name,
                None => {
                    eprintln!("warning: no audio: the card has no PipeWire source; set SRC=<name from pactl list short sources>");
                    return None;
                }
            }
        }
        src => src.to_string(),
    };
    let sink = Some(s.get("SINK")).filter(|v| !v.is_empty());
    // Validated: DELAY is 0..1 s, LAT_MS 1..1000.
    let delay_ms = (s.get("DELAY").parse::<f64>().unwrap_or(0.05) * 1000.0).round() as u32;
    let buffer_ms = s.get("LAT_MS").parse().unwrap_or(10);
    match audio::Audio::start(&source, sink, delay_ms, buffer_ms) {
        Ok(a) => {
            eprintln!("{}", a.describe());
            Some(a)
        }
        Err(e) => {
            eprintln!("warning: no audio: {e}");
            None
        }
    }
}

fn view(args: &[String]) -> Result<(), String> {
    let mut opts = app::Options {
        window: app::WindowMode::Fullscreen,
        monitor: None,
        seconds: None,
        stats: false,
        picture: picture::Picture::DEFAULT,
        scaling: scale::Scaling::Balanced,
        audio: None,
    };
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
    opts.picture = picture::Picture::from_settings(
        s.get("LEVELS"),
        s.get("CONTRAST"),
        s.get("BRIGHTNESS"),
        s.get("GAMMA"),
        s.get("SATURATION"),
    )?;
    opts.scaling = scale::Scaling::from_settings(s.get("SCALING"))?;
    let format = capture::Format::from_settings(s.get("RES"), s.get("FPS"), s.get("FMT"))?;

    let pins = card::Pins { dev: s.get("DEV"), serial: s.get("CARD_SERIAL"), port: s.get("CARD_PORT"), ignore: s.get("IGNORE_CARDS_RE") };
    let picked = card::pick(&card::list(), &pins, card::describe_dev(pins.dev), &s.config_file.display().to_string())?;
    if let Some(w) = &picked.warning {
        eprintln!("warning: {w}");
    }
    eprintln!("video device: {} ({})", picked.node.path.display(), picked.node.card_type);

    let source = capture::V4l2::open(&picked.node.path, format)?;
    let sound = audio(&s, &picked.node);
    opts.audio = sound.as_ref().map(audio::Audio::controls);
    let stats = opts.stats;
    let result = app::run(Box::new(source), opts);
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
    match view(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("protohdmicapture: {e}");
            ExitCode::FAILURE
        }
    }
}
