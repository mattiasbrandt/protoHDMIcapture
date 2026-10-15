//! Capture: frames from the card with each buffer's kernel timestamp and
//! sequence number, and a `poll()` timeout before every dequeue so quit never
//! waits on a frame (#5). `FrameSource` is the platform seam: V4L2 here,
//! Media Foundation on Windows (#9).

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use linuxvideo::format::{PixFormat, PixelFormat};
use linuxvideo::stream::ReadStream;
use linuxvideo::{Device, Fract};

use crate::mjpeg;
use crate::picture::Coding;

/// How long one `poll()` waits: the longest quit can take with no frames.
const POLL_SLICE: Duration = Duration::from_millis(100);
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
}

/// The platform seam for capture.
pub trait FrameSource: Send {
    fn format(&self) -> Format;
    /// Delivers events until `quit` is set, returning within `POLL_SLICE`
    /// of it. An error means the source is unusable (the card is gone).
    fn run(&mut self, quit: &AtomicBool, on_event: &mut dyn FnMut(Event<'_>)) -> io::Result<()>;
}

pub fn now_monotonic() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // CLOCK_MONOTONIC cannot fail with a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// V4L2 (Linux).
pub struct V4l2 {
    stream: ReadStream,
    format: Format,
    /// MJPEG: the decoder, and the frame it decodes into.
    mjpeg: Option<(mjpeg::Decoder, Vec<u8>)>,
}

/// How often a run of undecodable MJPEG frames is reported.
const DECODE_REPORT_EVERY: Duration = Duration::from_secs(5);

impl V4l2 {
    pub fn open(node: &Path, want: Format) -> Result<Self, String> {
        let at = |e: io::Error| format!("{}: {e}", node.display());
        let device = Device::open(node).map_err(at)?;
        let pixel_format = match want.pixels {
            Pixels::Yuyv => PixelFormat::YUYV,
            Pixels::Mjpeg => PixelFormat::MJPG,
        };
        let capture = device
            .video_capture(PixFormat::new(want.width, want.height, pixel_format))
            .map_err(at)?;
        let fmt = capture.format();
        if (fmt.width(), fmt.height(), fmt.pixel_format()) != (want.width, want.height, pixel_format) {
            return Err(format!(
                "{}: the card gives {}x{} {}, not {}x{} {} (RES/FMT)",
                node.display(),
                fmt.width(),
                fmt.height(),
                fmt.pixel_format(),
                want.width,
                want.height,
                pixel_format
            ));
        }
        // A JPEG has no stride.
        if want.pixels == Pixels::Yuyv && fmt.bytes_per_line() != want.width * 2 {
            return Err(format!("{}: stride {} bytes, expected {}", node.display(), fmt.bytes_per_line(), want.width * 2));
        }
        // FPS may be fractional (59.94 = 60000/1001).
        let interval = capture
            .set_frame_interval(Fract::new(1000, (want.fps * 1000.0).round() as u32))
            .map_err(at)?;
        let got = interval.denominator() as f64 / interval.numerator() as f64;
        if (got - want.fps).abs() > want.fps * 0.005 {
            return Err(format!("{}: the card gives {got:.3} fps, not {} (FPS)", node.display(), want.fps));
        }
        let stream = capture.into_stream().map_err(at)?;
        let mjpeg = (want.pixels == Pixels::Mjpeg)
            .then(|| (mjpeg::Decoder::new(want.width, want.height), vec![0; want.frame_size()]));
        Ok(Self { stream, format: want, mjpeg })
    }
}

impl FrameSource for V4l2 {
    fn format(&self) -> Format {
        self.format
    }

    fn run(&mut self, quit: &AtomicBool, on_event: &mut dyn FnMut(Event<'_>)) -> io::Result<()> {
        let fd = self.stream.as_raw_fd();
        let frame_size = self.format.frame_size();
        let mut last_seq: Option<u32> = None;
        let mut last_frame = now_monotonic();
        let mut no_signal = false;
        let mut warned_clock = false;
        let mjpeg = &mut self.mjpeg;
        // Undecodable MJPEG frames since the last report, and when that was.
        let mut undecodable = 0u64;
        let mut reported: Option<Duration> = None;

        while !quit.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, POLL_SLICE.as_millis() as libc::c_int) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                if !no_signal && now_monotonic() - last_frame >= NO_SIGNAL_AFTER {
                    no_signal = true;
                    on_event(Event::NoSignal);
                }
                continue;
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::other(format!(
                    "poll on the stream: revents {:#x} (the card is gone?)",
                    pfd.revents
                )));
            }
            self.stream.dequeue(|buf| {
                let dequeued = now_monotonic();
                last_frame = dequeued;
                if no_signal {
                    no_signal = false;
                    on_event(Event::SignalBack);
                }
                if !buf.is_timestamp_monotonic() && !warned_clock {
                    warned_clock = true;
                    eprintln!("warning: buffer timestamps are not CLOCK_MONOTONIC; latency figures are meaningless");
                }
                let sequence = buf.sequence();
                if let Some(prev) = last_seq {
                    let step = sequence.wrapping_sub(prev);
                    if step != 1 {
                        on_event(Event::Gap { from: prev, to: sequence, lost: step.wrapping_sub(1) });
                    }
                }
                last_seq = Some(sequence);
                // The card's first buffers after stream-on come short and
                // flagged (#5): dropped here.
                if buf.is_error() {
                    return Ok(());
                }
                let data = match mjpeg {
                    None if buf.len() != frame_size => return Ok(()),
                    None => &buf[..],
                    // Decoded here, on the capture thread, so the frame
                    // reaches the window as YUYV; a frame that does not
                    // decode is dropped, as a short one is.
                    Some((decoder, yuyv)) => match decoder.decode(&buf, yuyv) {
                        Ok(()) => &yuyv[..],
                        Err(e) => {
                            undecodable += 1;
                            if reported.is_none_or(|t| dequeued - t >= DECODE_REPORT_EVERY) {
                                eprintln!("warning: {undecodable} MJPEG frame(s) dropped, not decodable: {e}");
                                undecodable = 0;
                                reported = Some(dequeued);
                            }
                            return Ok(());
                        }
                    },
                };
                on_event(Event::Frame(Frame { data, captured: buf.timestamp(), dequeued }));
                Ok(())
            })?;
        }
        Ok(())
    }
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
        assert!(Format::from_settings("1920x1080", "60", "nv12").unwrap_err().contains("yuyv422 and mjpeg"));
        assert!(Format::from_settings("1281x720", "60", "yuyv422").is_err());
    }
}
