//! Capture: the MS2130 at YUYV 1080p60 through `linuxvideo`, with each
//! buffer's kernel timestamp and sequence number (the vendored patch), and a
//! `poll()` timeout before every dequeue so quit never waits on a frame.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use linuxvideo::format::{PixFormat, PixelFormat};
use linuxvideo::stream::ReadStream;
use linuxvideo::{CapabilityFlags, Device, Fract};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const FPS: u32 = 60;

/// How long one `poll()` waits: the longest quit can take with no frames.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// No frame for this long is "no frames".
pub const NO_FRAMES_AFTER: Duration = Duration::from_secs(1);

/// The card's file in `capture-cards/`: its `Ids:` line names the USB ids
/// to look for, so the node is found, never hardcoded (it moves on a replug).
const CARD_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../capture-cards/ms2130.md");

/// A dequeued frame, valid only inside the callback.
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub sequence: u32,
    /// Kernel capture time, `CLOCK_MONOTONIC`.
    pub captured: Duration,
    /// When the capture loop got it, `CLOCK_MONOTONIC`.
    pub dequeued: Duration,
}

/// What the capture loop reports.
pub enum Event<'a> {
    Frame(Frame<'a>),
    /// Sequence numbers jumped: `lost` frames never reached us.
    Gap { from: u32, to: u32, lost: u32 },
    /// No frame for `NO_FRAMES_AFTER`.
    NoFrames,
    /// Frames again after `NoFrames`.
    FramesBack,
}

pub fn now_monotonic() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // CLOCK_MONOTONIC cannot fail with a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The USB ids from the card file's `Ids:` line, as `vvvv:pppp`.
fn card_ids() -> io::Result<Vec<String>> {
    let text = fs::read_to_string(CARD_FILE)
        .map_err(|e| io::Error::new(e.kind(), format!("{CARD_FILE}: {e}")))?;
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("Ids:"))
        .ok_or_else(|| io::Error::other(format!("{CARD_FILE}: no Ids: line")))?;
    Ok(line.split_whitespace().map(str::to_lowercase).collect())
}

/// The USB `vvvv:pppp` of a video node, if it sits on USB.
pub fn usb_id(node: &Path) -> Option<String> {
    let name = node.file_name()?;
    // device -> the USB interface; its parent is the USB device.
    let dev = Path::new("/sys/class/video4linux").join(name).join("device/..");
    let read = |f: &str| fs::read_to_string(dev.join(f)).ok().map(|s| s.trim().to_lowercase());
    Some(format!("{}:{}", read("idVendor")?, read("idProduct")?))
}

/// The video capture node of the first MS2130 found.
pub fn find_card() -> io::Result<PathBuf> {
    find_card_node(CapabilityFlags::VIDEO_CAPTURE)
}

/// The first MS2130 node with capability `cap` (the card has a video
/// capture node and a metadata node).
pub fn find_card_node(cap: CapabilityFlags) -> io::Result<PathBuf> {
    let ids = card_ids()?;
    let mut nodes: Vec<PathBuf> = fs::read_dir("/dev")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("video")))
        .collect();
    nodes.sort();
    for node in nodes {
        let Some(id) = usb_id(&node) else { continue };
        if !ids.contains(&id) {
            continue;
        }
        let caps = Device::open(&node)?.capabilities()?.device_capabilities();
        if caps.contains(cap) {
            return Ok(node);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no MS2130 ({}) found", ids.join(" ")),
    ))
}

/// An open stream at YUYV 1920x1080 @ 60.
pub struct Capture {
    stream: ReadStream,
    pub frame_size: usize,
}

impl Capture {
    pub fn open(node: &Path) -> io::Result<Self> {
        let device = Device::open(node)?;
        let capture = device.video_capture(PixFormat::new(WIDTH, HEIGHT, PixelFormat::YUYV))?;
        let fmt = capture.format();
        if (fmt.width(), fmt.height(), fmt.pixel_format()) != (WIDTH, HEIGHT, PixelFormat::YUYV) {
            return Err(io::Error::other(format!(
                "the driver chose {}x{} {}, not {WIDTH}x{HEIGHT} YUYV",
                fmt.width(),
                fmt.height(),
                fmt.pixel_format()
            )));
        }
        if fmt.bytes_per_line() != WIDTH * 2 {
            return Err(io::Error::other(format!(
                "stride {} bytes, expected {}",
                fmt.bytes_per_line(),
                WIDTH * 2
            )));
        }
        let interval = capture.set_frame_interval(Fract::new(1, FPS))?;
        if (interval.numerator(), interval.denominator()) != (1, FPS) {
            return Err(io::Error::other(format!("the driver chose a frame interval of {interval}, not 1/{FPS}")));
        }
        let frame_size = (WIDTH * HEIGHT * 2) as usize;
        let stream = capture.into_stream()?;
        Ok(Self { stream, frame_size })
    }

    /// Delivers frames until `quit` is set. Returns within `POLL_SLICE` of
    /// it being set, frames or not. An error means the stream is unusable
    /// (the card is gone, a dequeue failed).
    pub fn run(
        &mut self,
        quit: &AtomicBool,
        mut on_event: impl FnMut(Event<'_>),
    ) -> io::Result<()> {
        let fd = self.stream.as_raw_fd();
        let mut last_seq: Option<u32> = None;
        let mut last_frame = now_monotonic();
        let mut no_frames = false;
        let mut warned_clock = false;
        let frame_size = self.frame_size;

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
                if !no_frames && now_monotonic() - last_frame >= NO_FRAMES_AFTER {
                    no_frames = true;
                    on_event(Event::NoFrames);
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
                if no_frames {
                    no_frames = false;
                    on_event(Event::FramesBack);
                }
                if !buf.is_timestamp_monotonic() && !warned_clock {
                    warned_clock = true;
                    eprintln!("warning: buffer timestamps are not CLOCK_MONOTONIC; latencies are meaningless");
                }
                let sequence = buf.sequence();
                if let Some(prev) = last_seq {
                    let step = sequence.wrapping_sub(prev);
                    if step != 1 {
                        on_event(Event::Gap { from: prev, to: sequence, lost: step.wrapping_sub(1) });
                    }
                }
                last_seq = Some(sequence);
                if buf.is_error() || buf.len() != frame_size {
                    eprintln!(
                        "frame {sequence}: {} bytes{}, skipped",
                        buf.len(),
                        if buf.is_error() { ", error flag" } else { "" }
                    );
                    return Ok(());
                }
                on_event(Event::Frame(Frame { data: &buf[..], sequence, captured: buf.timestamp(), dequeued }));
                Ok(())
            })?;
        }
        Ok(())
    }
}
