//! protoHDMIcapture Rust PoC (issue #5): a single-process viewer measured
//! against a latency budget. Linux only, throwaway.

mod capture;
mod display;
mod presentation;
mod stats;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use capture::{Capture, Event};

const USAGE: &str = "usage: hdmipoc capture [--device /dev/videoN] [--seconds N]\n       hdmipoc view [--device /dev/videoN] [--seconds N] [--windowed | --monitor NAME] [--mailbox]";

fn run_capture(args: &[String]) -> Result<(), String> {
    let mut device: Option<PathBuf> = None;
    let mut seconds: Option<u64> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = Some(it.next().ok_or(USAGE)?.into()),
            "--seconds" => seconds = Some(it.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            _ => return Err(USAGE.into()),
        }
    }
    let node = match device {
        Some(d) => d,
        None => capture::find_card().map_err(|e| e.to_string())?,
    };

    let quit = Arc::new(AtomicBool::new(false));
    let q = quit.clone();
    ctrlc::set_handler(move || q.store(true, Ordering::Relaxed)).map_err(|e| e.to_string())?;

    let mut cap = Capture::open(&node).map_err(|e| format!("{}: {e}", node.display()))?;
    eprintln!(
        "capture: {} YUYV {}x{} @ {}, poll slice 100 ms, no frames after {} ms",
        node.display(),
        capture::WIDTH,
        capture::HEIGHT,
        capture::FPS,
        capture::NO_FRAMES_AFTER.as_millis()
    );

    let start = capture::now_monotonic();
    if let Some(s) = seconds {
        let q = quit.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(s));
            q.store(true, Ordering::Relaxed);
        });
    }
    let mut frames: u64 = 0;
    let mut lost: u64 = 0;
    let mut gaps: u64 = 0;
    let mut first_seq: Option<u32> = None;
    let mut last_seq = 0u32;
    // Kernel capture timestamp -> dequeued by the capture loop.
    let mut to_dequeue: Vec<Duration> = Vec::with_capacity(64 * 1024);
    let mut window_frames = 0u64;
    let mut window_start = start;

    let result = cap.run(&quit, |e| {
        let t = (capture::now_monotonic() - start).as_secs_f64();
        match e {
            Event::Frame(f) => {
                frames += 1;
                window_frames += 1;
                first_seq.get_or_insert(f.sequence);
                last_seq = f.sequence;
                to_dequeue.push(f.dequeued.saturating_sub(f.captured));
                if f.dequeued - window_start >= Duration::from_secs(10) {
                    let secs = (f.dequeued - window_start).as_secs_f64();
                    eprintln!(
                        "{t:6.1} s  {:5.2} fps  frames {frames}  gaps {gaps} (lost {lost})",
                        window_frames as f64 / secs
                    );
                    window_frames = 0;
                    window_start = f.dequeued;
                }
            }
            // The card's first frames after stream-on come short and
            // flagged; a gap before the first good frame is start-up, not
            // a capture gap.
            Event::Gap { from, to, lost: n } if frames == 0 => {
                eprintln!("{t:6.1} s  start-up gap: sequence {from} -> {to}, {n} lost");
            }
            Event::Gap { from, to, lost: n } => {
                gaps += 1;
                lost += n as u64;
                eprintln!("{t:6.1} s  gap: sequence {from} -> {to}, {n} lost");
            }
            Event::NoFrames => eprintln!("{t:6.1} s  no frames"),
            Event::FramesBack => eprintln!("{t:6.1} s  frames again"),
        }
    });

    let elapsed = (capture::now_monotonic() - start).as_secs_f64();
    let span = first_seq.map_or(0, |f| last_seq.wrapping_sub(f) as u64 + 1);
    eprintln!("summary: {elapsed:.1} s, {frames} frames, sequence span {span}, {gaps} gaps, {lost} lost");
    match stats::percentiles(&mut to_dequeue) {
        // uvcvideo stamps a buffer when its first packet arrives
        // (V4L2_BUF_FLAG_TSTAMP_SRC_SOE); dequeue comes after the last, so
        // this is mostly the frame's USB transfer.
        Some((med, p99)) => eprintln!("summary: capture -> dequeue median {med:.2} ms, p99 {p99:.2} ms"),
        None => eprintln!("summary: no frames"),
    }
    result.map_err(|e| format!("capture: {e}"))
}

fn run_view(args: &[String]) -> Result<(), String> {
    let mut device: Option<PathBuf> = None;
    let mut opts = display::Options { windowed: false, seconds: None, mailbox: false, monitor: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = Some(it.next().ok_or(USAGE)?.into()),
            "--seconds" => opts.seconds = Some(it.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?),
            "--windowed" => opts.windowed = true,
            "--mailbox" => opts.mailbox = true,
            "--monitor" => opts.monitor = Some(it.next().ok_or(USAGE)?.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    let node = match device {
        Some(d) => d,
        None => capture::find_card().map_err(|e| e.to_string())?,
    };
    display::run(&node, opts)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("capture") => run_capture(&args[1..]),
        Some("view") => run_view(&args[1..]),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hdmipoc: {e}");
            ExitCode::FAILURE
        }
    }
}
