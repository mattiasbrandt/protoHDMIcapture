//! V4L2 (Linux): the card's buffers through linuxvideo, each with its
//! kernel timestamp and sequence number, `poll()` before every dequeue.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use linuxvideo::format::{PixFormat, PixelFormat};
use linuxvideo::stream::ReadStream;
use linuxvideo::{Device, Fract};

use super::{DECODE_REPORT_EVERY, Event, Format, Frame, FrameSource, NO_SIGNAL_AFTER, POLL_SLICE, Pixels, now_monotonic};
use crate::mjpeg;

impl Pixels {
    /// Its V4L2 fourcc.
    pub fn fourcc(self) -> PixelFormat {
        match self {
            Pixels::Yuyv => PixelFormat::YUYV,
            Pixels::Mjpeg => PixelFormat::MJPG,
        }
    }
}

/// V4L2 (Linux).
pub struct V4l2 {
    stream: ReadStream,
    format: Format,
    /// MJPEG: the decoder, and the frame it decodes into.
    mjpeg: Option<(mjpeg::Decoder, Vec<u8>)>,
}

impl V4l2 {
    pub fn open(node: &Path, want: Format) -> Result<Self, String> {
        let at = |e: io::Error| format!("{}: {e}", node.display());
        let device = Device::open(node).map_err(at)?;
        let pixel_format = want.pixels.fourcc();
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
                                on_event(Event::Undecodable { count: undecodable, why: e.to_string() });
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
