//! The dashboard (#16): the viewer's live numbers in a terminal, as
//! `protohdmicapture --dashboard`, another process of the same binary.
//!
//! - The viewer keeps its numbers in a `Live`, one per window run, handed
//!   in by the run's caller (#21). The capture thread, the display and the
//!   audio write it; nothing in it blocks them.
//! - `Server`: once a second, a snapshot of `Live` as one JSON line to every
//!   connected dashboard, over `dashboard.sock` in the run directory
//!   (docs/run-dir.md). Writes never block: a dashboard that stops reading
//!   is dropped once a minute of lines waits for it, and the feed never
//!   notices.
//! - `client`: connects, draws each line, and closes itself when the viewer
//!   goes, as view.sh's dashboard does. Quitting it touches nothing.
//! - `open`: the d key, a new terminal running `--dashboard`.
//!
//! The transport is a Unix socket; on Windows (#9) a named pipe
//! (`\\.\pipe\protohdmicapture-dashboard`) takes `transport`'s place, and
//! the lines stay the same.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::audio::{Counters, RATE};
use crate::capture::{Dropped, Reason, Since, Signal};
use crate::overlay::Notify;
use crate::stats::percentiles;

/// The lines' format; a dashboard of another version says so.
pub const VERSION: u64 = 1;
/// How often a snapshot goes out.
pub const TICK: Duration = Duration::from_secs(1);
/// The latency figures cover this much time.
const WINDOW: Duration = Duration::from_secs(10);
/// A dashboard this far behind is dropped.
const BACKLOG: usize = 64 * 1024;
/// The events kept for the dashboard.
const EVENTS: usize = 8;
/// Latency samples kept between two snapshots: a few seconds even at 240
/// Hz. Only the server takes them, so with no server (it failed to
/// start) they would otherwise pile up for as long as the viewer runs.
const SAMPLES: usize = 4096;
/// A started dashboard waits this long for a viewer to appear.
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// No line for this long: the viewer is shown as not answering.
const STALE: Duration = Duration::from_secs(3);

/// The socket in the run directory.
pub fn socket_path(run_dir: &Path) -> PathBuf {
    run_dir.join("dashboard.sock")
}

/// The audio, as the dashboard shows it.
pub enum AudioLive {
    /// AUDIO is not loopback, or not started yet: why.
    Off(String),
    Failed(String),
    Running {
        source: String,
        sink: String,
        /// LAT_MS, what was asked.
        buffer_ms: u32,
        counters: Arc<Counters>,
    },
}

/// One window run's numbers, written where they happen. The viewer's is
/// the one the dashboard serves; the setup wizard's feed windows (#13) each
/// write into one nobody reads, so theirs never reach the viewer's.
pub struct Live {
    /// The run's start.
    started: Instant,
    frames: AtomicU64,
    gaps: AtomicU64,
    lost: AtomicU64,
    /// The frame policy's signal state.
    signal: Mutex<Signal>,
    /// Buffers the frame policy dropped, by reason; it counts them here.
    pub dropped: Dropped,
    presented: AtomicU64,
    repeated: AtomicU64,
    /// The display's refresh period, in ns (0: unknown).
    refresh_ns: AtomicU64,
    /// (dequeue -> presented, kernel -> presented) since the last snapshot.
    samples: Mutex<Vec<(Duration, Duration)>>,
    events: Mutex<VecDeque<String>>,
    audio: Mutex<AudioLive>,
}

impl Live {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            frames: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            signal: Mutex::new(Signal::Picture),
            dropped: Dropped::default(),
            presented: AtomicU64::new(0),
            repeated: AtomicU64::new(0),
            refresh_ns: AtomicU64::new(0),
            samples: Mutex::new(Vec::with_capacity(256)),
            events: Mutex::new(VecDeque::with_capacity(EVENTS)),
            audio: Mutex::new(AudioLive::Off("not started".into())),
        }
    }

    pub fn frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn gap(&self, from: u32, to: u32, lost: u32) {
        self.gaps.fetch_add(1, Ordering::Relaxed);
        self.lost.fetch_add(u64::from(lost), Ordering::Relaxed);
        self.event(format!("capture gap: sequence {from} -> {to}, {lost} lost"));
    }

    /// A change of the signal state, as the frame policy sends it.
    pub fn signal(&self, signal: &Signal) {
        *self.signal.lock().expect("the signal's lock is never poisoned") = signal.clone();
        self.event(signal.to_string());
    }

    /// A presented card frame: its latencies and the refreshes it repeated.
    pub fn presented(&self, to_present: Duration, kernel_to_present: Duration, repeated: u64, refresh: Option<Duration>) {
        self.presented.fetch_add(1, Ordering::Relaxed);
        self.repeated.fetch_add(repeated, Ordering::Relaxed);
        if let Some(r) = refresh {
            self.refresh_ns.store(r.as_nanos() as u64, Ordering::Relaxed);
        }
        let mut samples = self.samples.lock().expect("the samples' lock is never poisoned");
        if samples.len() < SAMPLES {
            samples.push((to_present, kernel_to_present));
        }
    }

    fn up(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn set_audio(&self, audio: AudioLive) {
        *self.audio.lock().expect("the audio's lock is never poisoned") = audio;
    }

    fn event(&self, text: String) {
        let mut e = self.events.lock().expect("the events' lock is never poisoned");
        if e.len() == EVENTS {
            e.pop_front();
        }
        e.push_back(format!("{:7.1} s  {text}", self.up().as_secs_f64()));
    }
}

/// What a snapshot needs from the previous ones: rates and the window.
struct Rolling {
    at: Instant,
    frames: u64,
    presented: u64,
    repeated: u64,
    cpu_ticks: Option<u64>,
    /// The snapshots of the last WINDOW, oldest first.
    ticks: VecDeque<Tick>,
}

/// (when it began, its (dequeue, kernel) -> presented samples, the
/// repeated refreshes at its start).
type Tick = (Instant, Vec<(Duration, Duration)>, u64);

impl Rolling {
    fn new(live: &Live) -> Self {
        Self {
            at: Instant::now(),
            frames: live.frames.load(Ordering::Relaxed),
            presented: live.presented.load(Ordering::Relaxed),
            repeated: live.repeated.load(Ordering::Relaxed),
            cpu_ticks: cpu_ticks(),
            ticks: VecDeque::new(),
        }
    }

    /// The snapshot now, as one JSON value.
    fn snapshot(&mut self, live: &Live) -> Value {
        let now = Instant::now();
        let dt = (now - self.at).as_secs_f64().max(1e-3);
        let frames = live.frames.load(Ordering::Relaxed);
        let presented = live.presented.load(Ordering::Relaxed);
        let repeated = live.repeated.load(Ordering::Relaxed);
        let cpu = cpu_ticks();
        let cpu_pct = match (self.cpu_ticks, cpu) {
            // Safety: sysconf has no preconditions.
            (Some(a), Some(b)) => Some((b.saturating_sub(a)) as f64 / clock_ticks_per_s() / dt * 100.0),
            _ => None,
        };

        let fresh = std::mem::take(&mut *live.samples.lock().expect("the samples' lock is never poisoned"));
        self.ticks.push_back((self.at, fresh, self.repeated));
        while self.ticks.len() > 1 && self.ticks.front().is_some_and(|(t, _, _)| now - *t > WINDOW) {
            self.ticks.pop_front();
        }
        let (mut to, mut kernel): (Vec<Duration>, Vec<Duration>) = self.ticks.iter().flat_map(|(_, s, _)| s.iter().copied()).unzip();
        let (began, oldest) = self.ticks.front().map_or((now, repeated), |(t, _, r)| (*t, *r));
        let window_s = (now - began).as_secs_f64();
        let latency = |v: &mut Vec<Duration>| match percentiles(v) {
            Some((median, p99)) => json!({ "median_ms": median, "p99_ms": p99, "n": v.len() }),
            None => Value::Null,
        };
        let refresh_ns = live.refresh_ns.load(Ordering::Relaxed);
        let (signal, signal_why) = signal_json(&live.signal.lock().expect("the signal's lock is never poisoned"));

        let snap = json!({
            "v": VERSION,
            "pid": std::process::id(),
            "up_s": live.up().as_secs_f64(),
            "capture_fps": (frames - self.frames) as f64 / dt,
            "presented_fps": (presented - self.presented) as f64 / dt,
            "frames": frames,
            "gaps": live.gaps.load(Ordering::Relaxed),
            "lost": live.lost.load(Ordering::Relaxed),
            "signal": signal,
            "signal_why": signal_why,
            "dropped": {
                "undecodable": live.dropped.undecodable.load(Ordering::Relaxed),
                "unusable": live.dropped.unusable.load(Ordering::Relaxed),
            },
            "window_s": window_s,
            "dequeue_present": latency(&mut to),
            "kernel_present": latency(&mut kernel),
            "refresh_hz": if refresh_ns == 0 { Value::Null } else { json!(1e9 / refresh_ns as f64) },
            "repeated_window": repeated - oldest,
            "repeated_total": repeated,
            "cpu_pct": cpu_pct,
            "rss_mb": rss_mb(),
            "audio": audio_json(&live.audio.lock().expect("the audio's lock is never poisoned")),
            "events": live.events.lock().expect("the events' lock is never poisoned").iter().cloned().collect::<Vec<_>>(),
        });
        self.at = now;
        self.frames = frames;
        self.presented = presented;
        self.repeated = repeated;
        self.cpu_ticks = cpu;
        snap
    }
}

/// The state as view.sh's dashboard names it: ok, never (no picture since
/// the feed opened) or stopped; and why, when frames arrive that cannot be
/// shown (null when nothing arrives).
fn signal_json(signal: &Signal) -> (&'static str, Value) {
    let Signal::NoSignal { since, reason } = signal else { return ("ok", Value::Null) };
    let name = match since {
        Since::Never => "never",
        Since::Stopped => "stopped",
    };
    let why = match reason {
        Reason::Nothing => Value::Null,
        Reason::Undecodable(why) => json!(format!("MJPEG frames do not decode: {why}")),
        Reason::Unusable(why) => json!(format!("frames the viewer cannot use: {why}")),
    };
    (name, why)
}

fn audio_json(a: &AudioLive) -> Value {
    match a {
        AudioLive::Off(why) => json!({ "state": "off", "why": why }),
        AudioLive::Failed(why) => json!({ "state": "failed", "why": why }),
        AudioLive::Running { source, sink, buffer_ms, counters: c } => {
            let ms = |f: u32| f64::from(f) * 1000.0 / f64::from(RATE);
            let range = |lo: &std::sync::atomic::AtomicU32, hi: &std::sync::atomic::AtomicU32| {
                let (lo, hi) = (lo.load(Ordering::Relaxed), hi.load(Ordering::Relaxed));
                if lo == u32::MAX { Value::Null } else { json!([ms(lo), ms(hi)]) }
            };
            json!({
                "state": "running",
                "source": source,
                "sink": sink,
                "buffer_ms": buffer_ms,
                "granted_in_ms": range(&c.in_min, &c.in_max),
                "granted_out_ms": range(&c.out_min, &c.out_max),
                "delay_ms": c.target_ms.load(Ordering::Relaxed),
                "ring_ms": ms(c.fill.load(Ordering::Relaxed)),
                "underruns": c.ring_underruns.load(Ordering::Relaxed),
                "xruns": c.xruns.load(Ordering::Relaxed),
                "errors": c.other_errors.load(Ordering::Relaxed),
            })
        }
    }
}

/// This process's user and system CPU time, in clock ticks.
#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // After the command name, which may hold spaces: fields 3 onwards.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_s() -> f64 {
    // Safety: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

#[cfg(target_os = "linux")]
fn rss_mb() -> Option<f64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // Safety: sysconf has no preconditions.
    Some(pages as f64 * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64 / 1048576.0)
}

/// No CPU and memory figures off Linux yet (they read /proc).
#[cfg(not(target_os = "linux"))]
fn cpu_ticks() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_s() -> f64 {
    1.0
}

#[cfg(not(target_os = "linux"))]
fn rss_mb() -> Option<f64> {
    None
}

/// Why the dashboard does not run on Windows yet (#18).
#[cfg(windows)]
const NOT_ON_WINDOWS: &str = "the dashboard is not on Windows yet: its named pipe is still to come (#18)";

/// The Unix socket.
#[cfg(unix)]
mod transport {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;

    /// Binds `path`, replacing a socket a killed viewer left. Called with
    /// `video.lock` held, so no live viewer owns it. Returns the inode, to
    /// remove only our own at the end.
    pub fn listen(path: &Path) -> std::io::Result<(Listener, u64)> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let l = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        l.set_nonblocking(true)?;
        Ok((l, std::fs::metadata(path)?.ino()))
    }

    pub fn inode(path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|m| m.ino())
    }

    pub fn connect(path: &Path) -> std::io::Result<Stream> {
        UnixStream::connect(path)
    }
}

/// Windows: the named pipe is still to come (#18), so listening and
/// connecting fail and nothing else here runs. The stream types only give
/// the server and client code the same shape on both platforms; no TCP
/// socket is ever opened.
#[cfg(windows)]
mod transport {
    use std::path::Path;

    pub type Listener = std::net::TcpListener;
    pub type Stream = std::net::TcpStream;

    fn not_yet() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Unsupported, super::NOT_ON_WINDOWS)
    }

    pub fn listen(_: &Path) -> std::io::Result<(Listener, u64)> {
        Err(not_yet())
    }

    pub fn inode(_: &Path) -> Option<u64> {
        None
    }

    pub fn connect(_: &Path) -> std::io::Result<Stream> {
        Err(not_yet())
    }
}

/// A dashboard connected to the viewer, and what has not reached it yet.
struct Client {
    stream: transport::Stream,
    pending: Vec<u8>,
}

impl Client {
    /// Sends what it can without waiting. False: the dashboard is gone or
    /// too far behind.
    fn send(&mut self, line: &[u8]) -> bool {
        self.pending.extend_from_slice(line);
        while !self.pending.is_empty() {
            match self.stream.write(&self.pending) {
                Ok(0) => return false,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        self.pending.len() <= BACKLOG
    }
}

/// The viewer's side: the socket, and the thread that feeds it.
pub struct Server {
    path: PathBuf,
    inode: u64,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Listens on `path` and sends `live` to each dashboard every `tick`.
    pub fn start(path: &Path, live: Arc<Live>, tick: Duration) -> Result<Self, String> {
        let (listener, inode) = transport::listen(path).map_err(|e| format!("dashboard socket {}: {e}", path.display()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("dashboard".into())
                .spawn(move || serve(listener, &live, tick, &stop))
                .map_err(|e| format!("dashboard thread: {e}"))?
        };
        Ok(Self { path: path.to_path_buf(), inode, stop, thread: Some(thread) })
    }
}

fn serve(listener: transport::Listener, live: &Live, tick: Duration, stop: &AtomicBool) {
    let mut clients: Vec<Client> = Vec::new();
    let mut rolling = Rolling::new(live);
    let slice = Duration::from_millis(50).min(tick);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < next {
            thread::sleep(slice.min(next - now));
            continue;
        }
        next += tick;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        clients.push(Client { stream, pending: Vec::new() });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("warning: dashboard: accept: {e}");
                    break;
                }
            }
        }
        // Computed with no dashboard too: the rates and the window stay
        // right for the next one to connect.
        let mut line = rolling.snapshot(live).to_string().into_bytes();
        line.push(b'\n');
        clients.retain_mut(|c| c.send(&line));
    }
}

impl Drop for Server {
    /// Dashboards see the end of the stream and close; the socket goes,
    /// if it is still ours.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if transport::inode(&self.path) == Some(self.inode)
            && let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("warning: cannot remove {}: {e}", self.path.display());
        }
    }
}

/// Colours for a terminal; none when the output is not one.
struct Paint {
    on: bool,
}

impl Paint {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn head(&self, t: &str) -> String {
        self.wrap("1;34", t)
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn red(&self, t: &str) -> String {
        self.wrap("31", t)
    }
    /// Green when `ok`, yellow when not, as view.sh's dashboard.
    fn good_bad(&self, t: &str, ok: bool) -> String {
        self.wrap(if ok { "32" } else { "33" }, t)
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// One snapshot as the dashboard's text.
fn render(s: &Value, p: &Paint) -> String {
    let mut out = Vec::new();
    let row = |out: &mut Vec<String>, name: &str, value: String| out.push(format!("  {} {value}", p.dim(&format!("{name:<17}"))));
    let n1 = |v: &Value| num(v).map_or("?".to_string(), |x| format!("{x:.1}"));
    let n2 = |v: &Value| num(v).map_or("?".to_string(), |x| format!("{x:.2}"));
    let int = |v: &Value| v.as_u64().map_or("?".to_string(), |x| x.to_string());

    out.push(String::new());
    out.push(format!(
        "  {}   {}",
        p.bold("protoHDMIcapture · live"),
        p.dim(&format!("Rust viewer · pid {} · up {:.0} s", int(&s["pid"]), num(&s["up_s"]).unwrap_or(0.0)))
    ));

    out.push(String::new());
    out.push(format!("  {}", p.head("Video")));
    let fps = num(&s["capture_fps"]).unwrap_or(0.0);
    let shown = num(&s["presented_fps"]).unwrap_or(0.0);
    row(
        &mut out,
        "fps",
        format!(
            "{} captured · {} presented{}",
            n1(&s["capture_fps"]),
            p.good_bad(&n1(&s["presented_fps"]), fps == 0.0 || shown >= fps * 0.95),
            match num(&s["refresh_hz"]) {
                Some(hz) => format!(" · display {hz:.2} Hz"),
                None => String::new(),
            }
        ),
    );
    let gaps = s["gaps"].as_u64().unwrap_or(0);
    row(
        &mut out,
        "capture gaps",
        format!("{} · {} frames lost · {} frames captured", p.good_bad(&int(&s["gaps"]), gaps == 0), int(&s["lost"]), int(&s["frames"])),
    );
    row(
        &mut out,
        "signal",
        match (s["signal"].as_str(), s["signal_why"].as_str()) {
            (Some("ok"), _) => "ok".into(),
            (Some("never"), None) => p.red("none since the feed opened"),
            (Some("stopped"), None) => p.red("stopped (source asleep or off?)"),
            (Some("never" | "stopped"), Some(why)) => p.red(&format!("no picture: {why}")),
            _ => "?".into(),
        },
    );
    let (undecodable, unusable) = (s["dropped"]["undecodable"].as_u64().unwrap_or(0), s["dropped"]["unusable"].as_u64().unwrap_or(0));
    row(
        &mut out,
        "dropped",
        format!(
            "{} · {} undecodable · {} unusable",
            p.good_bad(&(undecodable + unusable).to_string(), undecodable + unusable == 0),
            int(&s["dropped"]["undecodable"]),
            int(&s["dropped"]["unusable"])
        ),
    );
    let window = format!("last {:.0} s", num(&s["window_s"]).unwrap_or(0.0));
    for (name, key) in [("dequeue→presented", "dequeue_present"), ("kernel→presented", "kernel_present")] {
        let v = &s[key];
        row(
            &mut out,
            name,
            if v.is_null() {
                p.dim("no frame presented yet")
            } else {
                format!("median {} ms · p99 {} ms {}", n2(&v["median_ms"]), n2(&v["p99_ms"]), p.dim(&format!("({window}, {} frames)", int(&v["n"]))))
            },
        );
    }
    let rep = s["repeated_window"].as_u64().unwrap_or(0);
    row(
        &mut out,
        "repeated",
        format!(
            "{} refreshes {} · {} since start",
            p.good_bad(&int(&s["repeated_window"]), rep == 0),
            p.dim(&format!("({window})")),
            int(&s["repeated_total"])
        ),
    );
    row(&mut out, "process", format!("cpu {}% · rss {} MB", n1(&s["cpu_pct"]), n1(&s["rss_mb"])));

    out.push(String::new());
    out.push(format!("  {}", p.head("Audio")));
    let a = &s["audio"];
    match a["state"].as_str() {
        Some("running") => {
            row(&mut out, "linked", format!("{} → {}", a["source"].as_str().unwrap_or("?"), a["sink"].as_str().unwrap_or("?")));
            let granted = |v: &Value| match v.as_array().map(|r| (num(&r[0]), num(&r[1]))) {
                Some((Some(lo), Some(hi))) if (lo - hi).abs() < 0.05 => format!("{lo:.1} ms"),
                Some((Some(lo), Some(hi))) => format!("{lo:.1}..{hi:.1} ms"),
                _ => "?".into(),
            };
            row(
                &mut out,
                "buffer",
                format!(
                    "asked {} ms · got in {} · out {}",
                    int(&a["buffer_ms"]),
                    granted(&a["granted_in_ms"]),
                    granted(&a["granted_out_ms"])
                ),
            );
            row(&mut out, "sync", format!("delay {} ms · ring {} ms", int(&a["delay_ms"]), n1(&a["ring_ms"])));
            let bad = |k: &str| p.good_bad(&int(&a[k]), a[k].as_u64() == Some(0));
            row(&mut out, "underruns", format!("{} · xruns {} · errors {}", bad("underruns"), bad("xruns"), bad("errors")));
        }
        Some("failed") => row(&mut out, "loopback", p.red(&format!("failed: {}", a["why"].as_str().unwrap_or("?")))),
        _ => row(&mut out, "loopback", format!("not running ({})", a["why"].as_str().unwrap_or("?"))),
    }

    out.push(String::new());
    out.push(format!("  {}", p.head("Recent events")));
    match s["events"].as_array() {
        Some(e) if !e.is_empty() => out.extend(e.iter().map(|l| format!("    {}", l.as_str().unwrap_or("?")))),
        _ => out.push(format!("    {}", p.dim("none"))),
    }
    out.push(String::new());
    out.push(format!("  {}", p.dim("Ctrl-C closes this window; the feed keeps running.")));
    out.join("\n")
}

/// `--dashboard`: draws the running viewer's numbers until it ends. Waits
/// a few seconds for a viewer that is still starting, as view.sh's does.
pub fn client(run_dir: &Path) -> Result<(), String> {
    let tty = io::IsTerminal::is_terminal(&io::stdout());
    let paint = Paint { on: tty };
    let path = socket_path(run_dir);
    let deadline = Instant::now() + CONNECT_WAIT;
    let stream = loop {
        match transport::connect(&path) {
            Ok(s) => break s,
            // No transport on this platform (Windows, #18): no use waiting.
            Err(e) if e.kind() == io::ErrorKind::Unsupported => return Err(e.to_string()),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                println!("No Rust viewer is running ({}: {e}).", path.display());
                if tty {
                    thread::sleep(Duration::from_secs(3));
                }
                return Err("no viewer to show".into());
            }
        }
    };
    stream.set_read_timeout(Some(STALE)).map_err(|e| format!("dashboard socket: {e}"))?;
    let mut lines = Lines { reader: BufReader::new(stream), buf: Vec::new() };
    let mut last: Option<Value> = None;
    let mut since = Instant::now();
    let mut out = io::stdout().lock();
    loop {
        let (text, note) = match lines.next() {
            Ok(Some(line)) => {
                since = Instant::now();
                let v: Value = serde_json::from_slice(&line).map_err(|e| format!("a line from the viewer: {e}"))?;
                if v["v"].as_u64() != Some(VERSION) {
                    return Err(format!("the viewer speaks dashboard version {}, this is {VERSION}: run the same build", v["v"]));
                }
                let text = render(&v, &paint);
                last = Some(v);
                (text, None)
            }
            Ok(None) => {
                println!("\n  {}", paint.good_bad("The feed has ended; closing.", false));
                if tty {
                    thread::sleep(Duration::from_secs(2));
                }
                return Ok(());
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => (
                last.as_ref().map_or_else(String::new, |v| render(v, &paint)),
                Some(format!("The viewer has not answered for {:.0} s.", since.elapsed().as_secs_f64())),
            ),
            Err(e) => return Err(format!("dashboard socket: {e}")),
        };
        let clear = if tty { "\x1b[H\x1b[2J" } else { "" };
        let note = note.map_or_else(String::new, |n| format!("\n  {}", paint.red(&n)));
        // A closed terminal ends the dashboard, nothing else.
        if writeln!(out, "{clear}{text}{note}").and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
}

/// Newline-ended lines that survive read timeouts: a partial line read
/// before one stays in `buf`.
struct Lines<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: Read> Lines<R> {
    /// The next line without its newline; None at the end of the stream.
    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            match self.reader.read_until(b'\n', &mut self.buf) {
                Ok(0) => return Ok(None),
                Ok(_) if self.buf.ends_with(b"\n") => {
                    let mut line = std::mem::take(&mut self.buf);
                    line.pop();
                    return Ok(Some(line));
                }
                // The end mid-line: the viewer died while writing it.
                Ok(_) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// A terminal to run `args` in, as lib/desktop.sh's desktop_open_terminal
/// picks one: xdg-terminal-exec, then $TERMINAL, then x-terminal-emulator.
#[cfg(unix)]
fn terminal_command(args: &[String], terminal: Option<&str>, have: impl Fn(&str) -> bool) -> Option<std::process::Command> {
    let (app, title) = ("protohdmicapture.debug", "protoHDMIcapture debug");
    if have("xdg-terminal-exec") {
        let mut c = std::process::Command::new("xdg-terminal-exec");
        c.args([format!("--app-id={app}"), format!("--title={title}"), "--".into()]).args(args);
        return Some(c);
    }
    for t in terminal.into_iter().chain(["x-terminal-emulator"]) {
        if !t.is_empty() && have(t) {
            let mut c = std::process::Command::new(t);
            c.arg("-e").args(args);
            return Some(c);
        }
    }
    None
}

/// An executable of that name on PATH.
#[cfg(unix)]
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| std::fs::metadata(d.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
    })
}

/// A terminal that exits within this of its start failed to show the
/// dashboard; a later exit is the user closing it (Ctrl-C, the window's
/// close button), whatever status the terminal passes on.
#[cfg(unix)]
const TERMINAL_START: Duration = Duration::from_secs(1);

/// Why the dashboard's terminal failed, from how long it ran and how it
/// ended; None for a terminal that ran and was closed.
#[cfg(unix)]
fn terminal_failed(ran: Duration, status: io::Result<std::process::ExitStatus>) -> Option<String> {
    match status {
        Ok(s) if s.success() || ran >= TERMINAL_START => None,
        Ok(s) => Some(s.to_string()),
        Err(e) => Some(e.to_string()),
    }
}

/// The d key on Windows: not yet (#18).
#[cfg(windows)]
pub fn open(_: Option<Notify>) -> Result<(), String> {
    Err(NOT_ON_WINDOWS.into())
}

/// The d key: `--dashboard` of this binary in a new terminal, in its own
/// session (a Ctrl-C meant for the viewer's terminal does not reach it).
/// Err: no terminal could be started. A terminal that fails later is
/// reported through `notify`.
#[cfg(unix)]
pub fn open(notify: Option<Notify>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let args = [exe.to_string_lossy().into_owned(), "--dashboard".into()];
    let terminal = std::env::var("TERMINAL").ok();
    let mut cmd = terminal_command(&args, terminal.as_deref(), on_path)
        .ok_or("no terminal found for the dashboard (xdg-terminal-exec, $TERMINAL or x-terminal-emulator)")?;
    use std::os::unix::process::CommandExt;
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // Safety: setsid is async-signal-safe. Our fds (video.lock, the
    // socket) are close-on-exec, so the terminal holds no lock of ours.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start a terminal for the dashboard: {e}"))?;
    let started = Instant::now();
    let name = cmd.get_program().to_string_lossy().into_owned();
    // Reaped here; a terminal that fails to start says so on the window.
    thread::spawn(move || {
        let status = child.wait();
        if let Some(why) = terminal_failed(started.elapsed(), status) {
            eprintln!("warning: the dashboard's terminal ({name}) failed: {why}");
            if let Some(n) = notify {
                n(Some((format!("Dashboard: {name} failed ({why})"), Duration::from_secs(3))));
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phc-dash-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_line(r: &mut Lines<transport::Stream>) -> Value {
        serde_json::from_slice(&r.next().unwrap().expect("a line")).unwrap()
    }

    #[test]
    fn samples_stay_bounded_with_no_server_taking_them() {
        let live = Live::new();
        for _ in 0..SAMPLES + 100 {
            live.presented(Duration::from_millis(10), Duration::from_millis(30), 0, None);
        }
        assert_eq!(live.samples.lock().unwrap().len(), SAMPLES);
        assert_eq!(live.presented.load(Ordering::Relaxed), (SAMPLES + 100) as u64, "every present still counted");
    }

    #[test]
    #[cfg(unix)]
    fn only_a_terminal_that_ends_at_once_failed() {
        use std::os::unix::process::ExitStatusExt;
        let exit = |code: i32| Ok(std::process::ExitStatus::from_raw(code << 8));
        let soon = Duration::from_millis(200);
        let later = Duration::from_secs(30);
        assert_eq!(terminal_failed(soon, exit(0)), None);
        assert_eq!(terminal_failed(soon, exit(1)).as_deref(), Some("exit status: 1"));
        // Ctrl-C in the dashboard, passed on by the terminal: closed, not failed.
        assert_eq!(terminal_failed(later, exit(130)), None);
        assert!(terminal_failed(later, Err(io::Error::other("wait"))).is_some());
    }

    #[test]
    fn two_runs_keep_their_numbers_apart() {
        // The wizard's feed window ends with gaps and no signal; the
        // viewer's run, after it, starts clean with no reset.
        let wizard = Live::new();
        let viewer = Live::new();
        wizard.frame();
        wizard.gap(1, 3, 1);
        wizard.signal(&Signal::NoSignal { since: Since::Stopped, reason: Reason::Nothing });
        wizard.dropped.unusable.fetch_add(1, Ordering::Relaxed);
        wizard.presented(Duration::from_millis(10), Duration::from_millis(30), 1, Some(Duration::from_micros(16_683)));
        assert_eq!(
            [&viewer.frames, &viewer.gaps, &viewer.lost, &viewer.presented, &viewer.repeated, &viewer.refresh_ns]
                .map(|n| n.load(Ordering::Relaxed)),
            [0; 6]
        );
        assert_eq!(*viewer.signal.lock().unwrap(), Signal::Picture);
        assert_eq!(viewer.dropped.unusable.load(Ordering::Relaxed), 0);
        assert!(viewer.samples.lock().unwrap().is_empty() && viewer.events.lock().unwrap().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn a_dashboard_gets_only_the_runs_it_was_given() {
        let dir = tmp("given");
        let viewer = Arc::new(Live::new());
        let wizard = Live::new();
        let server = Server::start(&socket_path(&dir), viewer.clone(), Duration::from_millis(20)).unwrap();
        let s = transport::connect(&socket_path(&dir)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = Lines { reader: BufReader::new(s), buf: Vec::new() };
        for _ in 0..5 {
            wizard.frame();
        }
        wizard.signal(&Signal::NoSignal { since: Since::Never, reason: Reason::Nothing });
        viewer.frame();
        let s = (0..250).map(|_| read_line(&mut r)).find(|v| v["frames"] == 1).expect("the viewer's frame arrives");
        assert_eq!(s["signal"], "ok", "not the wizard's no signal");
        assert!(s["events"].as_array().unwrap().is_empty(), "{s}");
        drop(server);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_dashboard_gets_the_live_numbers() {
        let dir = tmp("numbers");
        let live = Arc::new(Live::new());
        let server = Server::start(&socket_path(&dir), live.clone(), Duration::from_millis(100)).unwrap();
        let s = transport::connect(&socket_path(&dir)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = Lines { reader: BufReader::new(s), buf: Vec::new() };
        let first = read_line(&mut r);
        assert_eq!(first["v"], VERSION);
        assert_eq!(first["pid"], std::process::id());
        assert_eq!(first["signal"], "ok");
        assert!(first["dequeue_present"].is_null());
        assert_eq!(first["audio"]["state"], "off");

        for _ in 0..6 {
            live.frame();
        }
        live.gap(10, 13, 2);
        live.signal(&Signal::NoSignal { since: Since::Stopped, reason: Reason::Undecodable("not a JPEG".into()) });
        live.dropped.undecodable.fetch_add(4, Ordering::Relaxed);
        live.presented(Duration::from_millis(10), Duration::from_millis(30), 0, Some(Duration::from_micros(16_683)));
        live.presented(Duration::from_millis(12), Duration::from_millis(32), 2, None);
        let counters = Arc::new(Counters::default());
        counters.in_min.store(480, Ordering::Relaxed);
        counters.in_max.store(480, Ordering::Relaxed);
        // As Audio::start leaves it: no output callback yet.
        counters.out_min.store(u32::MAX, Ordering::Relaxed);
        counters.xruns.store(3, Ordering::Relaxed);
        counters.target_ms.store(80, Ordering::Relaxed);
        live.set_audio(AudioLive::Running { source: "card-in".into(), sink: "speakers".into(), buffer_ms: 10, counters });
        // The next snapshot after the changes.
        let s = (0..20).map(|_| read_line(&mut r)).find(|v| v["frames"] == 6 && !v["dequeue_present"].is_null()).expect("the numbers arrive");
        assert_eq!(s["gaps"], 1);
        assert_eq!(s["lost"], 2);
        assert_eq!(s["signal"], "stopped");
        assert_eq!(s["signal_why"], "MJPEG frames do not decode: not a JPEG");
        assert_eq!(s["dropped"], json!({ "undecodable": 4, "unusable": 0 }));
        assert_eq!(s["dequeue_present"]["n"], 2);
        assert_eq!(s["kernel_present"]["median_ms"], 32.0);
        assert_eq!(s["repeated_total"], 2);
        assert_eq!(s["repeated_window"], 2);
        assert!((s["refresh_hz"].as_f64().unwrap() - 59.94).abs() < 0.01);
        let a = &s["audio"];
        assert_eq!((a["state"].as_str(), a["source"].as_str(), a["sink"].as_str()), (Some("running"), Some("card-in"), Some("speakers")));
        assert_eq!(a["granted_in_ms"], json!([10.0, 10.0]));
        assert!(a["granted_out_ms"].is_null(), "no output callback yet");
        assert_eq!((a["xruns"].as_u64(), a["delay_ms"].as_u64()), (Some(3), Some(80)));
        let events: Vec<&str> = s["events"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
        assert!(events[0].ends_with("capture gap: sequence 10 -> 13, 2 lost"), "{events:?}");
        assert!(events[1].ends_with("no signal (stopped): MJPEG frames do not decode: not a JPEG"), "{events:?}");

        // The viewer ends: the dashboard sees the end, the socket goes.
        drop(server);
        loop {
            match r.next() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(!socket_path(&dir).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_dashboard_that_stops_reading_never_holds_the_viewer() {
        let dir = tmp("slow");
        let server = Server::start(&socket_path(&dir), Arc::new(Live::new()), Duration::from_millis(1)).unwrap();
        // Connected, never read: the server must keep its pace and drop it.
        let stuck = transport::connect(&socket_path(&dir)).unwrap();
        let healthy = transport::connect(&socket_path(&dir)).unwrap();
        healthy.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut r = Lines { reader: BufReader::new(healthy), buf: Vec::new() };
        // Thousands of ~1 KB lines: far past the socket buffer and BACKLOG.
        let mut got = 0;
        let started = Instant::now();
        while got < 3000 {
            read_line(&mut r);
            got += 1;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "the server kept going");
        // The stuck one was dropped: its socket reads to an end once drained.
        stuck.set_nonblocking(false).unwrap();
        stuck.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut sink = Vec::new();
        let mut s = stuck;
        s.read_to_end(&mut sink).expect("the server closed it");
        drop(server);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_dead_viewers_socket_is_replaced_and_only_ours_removed() {
        let dir = tmp("stale");
        let path = socket_path(&dir);
        // A killed viewer's socket: nobody listens on it.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let server = Server::start(&path, Arc::new(Live::new()), Duration::from_millis(50)).unwrap();
        transport::connect(&path).expect("the new one listens");
        // Replaced under us (it cannot be, with video.lock held; checked anyway).
        std::fs::remove_file(&path).unwrap();
        let other = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(server);
        assert!(path.exists(), "not ours: left");
        drop(other);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn lines_survive_a_timeout_mid_line() {
        let (mut a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut r = Lines { reader: BufReader::new(b), buf: Vec::new() };
        a.write_all(b"{\"v\":").unwrap();
        let e = r.next().unwrap_err();
        assert!(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut));
        a.write_all(b"1}\n{\"v\"").unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"{\"v\":1}");
        drop(a);
        assert_eq!(r.next().unwrap(), None, "the end mid-line is the end");
    }

    fn sample() -> Value {
        json!({
            "v": 1, "pid": 4242, "up_s": 12.4, "capture_fps": 60.0, "presented_fps": 59.9,
            "frames": 744, "gaps": 0, "lost": 0, "signal": "ok", "signal_why": null,
            "dropped": { "undecodable": 0, "unusable": 0 }, "window_s": 10.0,
            "dequeue_present": { "median_ms": 9.31, "p99_ms": 14.2, "n": 599 },
            "kernel_present": { "median_ms": 40.46, "p99_ms": 49.49, "n": 599 },
            "refresh_hz": 59.951, "repeated_window": 0, "repeated_total": 3,
            "cpu_pct": 7.25, "rss_mb": 180.0,
            "audio": { "state": "running", "source": "alsa_input.usb-MACROSILICON", "sink": "alsa_output.pci",
                       "buffer_ms": 10, "granted_in_ms": [10.0, 10.0], "granted_out_ms": [10.0, 21.3],
                       "delay_ms": 80, "ring_ms": 80.2, "underruns": 0, "xruns": 1, "errors": 0 },
            "events": ["    3.0 s  capture gap: sequence 179 -> 181, 1 lost"],
        })
    }

    #[test]
    fn the_dashboards_text() {
        let text = render(&sample(), &Paint { on: false });
        let want = [
            "  protoHDMIcapture · live   Rust viewer · pid 4242 · up 12 s",
            "  fps               60.0 captured · 59.9 presented · display 59.95 Hz",
            "  capture gaps      0 · 0 frames lost · 744 frames captured",
            "  signal            ok",
            "  dropped           0 · 0 undecodable · 0 unusable",
            "  dequeue→presented median 9.31 ms · p99 14.20 ms (last 10 s, 599 frames)",
            "  kernel→presented  median 40.46 ms · p99 49.49 ms (last 10 s, 599 frames)",
            "  repeated          0 refreshes (last 10 s) · 3 since start",
            "  process           cpu 7.2% · rss 180.0 MB",
            "  linked            alsa_input.usb-MACROSILICON → alsa_output.pci",
            "  buffer            asked 10 ms · got in 10.0 ms · out 10.0..21.3 ms",
            "  sync              delay 80 ms · ring 80.2 ms",
            "  underruns         0 · xruns 1 · errors 0",
            "        3.0 s  capture gap: sequence 179 -> 181, 1 lost",
            "  Ctrl-C closes this window; the feed keeps running.",
        ];
        for w in want {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }

        let mut s = sample();
        s["signal"] = json!("never");
        s["dequeue_present"] = Value::Null;
        s["audio"] = json!({ "state": "failed", "why": "the card has no PipeWire source" });
        s["events"] = json!([]);
        let text = render(&s, &Paint { on: false });
        for w in [
            "  signal            none since the feed opened",
            "  dequeue→presented no frame presented yet",
            "  loopback          failed: the card has no PipeWire source",
            "    none",
        ] {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }
        s["audio"] = json!({ "state": "off", "why": "AUDIO=none" });
        assert!(render(&s, &Paint { on: false }).contains("  loopback          not running (AUDIO=none)"));
        s["signal_why"] = json!("MJPEG frames do not decode: not a JPEG");
        s["dropped"] = json!({ "undecodable": 120, "unusable": 0 });
        let text = render(&s, &Paint { on: false });
        for w in ["  signal            no picture: MJPEG frames do not decode: not a JPEG", "  dropped           120 · 120 undecodable · 0 unusable"] {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn the_terminal_as_view_sh_picks_it() {
        let args = ["/x/protohdmicapture".to_string(), "--dashboard".into()];
        let argv = |c: std::process::Command| {
            std::iter::once(c.get_program().to_string_lossy().into_owned())
                .chain(c.get_args().map(|a| a.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let c = terminal_command(&args, Some("foot"), |_| true).unwrap();
        assert_eq!(
            argv(c),
            "xdg-terminal-exec --app-id=protohdmicapture.debug --title=protoHDMIcapture debug -- /x/protohdmicapture --dashboard"
        );
        let c = terminal_command(&args, Some("foot"), |n| n != "xdg-terminal-exec").unwrap();
        assert_eq!(argv(c), "foot -e /x/protohdmicapture --dashboard");
        let c = terminal_command(&args, Some("nope"), |n| n == "x-terminal-emulator").unwrap();
        assert_eq!(argv(c), "x-terminal-emulator -e /x/protohdmicapture --dashboard");
        assert!(terminal_command(&args, None, |_| false).is_none());
    }

    #[test]
    fn signal_as_view_shs_dashboard_names_it() {
        let no = |since, reason| Signal::NoSignal { since, reason };
        assert_eq!(signal_json(&Signal::Picture), ("ok", Value::Null));
        assert_eq!(signal_json(&no(Since::Never, Reason::Nothing)), ("never", Value::Null));
        assert_eq!(signal_json(&no(Since::Stopped, Reason::Nothing)), ("stopped", Value::Null));
        assert_eq!(signal_json(&no(Since::Never, Reason::Unusable("short".into()))), ("never", json!("frames the viewer cannot use: short")));
    }
}
