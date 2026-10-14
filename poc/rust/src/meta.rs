//! The card's metadata node, read alongside another player (mpv) that owns
//! the video node: uvcvideo gives each metadata buffer the sequence number
//! and kernel timestamp of the video frame it belongs to (`uvc_video.c`,
//! `uvc_video_next_buffers`), and completes it with that frame. So this
//! times another player's frames without touching it.
//!
//! One line per buffer: sequence, kernel timestamp (CLOCK_MONOTONIC ns),
//! dequeued here (CLOCK_MONOTONIC ns), dequeued here (CLOCK_REALTIME ns).
//! The realtime column ties these to logs stamped with the wall clock
//! (WAYLAND_DEBUG).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use linuxvideo::format::{MetaFormat, PixelFormat};
use linuxvideo::Device;

use crate::capture::now_monotonic;

fn now_realtime() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // CLOCK_REALTIME cannot fail with a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Logs every metadata buffer to `out` until `quit`. Returns the count.
pub fn run(node: &Path, quit: &AtomicBool, out: &mut impl Write) -> io::Result<u64> {
    let device = Device::open(node)?.meta_capture(MetaFormat::new(PixelFormat::UVC))?;
    let mut stream = device.into_stream()?;
    let fd = stream.as_raw_fd();
    let mut n = 0u64;
    writeln!(out, "# sequence kernel_mono_ns dequeue_mono_ns dequeue_real_ns")?;
    while !quit.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!("poll on the metadata stream: revents {:#x}", pfd.revents)));
        }
        let (mono, real) = (now_monotonic(), now_realtime());
        stream.dequeue(|buf| {
            // Once the video stream stops, the node hands back empty
            // buffers (sequence 0, timestamp 0).
            if buf.timestamp().is_zero() {
                return Ok(());
            }
            writeln!(
                out,
                "{} {} {} {}",
                buf.sequence(),
                buf.timestamp().as_nanos(),
                mono.as_nanos(),
                real.as_nanos()
            )
        })?;
        n += 1;
    }
    Ok(n)
}
