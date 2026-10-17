//! The debug log (#27): with DEBUG≠off, the viewer's output also goes to
//! `debug.log` in the run dir (docs/run-dir.md), as view.sh's `debug_tee`
//! (lib/debug.sh) does, so a run started from a menu leaves something to
//! attach to a bug report. On top of the output:
//!
//! - a `[dbg]` header: the version, the desktop and the log's path
//! - the settings, each with where it came from
//! - the format the card was opened at
//! - a perf line every `DEBUG_PERF_INTERVAL` s (default 5), from the run's
//!   dashboard numbers
//! - how the run ended, in the file only (main prints the error itself)
//!
//! The log is started once `video.lock` is held: a refused second launch
//! must not truncate the running session's log.
//!
//! The output is teed through a pipe per stream: on Linux the fd itself is
//! swapped (`dup2`); on Windows (#33) the standard handle (`SetStdHandle`),
//! which Rust's stdout and stderr read on every write. There only Rust's
//! own output is teed: what a C library writes through the C runtime's
//! descriptors still goes to the console alone.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capture::Format;
use crate::dashboard::Live;
use crate::desktop::Desktop;
use crate::settings::Settings;

/// The perf line's interval without `DEBUG_PERF_INTERVAL`, as view.sh's.
const PERF_EVERY: Duration = Duration::from_secs(5);
/// How long the end waits for the copies to drain. A child that inherited
/// stderr (a terminal opened by DEBUG=window, say) keeps the pipe open past
/// the viewer's end; its copy is then left to end with the process.
const DRAIN: Duration = Duration::from_millis(500);

/// The numbers a perf line reads from the run's `Live`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub frames: u64,
    pub presented: u64,
    pub repeated: u64,
    pub gaps: u64,
    pub lost: u64,
    pub undecodable: u64,
    pub unusable: u64,
}

/// A running debug log. Dropped, it puts the output back as it was.
pub struct DebugLog {
    path: PathBuf,
    file: Arc<Mutex<Option<File>>>,
    /// Each teed stream with its original, to put back.
    teed: Vec<sys::Restore>,
    /// The stream the `[dbg]` lines go to (stderr, teed).
    out: sys::Stream,
    copies: Vec<(JoinHandle<()>, mpsc::Receiver<()>)>,
    perf: Option<(Sender<()>, JoinHandle<()>)>,
}

/// `DEBUG_PERF_INTERVAL` in seconds, else the default; a bad value is said
/// and the default used.
pub fn perf_every(value: Option<&str>) -> Result<Duration, String> {
    match value {
        None | Some("") => Ok(PERF_EVERY),
        Some(v) => match v.parse::<f64>() {
            Ok(s) if s > 0.0 && s.is_finite() => Ok(Duration::from_secs_f64(s)),
            _ => Err(format!("DEBUG_PERF_INTERVAL={v} is not a number of seconds above 0; using {}", PERF_EVERY.as_secs())),
        },
    }
}

impl DebugLog {
    /// Truncates `path` and copies this process's stdout and stderr into it
    /// from here on, as `debug_tee`.
    pub fn start(path: &Path) -> io::Result<Self> {
        Self::start_on(path, &sys::standard())
    }

    /// `start` over `streams` (tests use stand-ins of their own); the
    /// `[dbg]` lines go to the last one.
    fn start_on(path: &Path, streams: &[sys::Stream]) -> io::Result<Self> {
        let file = Arc::new(Mutex::new(Some(File::create(path)?)));
        // Rust's stdout is buffered: what it holds belongs before the tee.
        io::stdout().flush()?;
        let mut log = Self { path: path.to_path_buf(), file: file.clone(), teed: Vec::new(), out: *streams.last().expect("a stream to tee"), copies: Vec::new(), perf: None };
        for &stream in streams {
            // A failure here drops `log`, which puts back those teed so far.
            let sys::Teed { from, to, restore } = sys::tee(stream)?;
            log.teed.push(restore);
            let (done_tx, done_rx) = mpsc::channel();
            let file = file.clone();
            let path = log.path.clone();
            let thread = thread::Builder::new().name("debug.log".into()).spawn(move || {
                copy(from, to, &file, &path);
                // The log's end may have stopped waiting (see DRAIN).
                let _ = done_tx.send(());
            })?;
            log.copies.push((thread, done_rx));
        }
        Ok(log)
    }

    /// A `[dbg]` line, as lib/debug.sh's `dbg`: on stderr, so into the log.
    pub fn dbg(&self, msg: &str) {
        let line = format!("\x1b[2m{} [dbg] {msg}\x1b[0m\n", sys::clock());
        // Nowhere else to say it: the log's own output is what failed.
        if sys::write(self.out, line.as_bytes()).is_err() {
            self.to_file_only(&format!("[dbg] could not write to the output: {msg}"));
        }
    }

    /// The header, as view.sh's first `dbg` line, then that the lock is
    /// held.
    pub fn header(&self, desktop: &Desktop) {
        let desktop = match desktop {
            Desktop::Hyprland(_) => "hyprland",
            Desktop::Generic => "generic",
        };
        self.dbg(&format!("protoHDMIcapture {} · desktop {desktop} · log {}", env!("CARGO_PKG_VERSION"), self.path.display()));
        self.dbg("video lock: held");
    }

    /// lib/debug.sh's `debug_settings`: every setting with its value and
    /// where it came from.
    pub fn settings(&self, s: &Settings) {
        self.dbg(&format!("settings:{}", settings_line(s)));
    }

    /// What the card was opened at: the viewer asks for RES, FPS and FMT
    /// and refuses anything else, so what it asked for is what runs.
    pub fn format(&self, format: Format, test_pattern: bool) {
        let what = if test_pattern { "the test pattern" } else { "the card" };
        self.dbg(&format!(
            "negotiated: {}x{} {} @ {} fps ({what})",
            format.width,
            format.height,
            format.pixels.setting(),
            format.fps
        ));
    }

    /// A perf line every `every`, until the log ends.
    pub fn perf(&mut self, live: Arc<Live>, every: Duration) {
        let (stop, stopped) = mpsc::channel::<()>();
        let out = self.out;
        let thread = thread::spawn(move || {
            let mut prev = live.counts();
            let mut cpu = crate::dashboard::cpu_ticks();
            while stopped.recv_timeout(every) == Err(RecvTimeoutError::Timeout) {
                let now = live.counts();
                let cpu_now = crate::dashboard::cpu_ticks();
                let cpu_pct = match (cpu, cpu_now) {
                    (Some(a), Some(b)) => Some(b.saturating_sub(a) as f64 / crate::dashboard::clock_ticks_per_s() / every.as_secs_f64() * 100.0),
                    _ => None,
                };
                let line = format!("\x1b[2m{} [dbg] perf: {}\x1b[0m\n", sys::clock(), perf_line(prev, now, every, cpu_pct, crate::dashboard::rss_mb()));
                // `out` stays teed until this thread is stopped and joined.
                if let Err(e) = sys::write(out, line.as_bytes()) {
                    eprintln!("warning: debug.log: the perf line: {e}");
                    break;
                }
                prev = now;
                cpu = cpu_now;
            }
        });
        self.perf = Some((stop, thread));
    }

    /// How the run ended, in the file only: main says it on stderr.
    pub fn ended(&self, result: &Result<(), String>) {
        match result {
            Ok(()) => self.to_file_only(&format!("{} [dbg] ended", sys::clock())),
            Err(e) => self.to_file_only(&format!("{} [dbg] ended: {e}", sys::clock())),
        }
    }

    fn to_file_only(&self, line: &str) {
        let mut file = self.file.lock().expect("the log's lock is never poisoned");
        if let Some(f) = file.as_mut()
            && let Err(e) = writeln!(f, "{line}")
        {
            eprintln!("warning: debug.log: {e}");
            *file = None;
        }
    }
}

impl Drop for DebugLog {
    fn drop(&mut self) {
        if let Some((stop, thread)) = self.perf.take() {
            drop(stop);
            // A panic has printed itself; the teardown goes on.
            let _ = thread.join();
        }
        if let Err(e) = io::stdout().flush() {
            self.to_file_only(&format!("[dbg] flushing stdout: {e}"));
        }
        // The originals back: each pipe's last writer goes, and its copy
        // ends.
        for restore in self.teed.drain(..) {
            sys::restore(restore);
        }
        for (thread, done) in self.copies.drain(..) {
            if done.recv_timeout(DRAIN).is_ok() {
                // It has sent its last word: it cannot have panicked.
                let _ = thread.join();
            }
        }
    }
}

/// Copies `from` to `to` and to the log until `from` ends. A log that
/// cannot be written is said once and the copy goes on without it: the
/// feed matters more than its log. An original output that fails (a
/// reader gone) is said once in the log, which then has the rest alone.
/// The log's lock is held for one write at a time: `ended` and the other
/// copy take it too, and this one blocks in `read`.
fn copy(mut from: File, mut to: impl Write, file: &Mutex<Option<File>>, path: &Path) {
    let mut buf = [0u8; 8192];
    let mut forward = true;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Said where the output goes; if that fails too, nowhere is left.
            Err(e) => {
                let _ = writeln!(to, "warning: {}: reading the output: {e}; the log stops here", path.display());
                return;
            }
        };
        {
            let mut log = file.lock().expect("the log's lock is never poisoned");
            if let Some(f) = log.as_mut()
                && let Err(e) = f.write_all(&buf[..n])
            {
                // As above: the original output is the only place left.
                if forward {
                    let _ = writeln!(to, "warning: {}: {e}; the feed goes on without its log", path.display());
                }
                *log = None;
            }
        }
        // The original output gone: the log still gets the rest, and the
        // pipe is still drained so that nothing writing to it blocks.
        if forward && let Err(e) = to.write_all(&buf[..n]) {
            forward = false;
            let mut log = file.lock().expect("the log's lock is never poisoned");
            if let Some(f) = log.as_mut() {
                // The log is the only place left to say it.
                let _ = writeln!(f, "[dbg] the original output failed: {e}; only the log has the rest");
            }
        }
    }
}

/// ` NAME=value(source)` for every setting, `""` for an empty value.
fn settings_line(s: &Settings) -> String {
    let mut out = String::new();
    for r in &s.rows {
        let v = s.get(&r.name);
        let from = s.from(&r.name).word();
        let _ = write!(out, " {}={}({from})", r.name, if v.is_empty() { "\"\"" } else { v }); // a String cannot fail
    }
    out
}

/// One perf line from two readings `every` apart.
fn perf_line(prev: Counts, now: Counts, every: Duration, cpu_pct: Option<f64>, rss_mb: Option<f64>) -> String {
    let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / every.as_secs_f64();
    let cpu = cpu_pct.map_or("?".into(), |c| format!("{c:.1}"));
    let rss = rss_mb.map_or("?".into(), |r| format!("{r:.0}"));
    format!(
        "viewer cpu {cpu}% rss {rss}MB | capture {:.1} fps presented {:.1} fps repeated {} | gaps {} lost {} | dropped undecodable {} unusable {}",
        rate(prev.frames, now.frames),
        rate(prev.presented, now.presented),
        now.repeated.saturating_sub(prev.repeated),
        now.gaps,
        now.lost,
        now.undecodable,
        now.unusable
    )
}

/// The tee on Linux: each fd pointed at a pipe with `dup2`.
#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{FromRawFd, RawFd};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A teed stream: the fd itself.
    pub type Stream = RawFd;

    pub fn standard() -> [Stream; 2] {
        [libc::STDOUT_FILENO, libc::STDERR_FILENO]
    }

    /// A stream pointed at its pipe: the pipe's read end, where the output
    /// went before, and what puts it back.
    pub struct Teed {
        pub from: File,
        pub to: File,
        pub restore: Restore,
    }

    /// The fd and a duplicate of what it was.
    pub struct Restore {
        fd: RawFd,
        original: RawFd,
    }

    pub fn tee(fd: RawFd) -> io::Result<Teed> {
        // Safety: plain fd calls on fds this process owns; every result is
        // checked.
        let original = cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })?;
        let mut pipe = [0; 2];
        if let Err(e) = cvt(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) }) {
            unsafe { libc::close(original) };
            return Err(e);
        }
        // The copy writes to its own duplicate, so putting `original` back
        // never pulls the fd from under it.
        let to_original = match cvt(unsafe { libc::fcntl(original, libc::F_DUPFD_CLOEXEC, 3) }) {
            Ok(d) => d,
            Err(e) => {
                unsafe {
                    libc::close(original);
                    libc::close(pipe[0]);
                    libc::close(pipe[1]);
                }
                return Err(e);
            }
        };
        if let Err(e) = cvt(unsafe { libc::dup2(pipe[1], fd) }) {
            unsafe {
                libc::close(original);
                libc::close(to_original);
                libc::close(pipe[0]);
                libc::close(pipe[1]);
            }
            return Err(e);
        }
        unsafe { libc::close(pipe[1]) };
        // Safety: each fd is ours alone from here, and closed once.
        let (from, to) = unsafe { (File::from_raw_fd(pipe[0]), File::from_raw_fd(to_original)) };
        Ok(Teed { from, to, restore: Restore { fd, original } })
    }

    pub fn restore(r: Restore) {
        // Safety: both fds are open and ours.
        unsafe {
            libc::dup2(r.original, r.fd);
            libc::close(r.original);
        }
    }

    /// Writes to the stream, teed or not.
    pub fn write(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
        // Safety: the fd is open while the log runs; it is not closed here.
        let mut out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        out.write_all(bytes)
    }

    /// The local time as `date +%T.%3N`.
    pub fn clock() -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let secs = now.as_secs() as libc::time_t;
        // Safety: localtime_r writes into `tm`, which outlives the call.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            return format!("{secs}.{:03}", now.subsec_millis());
        }
        format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
    }

    pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
    }
}

/// The tee on Windows (#33): each standard handle swapped for a pipe's
/// write end with `SetStdHandle`, and the copy forwarded to what it was.
#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows::Win32::System::Console::{
        CONSOLE_MODE, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle, WriteConsoleW,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Threading::GetCurrentProcess;

    /// A teed stream: where its handle is kept, the process's standard
    /// handle or (tests) a stand-in.
    #[derive(Clone, Copy)]
    pub struct Stream {
        pub get: fn() -> HANDLE,
        pub set: fn(HANDLE) -> windows::core::Result<()>,
    }

    pub fn standard() -> [Stream; 2] {
        // Safety (all four): the process's standard handle slots; none is
        // closed here. No handle (a process without a console) reads as null.
        [
            Stream { get: || unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.unwrap_or_default(), set: |h| unsafe { SetStdHandle(STD_OUTPUT_HANDLE, h) } },
            Stream { get: || unsafe { GetStdHandle(STD_ERROR_HANDLE) }.unwrap_or_default(), set: |h| unsafe { SetStdHandle(STD_ERROR_HANDLE, h) } },
        ]
    }

    /// A stream pointed at its pipe: the pipe's read end, where the output
    /// went before, and what puts it back.
    pub struct Teed {
        pub from: File,
        pub to: Forward,
        pub restore: Restore,
    }

    /// The stream, its handle before, and the pipe's write end, held open
    /// until the stream is put back.
    pub struct Restore {
        stream: Stream,
        /// The handle as a number: not ours to close, only to put back
        /// (and so the log may end on another thread).
        original: isize,
        write: File,
    }

    pub fn tee(stream: Stream) -> io::Result<Teed> {
        let original = (stream.get)();
        let to = Forward::to(original)?;
        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // Safety: two out-pointers; not inheritable (no attributes), so a
        // child started with its own handles holds no end.
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }?;
        // Safety: each handle is ours alone from here, and closed once.
        let (from, write) = unsafe { (File::from_raw_handle(read.0), File::from_raw_handle(write.0)) };
        (stream.set)(HANDLE(write.as_raw_handle()))?;
        Ok(Teed { from, to, restore: Restore { stream, original: original.0 as isize, write } })
    }

    pub fn restore(r: Restore) {
        // Nowhere to say a failure: the output still goes into the pipe.
        let _ = (r.stream.set)(HANDLE(r.original as *mut _));
        // The pipe's last writer: its copy ends.
        drop(r.write);
    }

    /// Writes to the stream, teed or not.
    pub fn write(stream: Stream, bytes: &[u8]) -> io::Result<()> {
        let h = (stream.get)();
        if h.is_invalid() {
            return Err(io::Error::other("no handle"));
        }
        // Safety: the handle is open while the log runs; it is not closed here.
        let mut out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(h.0) });
        out.write_all(bytes)
    }

    /// Where a stream's copy goes: a console takes UTF-16, as Rust's own
    /// stdout writes it (bytes would show in the console's code page); a
    /// file or pipe takes the bytes; with no handle, nowhere.
    pub enum Forward {
        Nowhere,
        Bytes(File),
        Console { handle: File, pending: Vec<u8> },
    }

    impl Forward {
        fn to(original: HANDLE) -> io::Result<Self> {
            if original.is_invalid() {
                return Ok(Self::Nowhere);
            }
            let mut dup = HANDLE::default();
            // Safety: a duplicate of a live handle, owned by `handle` below,
            // so putting the original back never pulls it from under the copy.
            unsafe { DuplicateHandle(GetCurrentProcess(), original, GetCurrentProcess(), &mut dup, 0, false, DUPLICATE_SAME_ACCESS) }?;
            let handle = unsafe { File::from_raw_handle(dup.0) };
            let mut mode = CONSOLE_MODE::default();
            // Safety: an out-pointer and a live handle.
            Ok(if unsafe { GetConsoleMode(dup, &mut mode) }.is_ok() { Self::Console { handle, pending: Vec::new() } } else { Self::Bytes(handle) })
        }
    }

    impl Write for Forward {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                Self::Nowhere => Ok(buf.len()),
                Self::Bytes(f) => f.write(buf),
                Self::Console { handle, pending } => {
                    pending.extend_from_slice(buf);
                    let whole = whole(pending);
                    let text: Vec<u16> = String::from_utf8_lossy(&pending[..whole]).encode_utf16().collect();
                    pending.drain(..whole);
                    let mut rest = &text[..];
                    while !rest.is_empty() {
                        let mut n = 0u32;
                        // Safety: a live console handle and a slice it reads.
                        unsafe { WriteConsoleW(HANDLE(handle.as_raw_handle()), rest, Some(&mut n), None) }?;
                        if n == 0 {
                            return Err(io::Error::from(io::ErrorKind::WriteZero));
                        }
                        rest = &rest[n as usize..];
                    }
                    Ok(buf.len())
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            match self {
                Self::Bytes(f) => f.flush(),
                _ => Ok(()),
            }
        }
    }

    /// How much of `pending` goes to the console now: all of it, but for a
    /// character split across reads, which waits for its rest. Bytes that
    /// are no UTF-8 at all go, as U+FFFD.
    pub fn whole(pending: &[u8]) -> usize {
        match std::str::from_utf8(pending) {
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            _ => pending.len(),
        }
    }

    /// The local time as `date +%T.%3N`.
    pub fn clock() -> String {
        // Safety: no arguments.
        let t = unsafe { GetLocalTime() };
        format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::capture::Pixels;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("protohdmi-debuglog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A pipe standing in for stderr: (read end, the stream writing to it).
    /// `slot` tells each test's stand-in apart on Windows.
    #[cfg(unix)]
    fn stand_in(_slot: usize) -> (File, sys::Stream) {
        use std::os::fd::FromRawFd;
        let mut p = [0; 2];
        sys::cvt(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }).unwrap();
        (unsafe { File::from_raw_fd(p[0]) }, p[1])
    }

    /// Closes the stream's handle: the stand-in's reader then sees its end.
    #[cfg(unix)]
    fn close(fd: sys::Stream) {
        unsafe { libc::close(fd) };
    }

    #[cfg(windows)]
    mod slots {
        use std::sync::atomic::{AtomicIsize, Ordering};

        use windows::Win32::Foundation::HANDLE;

        /// A standard handle slot per test, as `SetStdHandle` keeps them; a
        /// fn pointer reaches them from the perf thread too.
        static SLOTS: [AtomicIsize; 4] = [const { AtomicIsize::new(0) }; 4];

        pub fn get<const N: usize>() -> HANDLE {
            HANDLE(SLOTS[N].load(Ordering::SeqCst) as *mut _)
        }

        pub fn set<const N: usize>(h: HANDLE) -> windows::core::Result<()> {
            SLOTS[N].store(h.0 as isize, Ordering::SeqCst);
            Ok(())
        }
    }

    #[cfg(windows)]
    fn stand_in(slot: usize) -> (File, sys::Stream) {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Pipes::CreatePipe;
        let stream = match slot {
            0 => sys::Stream { get: slots::get::<0>, set: slots::set::<0> },
            1 => sys::Stream { get: slots::get::<1>, set: slots::set::<1> },
            2 => sys::Stream { get: slots::get::<2>, set: slots::set::<2> },
            _ => sys::Stream { get: slots::get::<3>, set: slots::set::<3> },
        };
        let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
        unsafe { CreatePipe(&mut r, &mut w, None, 0) }.unwrap();
        (stream.set)(w).unwrap();
        (unsafe { File::from_raw_handle(r.0) }, stream)
    }

    #[cfg(windows)]
    fn close(stream: sys::Stream) {
        unsafe { windows::Win32::Foundation::CloseHandle((stream.get)()) }.unwrap();
    }

    fn write_to(stream: sys::Stream, s: &str) {
        sys::write(stream, s.as_bytes()).unwrap();
    }

    #[test]
    fn a_session_leaves_its_header_settings_format_perf_and_end() {
        let dir = tmp("session");
        let path = dir.join("debug.log");
        // A stale log from an earlier session: started anew.
        std::fs::write(&path, "an earlier session\n").unwrap();
        let (mut terminal, out) = stand_in(0);
        let live = Arc::new(Live::new());
        let s = Settings::resolve_with(&|k| match k {
            "CONFIG_FILE" => Some(dir.join("none").display().to_string()),
            "DEBUG" => Some("log".into()),
            _ => None,
        });
        {
            let mut log = DebugLog::start_on(&path, &[out]).unwrap();
            log.header(&Desktop::Generic);
            log.settings(&s);
            log.format(Format { width: 1920, height: 1080, fps: 60.0, pixels: Pixels::Yuyv }, false);
            write_to(out, "warning: something the viewer said\n");
            log.perf(live.clone(), Duration::from_millis(20));
            for _ in 0..30 {
                live.frame();
            }
            thread::sleep(Duration::from_millis(80));
            log.ended(&Err("capture: the card is gone".into()));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("an earlier session"), "{text}");
        for want in [
            "[dbg] protoHDMIcapture ",
            " · desktop generic · log ",
            "[dbg] video lock: held",
            " DEBUG=log(env)",
            " RES=1920x1080(default)",
            "[dbg] negotiated: 1920x1080 yuyv422 @ 60 fps (the card)",
            "warning: something the viewer said",
            "[dbg] perf: viewer cpu ",
            "[dbg] ended: capture: the card is gone",
        ] {
            assert!(text.contains(want), "no {want:?} in\n{text}");
        }
        // The original output got the same, but not the end line.
        close(out);
        let mut seen = String::new();
        terminal.read_to_string(&mut seen).unwrap();
        assert!(seen.contains("warning: something the viewer said") && seen.contains("[dbg] perf: "), "{seen}");
        assert!(!seen.contains("ended:"), "{seen}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_output_goes_back_once_the_log_ends() {
        let dir = tmp("restore");
        let path = dir.join("debug.log");
        let (mut terminal, out) = stand_in(1);
        drop(DebugLog::start_on(&path, &[out]).unwrap());
        write_to(out, "after the log\n");
        close(out);
        let mut seen = String::new();
        terminal.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "after the log\n");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("after the log"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_original_output_gone_leaves_the_log_and_its_end_working() {
        // The terminal's reader gone mid-run: the copy goes on into the
        // log alone, and neither `ended` nor the end waits on it (Codex
        // review of #27: the copy held the log's lock while it read).
        let dir = tmp("gone");
        let path = dir.join("debug.log");
        let (terminal, out) = stand_in(2);
        drop(terminal);
        let log = DebugLog::start_on(&path, &[out]).unwrap();
        write_to(out, "before\n");
        write_to(out, "after\n");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&path).unwrap().contains("after") {
            assert!(std::time::Instant::now() < deadline, "the log never got the output");
            thread::sleep(Duration::from_millis(5));
        }
        let (done, ended) = mpsc::channel();
        thread::spawn(move || {
            log.ended(&Ok(()));
            drop(log);
            done.send(()).unwrap();
        });
        ended.recv_timeout(Duration::from_secs(5)).expect("the log's end hung on the copy");
        let text = std::fs::read_to_string(&path).unwrap();
        for want in ["before", "after", "[dbg] the original output failed: ", "[dbg] ended"] {
            assert!(text.contains(want), "no {want:?} in\n{text}");
        }
        close(out);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A console takes whole characters: one split across two writes
    /// waits for its rest (#33).
    #[cfg(windows)]
    #[test]
    fn a_character_split_across_reads_reaches_the_console_whole() {
        let s = "a · b".as_bytes();
        let dot = s.iter().position(|&b| b == 0xC2).unwrap();
        assert_eq!(sys::whole(&s[..dot + 1]), dot, "the half of · waits");
        assert_eq!(sys::whole(s), s.len());
        // Not UTF-8 at all: goes on, not held for ever.
        assert_eq!(sys::whole(b"a\xFFb"), 3);
        assert_eq!(sys::whole(b""), 0);
    }

    #[test]
    fn a_perf_line_from_two_readings() {
        let a = Counts { frames: 100, presented: 90, repeated: 1, ..Counts::default() };
        let b = Counts { frames: 400, presented: 390, repeated: 4, gaps: 1, lost: 2, undecodable: 3, unusable: 0 };
        assert_eq!(
            perf_line(a, b, Duration::from_secs(5), Some(12.34), Some(87.6)),
            "viewer cpu 12.3% rss 88MB | capture 60.0 fps presented 60.0 fps repeated 3 | gaps 1 lost 2 | dropped undecodable 3 unusable 0"
        );
        assert!(perf_line(a, b, Duration::from_secs(5), None, None).starts_with("viewer cpu ?% rss ?MB"));
        // The counters it reads are the run's.
        let live = Live::new();
        live.frame();
        live.dropped.unusable.fetch_add(2, Ordering::Relaxed);
        assert_eq!(live.counts(), Counts { frames: 1, unusable: 2, ..Counts::default() });
    }

    #[test]
    fn the_perf_interval_from_the_environment() {
        assert_eq!(perf_every(None), Ok(PERF_EVERY));
        assert_eq!(perf_every(Some("")), Ok(PERF_EVERY));
        assert_eq!(perf_every(Some("0.5")), Ok(Duration::from_millis(500)));
        assert!(perf_every(Some("0")).unwrap_err().contains("DEBUG_PERF_INTERVAL=0"));
        assert!(perf_every(Some("soon")).is_err());
    }
}
