//! Capture: the card's buffers, each with its capture time and a sequence
//! number or sample time, and a timeout on every wait so quit never waits on
//! a frame (#5). `FrameSource` is the platform seam: an adapter (V4L2 here,
//! Media Foundation on Windows, #9; the test pattern; the wizard's Idle)
//! only hands its buffers over. The frame policy (`policy`, #20) above it
//! decides the rest: what a picture is, the signal state, the gaps.

use std::io;
use std::time::Duration;

use crate::picture::Coding;

#[cfg(windows)]
pub mod mf;
mod policy;
#[cfg(windows)]
mod usb_hub;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(windows)]
pub use mf::MediaFoundation;
pub use policy::{Dropped, Event, Finder, Monotonic, Reason, Since, Signal, run, run_finding};
#[cfg(target_os = "linux")]
pub use v4l2::V4l2;

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

/// A buffer as an adapter dequeued it, lent for one call.
pub struct Raw<'a> {
    pub data: &'a [u8],
    /// When the card captured it, on `now_monotonic`'s clock: V4L2's
    /// kernel time stamp (first USB packet, TSTAMP_SRC_SOE), Media
    /// Foundation's device time stamp.
    pub captured: Duration,
    /// When the adapter got it, `now_monotonic`.
    pub dequeued: Duration,
    pub mark: Mark,
    /// The driver flagged it as bad (V4L2's ERROR flag).
    pub flagged: bool,
}

/// What a buffer's place in the stream is read from, for the gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// The driver's sequence number (V4L2).
    Sequence(u32),
    /// The sample time in 100 ns units, where there are no sequence
    /// numbers (Media Foundation).
    #[cfg_attr(not(windows), allow(dead_code))]
    Time(i64),
}

/// The platform seam for capture: an adapter.
pub trait FrameSource: Send {
    fn format(&self) -> Format;

    /// Bytes from one YUYV row to the next as the card lays them out,
    /// stated once, at open; the policy packs padded rows. Unused for MJPEG.
    fn stride(&self) -> usize {
        self.format().width as usize * 2
    }

    /// The stream, started on the capture thread (Media Foundation's
    /// objects stay on the thread that made them).
    fn start(&mut self) -> io::Result<Box<dyn Stream + '_>>;
}

/// A started adapter's buffers.
pub trait Stream {
    /// Waits up to `wait` for the next buffer and lends it to `take`;
    /// returns without one when `wait` runs out. An error means the source
    /// is unusable (the card is gone).
    fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()>;
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
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut count = 0i64;
    // Safety: a plain out-pointer; it does not fail on Windows XP or later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
    }
    qpc_duration(count)
}

/// A performance counter reading as `now_monotonic` gives it: DXGI's
/// frame statistics time vblanks on it (#19).
#[cfg(windows)]
pub fn qpc_duration(count: i64) -> Duration {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    let mut freq = 0i64;
    // Safety: as above.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    let (count, freq) = (count.max(0) as u128, freq.max(1) as u128);
    Duration::from_nanos((count * 1_000_000_000 / freq) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

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
