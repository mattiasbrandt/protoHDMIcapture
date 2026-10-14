//! protoHDMIcapture's Rust viewer (#10): one program for capture, display
//! and audio. Opt-in beside view.sh until it has every feature (#10).

mod settings;

use std::env;
use std::process::ExitCode;

const USAGE: &str = "usage: protohdmicapture [--settings]";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        // Each setting, its value and where it came from, then whether they
        // are all valid: the same output as `view.sh --settings`.
        ["--settings"] => {
            let (text, code) = settings::Settings::resolve().report();
            print!("{text}");
            ExitCode::from(code as u8)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
