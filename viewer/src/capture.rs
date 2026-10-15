//! Capture: frames from the card with each buffer's kernel timestamp and
//! sequence number, and a `poll()` timeout before every dequeue so quit never
//! waits on a frame (#5). `FrameSource` is the platform seam: V4L2 here,
//! Media Foundation on Windows (#9).

use std::io;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::picture::Coding;

#[cfg(windows)]
pub mod mf;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(windows)]
pub use mf::MediaFoundation;
#[cfg(target_os = "linux")]
pub use v4l2::V4l2;

/// How long one wait for a frame lasts: the longest quit can take with no
/// frames.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// How often a run of undecodable MJPEG frames is reported.
const DECODE_REPORT_EVERY: Duration = Duration::from_secs(5);
/// No frame for this long is "no signal" (GLOSSARY).
pub const NO_SIGNAL_AFTER: Duration = Duration::from_secs(1);

/// What the card sends, from FMT: the one list of the formats the Rust
/// viewer captures. The settings parse, the wizard's mode list and each
/// platform's format names come from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixels {
    Yuyv,
    /// JPEG frames, decoded to YUYV in the capture loop (#17).
    Mjpeg,
}

/// The FMT values lib/settings.sh accepts that the Rust viewer does not
/// capture, each with the reason. view.sh plays them.
pub const NOT_CAPTURED: [(&str, &str); 3] = [
    ("nv12", "not captured by the Rust viewer yet"),
    ("rawvideo", "lets the player pick the uncompressed format, and the Rust viewer has to know which"),
    ("", "leaves the card on the format it was last set to, and the Rust viewer has to know which"),
];

impl Pixels {
    /// Every captured format, uncompressed first.
    pub const ALL: [Pixels; 2] = [Pixels::Yuyv, Pixels::Mjpeg];

    /// Its FMT spelling.
    pub fn setting(self) -> &'static str {
        match self {
            Pixels::Yuyv => "yuyv422",
            Pixels::Mjpeg => "mjpeg",
        }
    }

    /// From its FMT spelling; a value that is not captured is refused with
    /// the reason.
    pub fn from_setting(fmt: &str) -> Result<Self, String> {
        if let Some(p) = Self::ALL.into_iter().find(|p| p.setting() == fmt) {
            return Ok(p);
        }
        let why = NOT_CAPTURED.iter().find(|(v, _)| *v == fmt).map_or("not an FMT value", |(_, why)| why);
        Err(format!("FMT={fmt}: {why}; use {}, or view.sh", Self::settings().join(" or ")))
    }

    /// Every captured format's FMT spelling, uncompressed first.
    pub fn settings() -> Vec<&'static str> {
        Self::ALL.iter().map(|p| p.setting()).collect()
    }
}

/// What the card is asked for, from RES, FPS and FMT.
#[derive(Debug, Clone, Copy)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub pixels: Pixels,
}

impl Format {
    /// RES, FPS and FMT as the settings hold them (already validated).
    pub fn from_settings(res: &str, fps: &str, fmt: &str) -> Result<Self, String> {
        let pixels = Pixels::from_setting(fmt)?;
        let (w, h) = res.split_once('x').ok_or(format!("RES={res} is not WIDTHxHEIGHT"))?;
        let width: u32 = w.parse().map_err(|_| format!("RES={res}: bad width"))?;
        let height: u32 = h.parse().map_err(|_| format!("RES={res}: bad height"))?;
        if !width.is_multiple_of(2) {
            // MJPEG too: its frames are decoded to YUYV.
            return Err(format!("RES={res}: YUYV needs an even width"));
        }
        let fps: f64 = fps.parse().map_err(|_| format!("FPS={fps} is not a number"))?;
        Ok(Self { width, height, fps, pixels })
    }

    /// How the frames' colour is to be read (picture.rs).
    pub fn coding(&self) -> Coding {
        match self.pixels {
            Pixels::Yuyv => Coding::Raw,
            Pixels::Mjpeg => Coding::Jpeg,
        }
    }

    /// Bytes in one YUYV frame (an MJPEG frame once decoded).
    pub fn frame_size(&self) -> usize {
        (self.width * self.height * 2) as usize
    }
}

/// A dequeued frame, valid only inside the callback.
pub struct Frame<'a> {
    pub data: &'a [u8],
    /// Kernel capture time (first USB packet, TSTAMP_SRC_SOE), CLOCK_MONOTONIC.
    pub captured: Duration,
    /// When the capture loop got it, CLOCK_MONOTONIC.
    pub dequeued: Duration,
}

/// What the capture loop reports.
pub enum Event<'a> {
    Frame(Frame<'a>),
    /// Sequence numbers jumped: `lost` frames never reached us.
    Gap { from: u32, to: u32, lost: u32 },
    /// No frame for `NO_SIGNAL_AFTER`.
    NoSignal,
    /// Frames again after `NoSignal`.
    SignalBack,
    /// MJPEG frames arrive but do not decode: `count` since the last
    /// report, at most one report per `DECODE_REPORT_EVERY`. Frames that
    /// arrive keep `NoSignal` away, but none reach the window, so this is
    /// what says why the window has no picture.
    Undecodable { count: u64, why: String },
}

/// The platform seam for capture.
pub trait FrameSource: Send {
    fn format(&self) -> Format;
    /// Delivers events until `quit` is set, returning within `POLL_SLICE`
    /// of it. An error means the source is unusable (the card is gone).
    fn run(&mut self, quit: &AtomicBool, on_event: &mut dyn FnMut(Event<'_>)) -> io::Result<()>;
}

/// Frames lost between two sample times (100 ns units) at `fps`, where a
/// source has times but no sequence numbers (Media Foundation): the
/// periods skipped when the step is over 1.5 periods.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn lost_between(prev: i64, now: i64, fps: f64) -> u32 {
    let period = 1e7 / fps.max(1.0);
    let step = (now - prev) as f64;
    if step > 1.5 * period { ((step / period).round() as u32).saturating_sub(1) } else { 0 }
}

/// The clock capture times are on: CLOCK_MONOTONIC, the kernel's buffer
/// timestamps' clock.
#[cfg(target_os = "linux")]
pub fn now_monotonic() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // CLOCK_MONOTONIC cannot fail with a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The clock capture times are on: the performance counter (QPC), which
/// Media Foundation's device timestamps are on too (in 100 ns units).
#[cfg(windows)]
pub fn now_monotonic() -> Duration {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut freq) = (0i64, 0i64);
    // Safety: plain out-pointers; neither call fails on Windows XP or later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    let (count, freq) = (count as u128, freq.max(1) as u128);
    Duration::from_nanos((count * 1_000_000_000 / freq) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_frames_from_sample_times() {
        let p = 166_667; // 60 fps in 100 ns units
        assert_eq!(lost_between(0, p, 60.0), 0);
        assert_eq!(lost_between(0, p * 14 / 10, 60.0), 0, "jitter under 1.5 periods");
        assert_eq!(lost_between(0, 2 * p, 60.0), 1);
        assert_eq!(lost_between(0, 5 * p, 60.0), 4);
    }

    #[test]
    fn formats_from_the_settings() {
        let f = Format::from_settings("1920x1080", "59.94", "yuyv422").unwrap();
        assert_eq!((f.width, f.height, f.frame_size(), f.pixels), (1920, 1080, 1920 * 1080 * 2, Pixels::Yuyv));
        assert!((f.fps - 59.94).abs() < 1e-9);
        assert_eq!(f.coding(), Coding::Raw);
        // An MJPEG frame is passed on decoded, so as big as a YUYV one.
        let f = Format::from_settings("1280x720", "60", "mjpeg").unwrap();
        assert_eq!((f.pixels, f.frame_size(), f.coding()), (Pixels::Mjpeg, 1280 * 720 * 2, Coding::Jpeg));
        assert!(Format::from_settings("1281x720", "60", "yuyv422").is_err());
    }

    #[test]
    fn a_format_not_captured_is_refused_with_the_reason() {
        assert_eq!(
            Format::from_settings("1920x1080", "60", "nv12").unwrap_err(),
            "FMT=nv12: not captured by the Rust viewer yet; use yuyv422 or mjpeg, or view.sh"
        );
        assert!(Format::from_settings("1920x1080", "60", "").unwrap_err().starts_with("FMT=: leaves the card on the format"));
        assert!(Format::from_settings("1920x1080", "60", "rawvideo").unwrap_err().starts_with("FMT=rawvideo: lets the player pick"));
        for p in Pixels::ALL {
            assert_eq!(Pixels::from_setting(p.setting()), Ok(p));
        }
    }

    /// Every FMT value lib/settings.sh accepts is captured or refused on
    /// purpose: a new one fails here until it is one or the other.
    #[test]
    fn every_fmt_value_is_captured_or_refused_on_purpose() {
        let row = crate::settings::rows().into_iter().find(|r| r.name == "FMT").expect("lib/settings.sh has FMT");
        let mut values: Vec<&str> = row.values.strip_prefix("enum:").expect("FMT is an enum").split(',').collect();
        if row.has_flag("empty") {
            values.push("");
        }
        for v in &values {
            let captured = Pixels::ALL.iter().any(|p| p.setting() == *v);
            let refused = NOT_CAPTURED.iter().any(|(n, _)| n == v);
            assert!(captured != refused, "FMT={v}: captured ({captured}) or on NOT_CAPTURED ({refused}), exactly one");
        }
        // And nothing is decided for a value the settings do not have.
        for v in Pixels::ALL.iter().map(|p| p.setting()).chain(NOT_CAPTURED.iter().map(|(n, _)| *n)) {
            assert!(values.contains(&v), "FMT={v} is not in lib/settings.sh");
        }
    }
}
