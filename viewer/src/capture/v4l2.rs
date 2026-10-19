//! V4L2 (Linux): the card's buffers through linuxvideo, each with its
//! kernel timestamp and sequence number, `poll()` before every dequeue. The
//! frame policy (policy.rs) does the rest.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use linuxvideo::format::{PixFormat, PixelFormat};
use linuxvideo::stream::ReadStream;
use linuxvideo::{Device, Fract};

use super::{Format, FrameSource, Mark, Pixels, Raw, Stream, now_monotonic};

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
    /// Bytes per YUYV row.
    stride: usize,
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
        // A JPEG has no stride; padded YUYV rows are packed by the policy.
        if want.pixels == Pixels::Yuyv && fmt.bytes_per_line() < want.width * 2 {
            return Err(format!("{}: stride {} bytes, under a row's {}", node.display(), fmt.bytes_per_line(), want.width * 2));
        }
        let stride = fmt.bytes_per_line() as usize;
        // FPS may be fractional (59.94 = 60000/1001).
        let interval = capture
            .set_frame_interval(Fract::new(1000, (want.fps * 1000.0).round() as u32))
            .map_err(at)?;
        let got = interval.denominator() as f64 / interval.numerator() as f64;
        if (got - want.fps).abs() > want.fps * 0.005 {
            return Err(format!("{}: the card gives {got:.3} fps, not {} (FPS)", node.display(), want.fps));
        }
        let stream = capture.into_stream().map_err(at)?;
        Ok(Self { stream, format: want, stride })
    }

    /// The modes the device at `node` lists, in its own order: the formats
    /// this viewer captures, each discrete size and interval (as
    /// lib/setup.sh reads v4l2-ctl's list), the rate in frames per second.
    pub fn modes(node: &Path) -> Result<Vec<(Pixels, u32, u32, f64)>, String> {
        use linuxvideo::BufType;
        use linuxvideo::format::{FrameIntervals, FrameSizes};
        let dev = Device::open(node).map_err(|e| format!("{}: {e}", node.display()))?;
        let mut out = Vec::new();
        for desc in dev.formats(BufType::VIDEO_CAPTURE).flatten() {
            let pf = desc.pixel_format();
            // Only what the viewer captures: the wizard's `best_modes` drops the rest anyway.
            let Some(pixels) = Pixels::ALL.into_iter().find(|p| p.fourcc() == pf) else { continue };
            let Ok(FrameSizes::Discrete(sizes)) = dev.frame_sizes(pf) else { continue };
            for s in sizes {
                let Ok(FrameIntervals::Discrete(ivals)) = dev.frame_intervals(pf, s.width(), s.height()) else { continue };
                for i in ivals {
                    let f = i.fract();
                    if f.numerator() == 0 {
                        continue;
                    }
                    out.push((pixels, s.width(), s.height(), f64::from(f.denominator()) / f64::from(f.numerator())));
                }
            }
        }
        Ok(out)
    }
}

impl FrameSource for V4l2 {
    fn format(&self) -> Format {
        self.format
    }

    fn stride(&self) -> usize {
        self.stride
    }

    fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
        Ok(Box::new(Running { stream: &mut self.stream, warned_clock: false }))
    }
}

struct Running<'a> {
    stream: &'a mut ReadStream,
    warned_clock: bool,
}

impl Stream for Running<'_> {
    fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
        let fd = self.stream.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // Safety: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, wait.as_millis() as libc::c_int) };
        if n < 0 {
            let e = io::Error::last_os_error();
            // A signal cut the wait short: as a timeout.
            return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
        }
        if n == 0 {
            return Ok(());
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!("poll on the stream: revents {:#x} (the card is gone?)", pfd.revents)));
        }
        let warned_clock = &mut self.warned_clock;
        self.stream.dequeue(|buf| {
            let dequeued = now_monotonic();
            if !buf.is_timestamp_monotonic() && !*warned_clock {
                *warned_clock = true;
                eprintln!("warning: buffer timestamps are not CLOCK_MONOTONIC; latency figures are meaningless");
            }
            // The card's first buffers after stream-on come short and
            // flagged (#5): the policy drops them.
            take(Raw { data: &buf[..], captured: buf.timestamp(), dequeued, mark: Mark::Sequence(buf.sequence()), flagged: buf.is_error() });
            Ok(())
        })
        // The node gone under the dequeue: the card unplugged, as POLLERR above.
        .map_err(|e| if e.raw_os_error() == Some(libc::ENODEV) { io::Error::other(format!("dequeue: {e} (the card is gone?)")) } else { e })
    }
}
