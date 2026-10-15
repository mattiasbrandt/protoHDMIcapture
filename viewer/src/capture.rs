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

/// What the card sends, from FMT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixels {
    Yuyv,
    /// JPEG frames, decoded to YUYV in the capture loop (#17).
    Mjpeg,
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
        let pixels = match fmt {
            "yuyv422" => Pixels::Yuyv,
            "mjpeg" => Pixels::Mjpeg,
            other => return Err(format!("FMT={other}: the Rust viewer captures yuyv422 and mjpeg only for now")),
        };
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
        assert!(Format::from_settings("1920x1080", "60", "nv12").unwrap_err().contains("yuyv422 and mjpeg"));
        assert!(Format::from_settings("1281x720", "60", "yuyv422").is_err());
    }
}
