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
//! The transport is a Unix socket; on Windows (#19) a named pipe
//! (`\\.\pipe\protohdmicapture-dashboard-<run dir>`, `socket_path`) takes
//! its place, and the lines stay the same.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::audio::{Counters, RATE};
use crate::capture::{Dropped, Reason, Signal, Since};
use crate::desktop::{Desktop, fmt_g};
use crate::duplicate::{Cadence, Kind, Tracker};
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
/// The Display section reads the monitors at most this often: each read is
/// a `hyprctl` call.
const DISPLAY_EVERY: Duration = Duration::from_secs(5);

/// The socket in the run directory.
#[cfg(unix)]
pub fn socket_path(run_dir: &Path) -> PathBuf {
    run_dir.join("dashboard.sock")
}

/// The named pipe on Windows (#19): one per run directory, so one per user
/// (and per test), named after it.
#[cfg(windows)]
pub fn socket_path(run_dir: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\protohdmicapture-dashboard-{}", run_dir.display().to_string().replace(['\\', ':'], "-")))
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
    /// New and duplicate frames (duplicate.rs), as the capture thread
    /// tells them.
    new_frames: AtomicU64,
    duplicate_frames: AtomicU64,
    /// The source's cadence (`cadence_code`) and its breaks.
    cadence: AtomicU8,
    cadence_breaks: AtomicU64,
    /// The card's and the source's frame rates over the last second, as
    /// f64 bits (`NO_RATE`: not known yet).
    signal_hz: AtomicU64,
    source_fps: AtomicU64,
    /// Frame generation (framegen.rs), when the window runs it: its mode,
    /// the generated frames, the ones missed (not generated because the
    /// window was a refresh behind), the new frames shown real for any
    /// other reason (no steady 2:2, a scene cut), and the duplicates withheld
    /// (new-frames-only).
    framegen: Mutex<Option<&'static str>>,
    generated: AtomicU64,
    missed: AtomicU64,
    shown_real: AtomicU64,
    withheld: AtomicU64,
    /// GPU use (gpustat.rs, #72), while a GPU enhancement is on; None: no
    /// GPU section.
    gpu: Mutex<Option<Arc<crate::gpustat::Shared>>>,
    /// GameMode (gamemode.rs, #77), shown in the GPU section.
    gamemode: Mutex<Option<Arc<Mutex<crate::gamemode::Status>>>>,
    /// The compositor discards every present while frame callbacks keep
    /// coming: another present replaces ours before display (a
    /// frame-generation layer inserting its own, #72), so no latency can
    /// be read.
    replaced: AtomicBool,
}

/// A rate not known yet, in `Live`'s f64-bit fields (a NaN's bits).
const NO_RATE: u64 = u64::MAX;

fn cadence_code(c: Cadence) -> u8 {
    match c {
        Cadence::Measuring => 0,
        Cadence::TwoTwo => 1,
        Cadence::AllNew => 2,
        Cadence::Uneven => 3,
    }
}

fn cadence_of(code: u8) -> Cadence {
    match code {
        1 => Cadence::TwoTwo,
        2 => Cadence::AllNew,
        3 => Cadence::Uneven,
        _ => Cadence::Measuring,
    }
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
            new_frames: AtomicU64::new(0),
            duplicate_frames: AtomicU64::new(0),
            cadence: AtomicU8::new(cadence_code(Cadence::Measuring)),
            cadence_breaks: AtomicU64::new(0),
            signal_hz: AtomicU64::new(NO_RATE),
            source_fps: AtomicU64::new(NO_RATE),
            framegen: Mutex::new(None),
            generated: AtomicU64::new(0),
            missed: AtomicU64::new(0),
            shown_real: AtomicU64::new(0),
            withheld: AtomicU64::new(0),
            gpu: Mutex::new(None),
            gamemode: Mutex::new(None),
            replaced: AtomicBool::new(false),
        }
    }

    /// The GPU section's numbers while a GPU enhancement is on, none while
    /// it is off (gpustat.rs `Monitor`).
    pub fn set_gpu(&self, gpu: Option<Arc<crate::gpustat::Shared>>) {
        *self.gpu.lock().expect("the GPU section's lock is never poisoned") = gpu;
    }

    /// The window's GameMode status (gamemode.rs), for the GPU section.
    pub fn set_gamemode(&self, status: Arc<Mutex<crate::gamemode::Status>>) {
        *self.gamemode.lock().expect("the GameMode row's lock is never poisoned") = Some(status);
    }

    /// The GPU section is shown.
    #[cfg(test)]
    pub fn gpu_shown(&self) -> bool {
        self.gpu.lock().expect("the GPU section's lock is never poisoned").is_some()
    }

    /// The latency is shown as not measurable.
    #[cfg(test)]
    pub fn replaced_shown(&self) -> bool {
        self.replaced.load(Ordering::Relaxed)
    }

    /// Our presents are replaced before display, or no longer.
    pub fn set_replaced(&self, replaced: bool) {
        if self.replaced.swap(replaced, Ordering::Relaxed) != replaced && replaced {
            self.event("latency: not measurable: every present is replaced before display (a frame-generation layer?)".into());
        }
    }

    /// The window runs frame generation in this mode (framegen.rs).
    pub fn framegen(&self, mode: &'static str) {
        *self.framegen.lock().expect("the frame generation's lock is never poisoned") = Some(mode);
    }

    /// Frame generation was switched off mid-run (a dev build's `g`, #83).
    pub fn framegen_off(&self) {
        *self.framegen.lock().expect("the frame generation's lock is never poisoned") = None;
    }

    /// What frame generation did with one card frame: every new frame is
    /// generated, missed, or shown real (#78).
    pub fn framegen_step(&self, show: crate::framegen::Show, missed: bool, kind: Kind) {
        use crate::framegen::Show;
        let counter = match (show, missed, kind) {
            (_, true, _) => &self.missed,
            (Show::Generated, false, _) => &self.generated,
            (Show::Withheld, false, _) => &self.withheld,
            (Show::Real, false, Kind::New) => &self.shown_real,
            (Show::Real | Show::Held, false, _) => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Frame generation's mode and counts (generated, missed, shown real,
    /// withheld), while the window runs it.
    pub fn framegen_counts(&self) -> Option<(&'static str, [u64; 4])> {
        let mode = (*self.framegen.lock().expect("the frame generation's lock is never poisoned"))?;
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Some((mode, [n(&self.generated), n(&self.missed), n(&self.shown_real), n(&self.withheld)]))
    }

    /// The source's cadence, from the capture thread's tracker, which sees
    /// every card frame (`source`): frame generation's schedule reads it.
    pub fn cadence(&self) -> Cadence {
        cadence_of(self.cadence.load(Ordering::Relaxed))
    }

    /// A card frame's kind, and the source's cadence and rates after it.
    pub fn source(&self, kind: Kind, tracker: &Tracker) {
        match kind {
            Kind::New => self.new_frames.fetch_add(1, Ordering::Relaxed),
            Kind::Duplicate => self.duplicate_frames.fetch_add(1, Ordering::Relaxed),
        };
        self.cadence.store(cadence_code(tracker.cadence()), Ordering::Relaxed);
        self.cadence_breaks.store(tracker.breaks(), Ordering::Relaxed);
        let (signal, source) = tracker.rates().map_or((NO_RATE, NO_RATE), |(s, f)| (s.to_bits(), f.to_bits()));
        self.signal_hz.store(signal, Ordering::Relaxed);
        self.source_fps.store(source, Ordering::Relaxed);
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

    /// A presented card frame: its latencies (dequeue -> presented,
    /// kernel -> presented; none for a generated frame) and the refreshes
    /// it repeated.
    pub fn presented(&self, latency: Option<(Duration, Duration)>, repeated: u64, refresh: Option<Duration>) {
        self.presented.fetch_add(1, Ordering::Relaxed);
        self.repeated.fetch_add(repeated, Ordering::Relaxed);
        if let Some(r) = refresh {
            self.refresh_ns.store(r.as_nanos() as u64, Ordering::Relaxed);
        }
        let Some(sample) = latency else { return };
        let mut samples = self.samples.lock().expect("the samples' lock is never poisoned");
        if samples.len() < SAMPLES {
            samples.push(sample);
        }
    }

    fn up(&self) -> Duration {
        self.started.elapsed()
    }

    /// The counters, for the debug log's perf line (#27).
    pub fn counts(&self) -> crate::debuglog::Counts {
        crate::debuglog::Counts {
            frames: self.frames.load(Ordering::Relaxed),
            presented: self.presented.load(Ordering::Relaxed),
            repeated: self.repeated.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            lost: self.lost.load(Ordering::Relaxed),
            undecodable: self.dropped.undecodable.load(Ordering::Relaxed),
            unusable: self.dropped.unusable.load(Ordering::Relaxed),
        }
    }

    pub fn set_audio(&self, audio: AudioLive) {
        *self.audio.lock().expect("the audio's lock is never poisoned") = audio;
    }

    /// A line in the events list, stamped with the run's uptime.
    pub fn event(&self, text: String) {
        let mut e = self.events.lock().expect("the events' lock is never poisoned");
        if e.len() == EVENTS {
            e.pop_front();
        }
        e.push_back(format!("{:7.1} s  {text}", self.up().as_secs_f64()));
    }

    /// The audio's state as the dashboard shows it ("off", "failed",
    /// "running") and why, and the events without their stamps: what
    /// audio.rs's tests read back (#42).
    #[cfg(test)]
    pub fn audio_said(&self) -> (String, String, Vec<String>) {
        let audio = audio_json(&self.audio.lock().expect("the audio's lock is never poisoned"));
        let field = |k: &str| audio[k].as_str().unwrap_or_default().to_string();
        let events = self.events.lock().expect("the events' lock is never poisoned");
        (field("state"), field("why"), events.iter().map(|e| e.split_once(" s  ").map_or(e.clone(), |(_, t)| t.to_string())).collect())
    }
}

/// The Display section (#30), as view.sh's dashboard has it: the monitors
/// and which one the viewer is on, from the desktop adapter, and whether
/// REFRESH=match switched one, from its record in the run dir
/// (`refresh.restore`, docs/run-dir.md).
pub struct Display {
    desktop: Desktop,
    record: PathBuf,
    /// The last section, when it was read, and the record it was read with.
    last: Option<(Instant, Option<String>, Value)>,
}

impl Display {
    pub fn new(desktop: Desktop, record: PathBuf) -> Self {
        Self { desktop, record, last: None }
    }

    /// The section: read again once `DISPLAY_EVERY` has passed, or at once
    /// when the refresh record changed (a switch, or its restore).
    fn json(&mut self, now: Instant) -> Value {
        let record = match std::fs::read_to_string(&self.record) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", self.record.display())),
        };
        let seen = record.clone().unwrap_or_else(Some);
        if let Some((at, was, v)) = &self.last
            && now.duration_since(*at) < DISPLAY_EVERY
            && *was == seen
        {
            return v.clone();
        }
        // Decided not on Windows (#38): neither the monitors nor REFRESH=match.
        let v = if cfg!(windows) { not_on_windows() } else { display_json(self.desktop.monitors(std::process::id()), record) };
        self.last = Some((now, seen, v.clone()));
        v
    }
}

/// The Display section on Windows: what it would show is not there (#38).
fn not_on_windows() -> Value {
    json!({
        "monitors": null,
        "why": "they are known on Hyprland only, not on Windows",
        "refresh": { "unsupported": "REFRESH=match is not on Windows" },
    })
}

/// The Display section's JSON: the monitors, or why they are not known;
/// the refresh switch, if REFRESH=match made one.
fn display_json(monitors: Result<Vec<crate::desktop::Monitor>, String>, record: Result<Option<String>, String>) -> Value {
    let (monitors, why) = match monitors {
        Ok(m) => (
            json!(
                m.iter()
                    .map(|m| json!({ "name": m.name, "width": m.width, "height": m.height,
                                     "refresh_hz": m.refresh_hz, "focused": m.focused, "viewer": m.viewer }))
                    .collect::<Vec<_>>()
            ),
            Value::Null,
        ),
        Err(why) => (Value::Null, json!(why)),
    };
    // `monitor|rate set|original rate|restore command`.
    let refresh = match record {
        Ok(None) => Value::Null,
        Ok(Some(text)) => match text.lines().next().unwrap_or("").splitn(4, '|').collect::<Vec<_>>()[..] {
            [monitor, set, was, _] => json!({ "monitor": monitor, "set_hz": set, "was_hz": was }),
            _ => json!({ "unreadable": format!("unreadable refresh record: {}", text.trim()) }),
        },
        Err(why) => json!({ "unreadable": why }),
    };
    json!({ "monitors": monitors, "why": why, "refresh": refresh })
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
        let picture = *live.signal.lock().expect("the signal's lock is never poisoned") == Signal::Picture;
        let (signal, signal_why) = signal_json(&live.signal.lock().expect("the signal's lock is never poisoned"));
        // With no picture the last rates are stale: none.
        let rate = |a: &AtomicU64| match a.load(Ordering::Relaxed) {
            bits if bits != NO_RATE && picture => json!(f64::from_bits(bits)),
            _ => Value::Null,
        };

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
            "source": {
                "new": live.new_frames.load(Ordering::Relaxed),
                "duplicate": live.duplicate_frames.load(Ordering::Relaxed),
                "fps": rate(&live.source_fps),
                "signal_hz": rate(&live.signal_hz),
                "cadence": cadence_of(live.cadence.load(Ordering::Relaxed)).as_str(),
                "breaks": live.cadence_breaks.load(Ordering::Relaxed),
            },
            "framegen": match *live.framegen.lock().expect("the frame generation's lock is never poisoned") {
                Some(mode) => json!({
                    "mode": mode,
                    "generated": live.generated.load(Ordering::Relaxed),
                    "missed": live.missed.load(Ordering::Relaxed),
                    "shown_real": live.shown_real.load(Ordering::Relaxed),
                    "withheld": live.withheld.load(Ordering::Relaxed),
                }),
                None => Value::Null,
            },
            "gpu": match &*live.gpu.lock().expect("the GPU section's lock is never poisoned") {
                Some(g) => {
                    let mut j = crate::gpustat::json(g);
                    if let Some(status) = &*live.gamemode.lock().expect("the GameMode row's lock is never poisoned") {
                        j["gamemode"] = status.lock().expect("the GameMode status's lock is never poisoned").json();
                    }
                    j
                }
                None => Value::Null,
            },
            "latency_replaced": live.replaced.load(Ordering::Relaxed),
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
                "realtime_refused": c.realtime_refused.load(Ordering::Relaxed),
            })
        }
    }
}

/// This process's user and system CPU time, in clock ticks.
#[cfg(target_os = "linux")]
pub(crate) fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // After the command name, which may hold spaces: fields 3 onwards.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

#[cfg(target_os = "linux")]
pub(crate) fn clock_ticks_per_s() -> f64 {
    // Safety: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

#[cfg(target_os = "linux")]
pub(crate) fn rss_mb() -> Option<f64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // Safety: sysconf has no preconditions.
    Some(pages as f64 * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64 / 1048576.0)
}

/// This process's user and system CPU time on Windows, in 100 ns ticks.
#[cfg(windows)]
pub(crate) fn cpu_ticks() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // Safety: this process's pseudo-handle and four out-pointers.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some(ticks(kernel) + ticks(user))
}

#[cfg(windows)]
pub(crate) fn clock_ticks_per_s() -> f64 {
    1e7
}

/// The working set, Windows' resident memory.
#[cfg(windows)]
pub(crate) fn rss_mb() -> Option<f64> {
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // Safety: this process's pseudo-handle and a sized out-struct.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) }.ok().ok()?;
    Some(c.WorkingSetSize as f64 / 1048576.0)
}

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

    pub fn set_read_timeout(s: &mut Stream, t: Option<std::time::Duration>) -> std::io::Result<()> {
        s.set_read_timeout(t)
    }
}

/// The named pipe (Windows, #19), shaped as the Unix socket is for the
/// server and the client: a listener whose `accept` never waits, streams
/// whose writes never wait, and a read timeout for the client.
///
/// - The server's instances are `PIPE_NOWAIT`: `ConnectNamedPipe` says at
///   once whether a dashboard came, and a write puts what fits in the
///   pipe's buffer. Each connection gets a fresh instance to listen on.
/// - The access list grants the owner and SYSTEM only (the socket's 0600);
///   remote clients are refused. The pipe goes with the last handle, so
///   nothing is left behind and `inode` has nothing to compare.
/// - The client reads in wait mode, after `PeekNamedPipe` says bytes are
///   there, so a read timeout is a poll.
#[cfg(windows)]
mod transport {
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE, HLOCAL,
        INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_NONE, OPEN_EXISTING,
        PIPE_ACCESS_OUTBOUND,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_NOWAIT, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PeekNamedPipe, WaitNamedPipeW,
    };
    use windows::core::{HRESULT, HSTRING, w};

    /// The pipe's buffer: a minute of lines is the server's own limit.
    const BUFFER: u32 = 64 * 1024;
    /// How often a client read looks for bytes.
    const POLL: Duration = Duration::from_millis(20);
    /// How long a client waits for a free instance: over a server tick.
    const BUSY_WAIT_MS: u32 = 2000;

    fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
        e.code() == HRESULT::from_win32(code.0)
    }

    /// The owner and SYSTEM, nobody else: a security descriptor from SDDL,
    /// freed with the listener.
    struct OwnerOnly(PSECURITY_DESCRIPTOR);

    // Safety: the descriptor is plain memory, read by CreateNamedPipeW only.
    unsafe impl Send for OwnerOnly {}

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // Safety: LocalAlloc'd by ConvertStringSecurityDescriptor..., freed once.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    pub struct Listener {
        name: HSTRING,
        security: OwnerOnly,
        /// The instance waiting for the next dashboard.
        next: RefCell<File>,
    }

    fn instance(name: &HSTRING, security: &OwnerOnly, first: bool) -> io::Result<File> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0.0,
            bInheritHandle: false.into(),
        };
        let mode = if first { PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_OUTBOUND };
        // Safety: a NUL-terminated name and attributes that outlive the call.
        let h = unsafe {
            CreateNamedPipeW(
                name,
                mode,
                PIPE_TYPE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                0,
                0,
                Some(&sa),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Safety: a fresh handle this File now owns.
        Ok(unsafe { File::from_raw_handle(h.0) })
    }

    /// The pipe at `path`, its first instance ours alone (another process
    /// with it fails this).
    pub fn listen(path: &Path) -> io::Result<(Listener, u64)> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // Safety: an out-pointer; the descriptor is freed by OwnerOnly.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:P(A;;GA;;;OW)(A;;GA;;;SY)"), SDDL_REVISION_1, &mut sd, None) }
            .map_err(io::Error::from)?;
        let security = OwnerOnly(sd);
        let name = HSTRING::from(path.as_os_str());
        let next = RefCell::new(instance(&name, &security, true)?);
        Ok((Listener { name, security, next }, 0))
    }

    impl Listener {
        /// A dashboard that has connected, or WouldBlock.
        pub fn accept(&self) -> io::Result<(Stream, ())> {
            let handle = HANDLE(self.next.borrow().as_raw_handle());
            // Safety: a live pipe instance; no overlapped I/O.
            match unsafe { ConnectNamedPipe(handle, None) } {
                // NO_DATA: it came and went already; its first write fails.
                Err(e) if is(&e, ERROR_PIPE_CONNECTED) || is(&e, ERROR_NO_DATA) => {}
                Err(e) if is(&e, ERROR_PIPE_LISTENING) => return Err(io::ErrorKind::WouldBlock.into()),
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            let fresh = instance(&self.name, &self.security, false)?;
            Ok((Stream { file: self.next.replace(fresh), read_timeout: None }, ()))
        }
    }

    pub fn inode(_: &Path) -> Option<u64> {
        None
    }

    pub struct Stream {
        file: File,
        read_timeout: Option<Duration>,
    }

    impl Stream {
        /// The server's instances never wait already.
        pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn set_read_timeout(s: &mut Stream, t: Option<Duration>) -> io::Result<()> {
        s.read_timeout = t;
        Ok(())
    }

    impl Write for Stream {
        /// A full pipe takes nothing: WouldBlock, as a socket says it.
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self.file.write(buf)? {
                0 if !buf.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
                n => Ok(n),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Stream {
        /// Waits for bytes up to the read timeout; the server gone is the
        /// end of the stream.
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let start = Instant::now();
            loop {
                let mut available = 0u32;
                // Safety: a live pipe handle and an out-pointer.
                match unsafe { PeekNamedPipe(HANDLE(self.file.as_raw_handle()), None, 0, None, Some(&mut available), None) } {
                    Err(e) if is(&e, ERROR_BROKEN_PIPE) => return Ok(0),
                    Err(e) => return Err(e.into()),
                    Ok(()) if available > 0 => return self.file.read(buf),
                    Ok(()) => {}
                }
                if self.read_timeout.is_some_and(|t| start.elapsed() >= t) {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                thread::sleep(POLL);
            }
        }
    }

    /// The viewer's pipe, read only. Every instance busy (the server makes
    /// the next one as it accepts, once a tick) is waited out for a while;
    /// not there is an error the client waits out.
    pub fn connect(path: &Path) -> io::Result<Stream> {
        let name = HSTRING::from(path.as_os_str());
        loop {
            // Safety: a NUL-terminated name; the handle goes to the File.
            match unsafe {
                CreateFileW(&name, FILE_GENERIC_READ.0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
            } {
                // Safety: a fresh handle this File now owns.
                Ok(h) => return Ok(Stream { file: unsafe { File::from_raw_handle(h.0) }, read_timeout: None }),
                // Safety: a NUL-terminated name.
                Err(e) if is(&e, ERROR_PIPE_BUSY) => unsafe { WaitNamedPipeW(&name, BUSY_WAIT_MS) }.ok().map_err(io::Error::from)?,
                Err(e) => return Err(e.into()),
            }
        }
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
    /// Listens on `path` and sends `live` to each dashboard every `tick`,
    /// with the Display section when there is a `display`.
    pub fn start(path: &Path, live: Arc<Live>, display: Option<Display>, tick: Duration) -> Result<Self, String> {
        let (listener, inode) = transport::listen(path).map_err(|e| format!("dashboard socket {}: {e}", path.display()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("dashboard".into())
                .spawn(move || serve(listener, &live, display, tick, &stop))
                .map_err(|e| format!("dashboard thread: {e}"))?
        };
        Ok(Self { path: path.to_path_buf(), inode, stop, thread: Some(thread) })
    }
}

fn serve(listener: transport::Listener, live: &Live, mut display: Option<Display>, tick: Duration, stop: &AtomicBool) {
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
        let mut snap = rolling.snapshot(live);
        // Read only for a dashboard to show it to: it costs a hyprctl call.
        if !clients.is_empty()
            && let Some(d) = display.as_mut()
        {
            snap["display"] = d.json(now);
        }
        let mut line = snap.to_string().into_bytes();
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

/// Orange (xterm 256-colour 208), for warnings (#76).
const WARN: &str = "38;5;208";
/// Red (xterm 256-colour 196), for errors (#76).
const ERROR: &str = "38;5;196";

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
    /// Something to look at, the feed and the sound still running: orange.
    /// Warnings and errors use fixed 256-colour codes, not the 16 palette
    /// slots a terminal theme repaints: Omarchy themes set "red" (31) and
    /// "yellow" (33) to greens and cyans, and a warning showed green (#76).
    fn warn(&self, t: &str) -> String {
        self.wrap(WARN, t)
    }
    /// The feed or the sound has stopped: red.
    fn error(&self, t: &str) -> String {
        self.wrap(ERROR, t)
    }
    /// The palette's green when `ok`, a warning when not.
    fn good_warn(&self, t: &str, ok: bool) -> String {
        if ok { self.good(t) } else { self.warn(t) }
    }
    fn good(&self, t: &str) -> String {
        self.wrap("32", t)
    }
    /// By a known threshold's verdict; no threshold, no colour.
    fn level(&self, t: &str, level: Option<Level>) -> String {
        match level {
            Some(Level::Good) => self.good(t),
            Some(Level::Warn) => self.warn(t),
            Some(Level::Error) => self.error(t),
            None => t.to_string(),
        }
    }
}

/// A value against a known threshold (#76): only thresholds the code, the
/// docs or the hardware state; a value with none is not coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Good,
    Warn,
    Error,
}

/// A reading against the hardware's own limits: at or over `emergency` an
/// error, at or over `crit` a warning, under both good; None with neither
/// known.
fn against_limits(v: f64, crit: Option<f64>, emergency: Option<f64>) -> Option<Level> {
    if crit.is_none() && emergency.is_none() {
        return None;
    }
    Some(if emergency.is_some_and(|e| v >= e) {
        Level::Error
    } else if crit.is_some_and(|c| v >= c) {
        Level::Warn
    } else {
        Level::Good
    })
}

/// VRAM used as a share of the total: orange from here, red from
/// `VRAM_ERROR` (#76, the user's choice).
const VRAM_WARN: f64 = 0.90;
const VRAM_ERROR: f64 = 0.97;

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// The GPU section (gpustat.rs, #72): only while a GPU enhancement is on
/// (`gpu` null otherwise). The window's median, and its max where it
/// moves.
fn gpu_section(out: &mut Vec<String>, g: &Value, p: &Paint) {
    if g.is_null() {
        return;
    }
    let row = |out: &mut Vec<String>, name: &str, value: String| out.push(format!("  {} {value}", p.dim(&format!("{name:<17}"))));
    let med = |k: &str| num(&g[k]["median"]);
    let mm = |k: &str, unit: &str| match (num(&g[k]["median"]), num(&g[k]["max"])) {
        (Some(m), Some(x)) => Some(format!("{m:.0}{unit} {}", p.dim(&format!("(max {x:.0}{unit})")))),
        _ => None,
    };
    out.push(String::new());
    out.push(format!("  {}", p.head("GPU")));
    match (g["source"].as_str(), g["unavailable"].as_str()) {
        (Some(src), _) => row(out, "read from", p.dim(&format!("{src} · last {:.0} s", crate::gpustat::WINDOW.as_secs_f64()))),
        (_, Some(why)) => row(out, "whole GPU", p.dim(&format!("not available: {why}"))),
        _ => {}
    }
    if let Some(busy) = mm("busy_pct", " %") {
        row(out, "busy", busy);
    }
    let ours = match (mm("ours_pct", " %"), num(&g["ours_vram_mib"])) {
        (Some(pct), Some(v)) => Some(format!("{pct} of the engines · {v:.0} MB VRAM")),
        (Some(pct), None) => Some(format!("{pct} of the engines")),
        (None, Some(v)) => Some(format!("{v:.0} MB VRAM")),
        (None, None) => None,
    };
    row(out, "protoHDMIcapture", ours.unwrap_or_else(|| p.dim("no DRM engine counters (fdinfo)")));
    let clocks: Vec<String> = [("shader", med("shader_mhz")), ("memory", med("memory_mhz"))]
        .into_iter()
        .filter_map(|(n, v)| v.map(|v| format!("{n} {v:.0} MHz")))
        .collect();
    if !clocks.is_empty() {
        row(out, "clocks", clocks.join(" · "));
    }
    // GameMode (#77), next to the clocks it is asked to raise: active is
    // good, none on the system dim, a refusal or failed call a warning.
    let gm = &g["gamemode"];
    let why = gm["why"].as_str().unwrap_or("?");
    match gm["state"].as_str() {
        Some("active") => row(out, "GameMode", p.good("active for protoHDMIcapture")),
        Some("asking") => row(out, "GameMode", p.dim("asking")),
        Some("registered") => {
            row(out, "GameMode", p.warn(&format!("registered, not active (status {})", gm["query"].as_i64().unwrap_or(-1))))
        }
        Some("unavailable") => row(out, "GameMode", p.dim(&format!("not available: {why}"))),
        Some("failed") => row(out, "GameMode", p.warn(&format!("Warning: {why}"))),
        _ => {}
    }
    if let (Some(used), Some(total)) = (num(&g["vram_used_mib"]), num(&g["vram_total_mib"])) {
        // Near-full VRAM makes the driver evict to system RAM (stutter): the
        // user's thresholds (#76).
        let share = if total > 0.0 { used / total } else { 0.0 };
        let level = if share >= VRAM_ERROR {
            Level::Error
        } else if share >= VRAM_WARN {
            Level::Warn
        } else {
            Level::Good
        };
        row(out, "VRAM", p.level(&format!("{:.1} of {:.1} GB", used / 1024.0, total / 1024.0), Some(level)));
    }
    // At the board's power cap (hwmon power1_cap) the clocks are held back:
    // a warning; under it good (#76).
    let cap = num(&g["limits"]["power_cap_w"]);
    let capped = |w: f64| cap.map(|c| if w >= c { Level::Warn } else { Level::Good });
    if let (Some(m), Some(x)) = (med("power_w"), num(&g["power_w"]["max"])) {
        row(
            out,
            "power",
            format!(
                "{} {}{}{}",
                p.level(&format!("{m:.0} W"), capped(m)),
                p.dim("(max "),
                p.level(&format!("{x:.0} W"), capped(x)),
                p.dim(")")
            ),
        );
    }
    // Each sensor against its own hwmon temp*_crit / temp*_emergency (#76).
    let temps: Vec<String> = [("edge", "edge_c"), ("junction", "junction_c"), ("memory", "memory_c")]
        .into_iter()
        .filter_map(|(n, k)| {
            med(k).map(|v| {
                let l = &g["limits"][k];
                format!("{n} {}", p.level(&format!("{v:.0} °C"), against_limits(v, num(&l["crit"]), num(&l["emergency"]))))
            })
        })
        .collect();
    if !temps.is_empty() {
        row(out, "temperature", temps.join(" · "));
    }
    if let Some(f) = med("fan_rpm") {
        row(out, "fan", format!("{f:.0} RPM"));
    }
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
        p.dim(&format!("running · pid {} · up {:.0} s", int(&s["pid"]), num(&s["up_s"]).unwrap_or(0.0)))
    ));

    out.push(String::new());
    out.push(format!("  {}", p.head("Video")));
    // New-frames-only presents the source's frames, not the card's
    // (framegen.rs): it is held to the source's rate.
    let fps = match s["framegen"]["mode"].as_str() {
        Some("new-only") => num(&s["source"]["fps"]).unwrap_or(0.0),
        _ => num(&s["capture_fps"]).unwrap_or(0.0),
    };
    let shown = num(&s["presented_fps"]).unwrap_or(0.0);
    row(
        &mut out,
        "fps",
        format!(
            "{} captured · {} presented{}",
            n1(&s["capture_fps"]),
            p.good_warn(&n1(&s["presented_fps"]), fps == 0.0 || shown >= fps * 0.95),
            match num(&s["refresh_hz"]) {
                Some(hz) => format!(" · display {hz:.2} Hz"),
                None => String::new(),
            }
        ),
    );
    let src = &s["source"];
    row(
        &mut out,
        "source",
        match (num(&src["fps"]), num(&src["signal_hz"])) {
            (Some(fps), Some(hz)) => {
                let breaks = src["breaks"].as_u64().unwrap_or(0);
                format!(
                    "{fps:.1} fps in a {hz:.2} Hz signal ({}) · {} cadence break{}",
                    src["cadence"].as_str().unwrap_or("?"),
                    p.good_warn(&breaks.to_string(), breaks == 0),
                    if breaks == 1 { "" } else { "s" }
                )
            }
            _ => p.dim("no picture yet"),
        },
    );
    let fg = &s["framegen"];
    if let Some(mode) = fg["mode"].as_str() {
        let missed = fg["missed"].as_u64().unwrap_or(0);
        row(
            &mut out,
            "frame generation",
            format!(
                "{mode} · {} generated · {} missed · {} shown real · {} withheld",
                int(&fg["generated"]),
                p.good_warn(&missed.to_string(), missed == 0),
                int(&fg["shown_real"]),
                int(&fg["withheld"])
            ),
        );
    }
    let gaps = s["gaps"].as_u64().unwrap_or(0);
    row(
        &mut out,
        "capture gaps",
        format!("{} · {} frames lost · {} frames captured", p.good_warn(&int(&s["gaps"]), gaps == 0), int(&s["lost"]), int(&s["frames"])),
    );
    row(
        &mut out,
        "signal",
        match (s["signal"].as_str(), s["signal_why"].as_str()) {
            (Some("ok"), _) => p.good("ok"),
            (Some("never"), None) => p.error("none since the feed opened"),
            (Some("stopped"), None) => p.error("stopped (source asleep or off?)"),
            (Some("never" | "stopped"), Some(why)) => p.error(&format!("no picture: {why}")),
            _ => "?".into(),
        },
    );
    let (undecodable, unusable) = (s["dropped"]["undecodable"].as_u64().unwrap_or(0), s["dropped"]["unusable"].as_u64().unwrap_or(0));
    row(
        &mut out,
        "dropped",
        format!(
            "{} · {} undecodable · {} unusable",
            p.good_warn(&(undecodable + unusable).to_string(), undecodable + unusable == 0),
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
            if v.is_null() && s["latency_replaced"].as_bool() == Some(true) {
                p.warn("not measurable: every present is replaced before display (a frame-generation layer?)")
            } else if v.is_null() {
                p.dim("no frame presented yet")
            } else {
                // dequeue -> presented against #5's go criteria; kernel ->
                // presented against the reference baseline, a refresh more
                // while frame generation makes frames (stats.rs, #76).
                use crate::stats::{BASELINE_KERNEL_MEDIAN_MS, BASELINE_KERNEL_P99_MS, FRAMEGEN_EXTRA_MS, GATE_MEDIAN_MS, GATE_P99_MS};
                let generating = s["framegen"]["mode"].as_str().is_some_and(|m| m != "new-only");
                let extra = if generating { FRAMEGEN_EXTRA_MS } else { 0.0 };
                let (median, p99) = if key == "dequeue_present" {
                    (GATE_MEDIAN_MS, GATE_P99_MS)
                } else {
                    (BASELINE_KERNEL_MEDIAN_MS + extra, BASELINE_KERNEL_P99_MS + extra)
                };
                let gate = |k: &str, limit: f64| num(&v[k]).map(|x| if x <= limit { Level::Good } else { Level::Warn });
                format!(
                    "median {} · p99 {} {}",
                    p.level(&format!("{} ms", n2(&v["median_ms"])), gate("median_ms", median)),
                    p.level(&format!("{} ms", n2(&v["p99_ms"])), gate("p99_ms", p99)),
                    p.dim(&format!("({window}, {} frames)", int(&v["n"])))
                )
            },
        );
    }
    let rep = s["repeated_window"].as_u64().unwrap_or(0);
    row(
        &mut out,
        "repeated",
        format!(
            "{} refreshes {} · {} since start",
            p.good_warn(&int(&s["repeated_window"]), rep == 0),
            p.dim(&format!("({window})")),
            int(&s["repeated_total"])
        ),
    );
    row(&mut out, "process", format!("cpu {}% · rss {} MB", n1(&s["cpu_pct"]), n1(&s["rss_mb"])));
    gpu_section(&mut out, &s["gpu"], p);

    out.push(String::new());
    out.push(format!("  {}", p.head("Audio")));
    let a = &s["audio"];
    match a["state"].as_str() {
        Some("running") => {
            row(&mut out, "linked", format!("{} → {}", a["source"].as_str().unwrap_or("?"), a["sink"].as_str().unwrap_or("?")));
            // What the sound system granted against what was asked: more
            // than asked is more delay than the audio sync expects, a
            // warning; up to it good (#76).
            let asked = num(&a["buffer_ms"]);
            let within = |hi: f64| asked.map(|b| if hi <= b + 0.05 { Level::Good } else { Level::Warn });
            let granted = |v: &Value| match v.as_array().map(|r| (num(&r[0]), num(&r[1]))) {
                Some((Some(lo), Some(hi))) if (lo - hi).abs() < 0.05 => p.level(&format!("{lo:.1} ms"), within(hi)),
                Some((Some(lo), Some(hi))) => p.level(&format!("{lo:.1}..{hi:.1} ms"), within(hi)),
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
            let bad = |k: &str| p.good_warn(&int(&a[k]), a[k].as_u64() == Some(0));
            row(&mut out, "underruns", format!("{} · xruns {} · errors {}", bad("underruns"), bad("xruns"), bad("errors")));
            if a["realtime_refused"].as_bool() == Some(true) {
                row(&mut out, "priority", p.warn("real-time refused; sound may crackle under load"));
            }
        }
        Some("failed") => row(&mut out, "loopback", p.error(&format!("failed: {}", a["why"].as_str().unwrap_or("?")))),
        _ => row(&mut out, "loopback", format!("not running ({})", a["why"].as_str().unwrap_or("?"))),
    }

    // Not in a snapshot from a viewer with no Display section to send.
    let d = &s["display"];
    if d.is_object() {
        out.push(String::new());
        out.push(format!("  {}", p.head("Display")));
        match d["monitors"].as_array() {
            // legacy/lib/desktop.sh desktop_outputs: "name WxH@HzHz [focused]".
            Some(ms) => {
                for m in ms {
                    let rate = num(&m["refresh_hz"]).map_or("?".to_string(), |r| fmt_g((r * 100.0).round() / 100.0));
                    let mut line = format!("{} {}x{}@{rate}Hz", m["name"].as_str().unwrap_or("?"), int(&m["width"]), int(&m["height"]));
                    if m["focused"].as_bool() == Some(true) {
                        line.push_str(" focused");
                    }
                    if m["viewer"].as_bool() == Some(true) {
                        line.push_str(&p.dim(" · the viewer's"));
                    }
                    row(&mut out, "monitor", line);
                }
            }
            None => row(&mut out, "monitors", p.dim(&format!("not known: {}", d["why"].as_str().unwrap_or("?")))),
        }
        let r = &d["refresh"];
        row(
            &mut out,
            "refresh",
            if r.is_null() {
                p.dim("not switched")
            } else if let Some(why) = r["unsupported"].as_str() {
                p.dim(why)
            } else if let Some(why) = r["unreadable"].as_str() {
                p.warn(why)
            } else {
                format!(
                    "{} at {} Hz by REFRESH=match {}",
                    r["monitor"].as_str().unwrap_or("?"),
                    r["set_hz"].as_str().unwrap_or("?"),
                    p.dim(&format!("(back to {} Hz on exit)", r["was_hz"].as_str().unwrap_or("?")))
                )
            },
        );
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
    #[cfg(windows)]
    if tty {
        enable_vt();
    }
    let paint = Paint { on: tty };
    let path = socket_path(run_dir);
    let deadline = Instant::now() + CONNECT_WAIT;
    let mut stream = loop {
        match transport::connect(&path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                println!("protoHDMIcapture is not running ({}: {e}).", path.display());
                if tty {
                    thread::sleep(Duration::from_secs(3));
                }
                return Err("no viewer to show".into());
            }
        }
    };
    transport::set_read_timeout(&mut stream, Some(STALE)).map_err(|e| format!("dashboard socket: {e}"))?;
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
                println!("\n  {}", paint.good_warn("The feed has ended; closing.", false));
                if tty {
                    thread::sleep(Duration::from_secs(2));
                }
                return Ok(());
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => (
                last.as_ref().map_or_else(String::new, |v| render(v, &paint)),
                Some(format!("protoHDMIcapture has not answered for {:.0} s.", since.elapsed().as_secs_f64())),
            ),
            Err(e) => return Err(format!("dashboard socket: {e}")),
        };
        // The title too, each time: the viewer knows its window by it
        // where the terminal took no app id (`$TERMINAL -e`).
        let clear = if tty { format!("\x1b]0;{}\x07\x1b[H\x1b[2J", crate::desktop::DASHBOARD_TITLE) } else { String::new() };
        let note = note.map_or_else(String::new, |n| format!("\n  {}", paint.warn(&n)));
        // A closed terminal ends the dashboard, nothing else.
        if writeln!(out, "{clear}{text}{note}").and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
}

/// The console reads the escapes the drawing uses (colour, clear, title):
/// Windows Terminal does already, conhost only when asked.
#[cfg(windows)]
fn enable_vt() {
    use windows::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // Safety: this process's own console handle, read then set.
    unsafe {
        let mut mode = Default::default();
        let set = GetStdHandle(STD_OUTPUT_HANDLE)
            .and_then(|out| GetConsoleMode(out, &mut mode).and_then(|()| SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)));
        if let Err(e) = set {
            eprintln!("warning: the console shows the dashboard's colours as text: {e}");
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

/// A terminal to run `args` in, as legacy/lib/desktop.sh's desktop_open_terminal
/// picks one: xdg-terminal-exec, then $TERMINAL, then x-terminal-emulator.
#[cfg(unix)]
fn terminal_command(args: &[String], terminal: Option<&str>, have: impl Fn(&str) -> bool) -> Option<std::process::Command> {
    let (app, title) = (crate::desktop::DASHBOARD_APP, crate::desktop::DASHBOARD_TITLE);
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
const TERMINAL_START: Duration = Duration::from_secs(1);

/// Why the dashboard's terminal failed, from how long it ran and how it
/// ended; None for a terminal that ran and was closed.
fn terminal_failed(ran: Duration, status: io::Result<std::process::ExitStatus>) -> Option<String> {
    match status {
        Ok(s) if s.success() || ran >= TERMINAL_START => None,
        Ok(s) => Some(s.to_string()),
        Err(e) => Some(e.to_string()),
    }
}

/// Reaps the terminal `child`; one that fails to start says so on the window.
fn reap(mut child: std::process::Child, name: String, notify: Option<Notify>) {
    let started = Instant::now();
    thread::spawn(move || {
        let status = child.wait();
        if let Some(why) = terminal_failed(started.elapsed(), status) {
            eprintln!("warning: the dashboard's terminal ({name}) failed: {why}");
            if let Some(n) = notify {
                n(Some((format!("Dashboard: {name} failed ({why})"), Duration::from_secs(3))));
            }
        }
    });
}

/// The d key on Windows (#19): `--dashboard` of this binary in a console
/// of its own, through cmd's `start` (the desktop's terminal, Windows
/// Terminal or conhost). Started directly, the child would get the
/// viewer's own standard handles and draw into its terminal; cmd itself
/// gets no window, and its exit is `start`'s, at once.
#[cfg(windows)]
pub fn open(notify: Option<Notify>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/c", "start", crate::desktop::DASHBOARD_TITLE]).arg(exe).arg("--dashboard");
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    cmd.creation_flags(CREATE_NO_WINDOW.0);
    let child = cmd.spawn().map_err(|e| format!("cannot start a console for the dashboard: {e}"))?;
    reap(child, "cmd /c start".into(), notify);
    Ok(())
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
    let child = cmd.spawn().map_err(|e| format!("cannot start a terminal for the dashboard: {e}"))?;
    reap(child, cmd.get_program().to_string_lossy().into_owned(), notify);
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
            live.presented(Some((Duration::from_millis(10), Duration::from_millis(30))), 0, None);
        }
        assert_eq!(live.samples.lock().unwrap().len(), SAMPLES);
        assert_eq!(live.presented.load(Ordering::Relaxed), (SAMPLES + 100) as u64, "every present still counted");
    }

    #[test]
    #[cfg(windows)]
    fn the_named_pipe_carries_lines_and_its_end() {
        let dir = tmp("pipe");
        let path = socket_path(&dir);
        assert!(path.to_string_lossy().starts_with(r"\\.\pipe\protohdmicapture-dashboard-"), "{}", path.display());
        let (listener, _) = transport::listen(&path).unwrap();
        // Ours alone: a second first instance of the same name fails.
        assert!(transport::listen(&path).is_err());
        assert_eq!(listener.accept().err().map(|e| e.kind()), Some(io::ErrorKind::WouldBlock), "nobody yet");
        let mut client = transport::connect(&path).unwrap();
        transport::set_read_timeout(&mut client, Some(Duration::from_millis(200))).unwrap();
        let (mut server, ()) = listener.accept().unwrap();
        let mut r = Lines { reader: BufReader::new(client), buf: Vec::new() };
        let e = r.next().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "nothing sent yet");
        server.write_all(b"{\"v\":1}\n").unwrap();
        assert_eq!(r.next().unwrap().unwrap(), b"{\"v\":1}");
        // The next dashboard gets an instance of its own.
        let second = transport::connect(&path).unwrap();
        assert!(listener.accept().is_ok());
        drop(second);
        drop(server);
        assert_eq!(r.next().unwrap(), None, "the viewer gone is the end");
        std::fs::remove_dir_all(&dir).unwrap();
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
        wizard.presented(Some((Duration::from_millis(10), Duration::from_millis(30))), 1, Some(Duration::from_micros(16_683)));
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
    fn a_dashboard_gets_only_the_runs_it_was_given() {
        let dir = tmp("given");
        let viewer = Arc::new(Live::new());
        let wizard = Live::new();
        let server = Server::start(&socket_path(&dir), viewer.clone(), None, Duration::from_millis(20)).unwrap();
        let mut s = transport::connect(&socket_path(&dir)).unwrap();
        transport::set_read_timeout(&mut s, Some(Duration::from_secs(5))).unwrap();
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
    fn a_dashboard_gets_the_live_numbers() {
        let dir = tmp("numbers");
        let live = Arc::new(Live::new());
        let server = Server::start(&socket_path(&dir), live.clone(), None, Duration::from_millis(100)).unwrap();
        let mut s = transport::connect(&socket_path(&dir)).unwrap();
        transport::set_read_timeout(&mut s, Some(Duration::from_secs(5))).unwrap();
        let mut r = Lines { reader: BufReader::new(s), buf: Vec::new() };
        let first = read_line(&mut r);
        assert_eq!(first["v"], VERSION);
        assert_eq!(first["pid"], std::process::id());
        assert_eq!(first["signal"], "ok");
        assert!(first["dequeue_present"].is_null());
        assert_eq!(first["audio"]["state"], "off");

        let mut tracker = crate::duplicate::Tracker::new();
        for i in 0..6u64 {
            live.frame();
            let kind = if i % 2 == 0 { Kind::New } else { Kind::Duplicate };
            tracker.push(kind, Duration::from_micros(i * 16_667));
            live.source(kind, &tracker);
        }
        live.gap(10, 13, 2);
        live.signal(&Signal::NoSignal { since: Since::Stopped, reason: Reason::Undecodable("not a JPEG".into()) });
        live.dropped.undecodable.fetch_add(4, Ordering::Relaxed);
        live.presented(Some((Duration::from_millis(10), Duration::from_millis(30))), 0, Some(Duration::from_micros(16_683)));
        live.presented(Some((Duration::from_millis(12), Duration::from_millis(32))), 2, None);
        let counters = Arc::new(Counters::default());
        counters.in_min.store(480, Ordering::Relaxed);
        counters.in_max.store(480, Ordering::Relaxed);
        // As Audio::start leaves it: no output callback yet.
        counters.out_min.store(u32::MAX, Ordering::Relaxed);
        counters.xruns.store(3, Ordering::Relaxed);
        counters.realtime_refused.store(true, Ordering::Relaxed);
        counters.target_ms.store(80, Ordering::Relaxed);
        live.set_audio(AudioLive::Running { source: "card-in".into(), sink: "speakers".into(), buffer_ms: 10, counters });
        // The next snapshot after the changes.
        let s =
            (0..20).map(|_| read_line(&mut r)).find(|v| v["frames"] == 6 && !v["dequeue_present"].is_null()).expect("the numbers arrive");
        assert_eq!(s["gaps"], 1);
        assert_eq!(s["lost"], 2);
        assert_eq!(s["signal"], "stopped");
        assert_eq!(s["signal_why"], "MJPEG frames do not decode: not a JPEG");
        assert_eq!(s["dropped"], json!({ "undecodable": 4, "unusable": 0 }));
        assert_eq!(s["dequeue_present"]["n"], 2);
        assert_eq!(s["kernel_present"]["median_ms"], 32.0);
        assert_eq!(s["repeated_total"], 2);
        assert_eq!(s["source"]["new"], 3);
        assert_eq!(s["source"]["duplicate"], 3);
        assert_eq!(s["source"]["cadence"], "measuring");
        assert!(s["source"]["fps"].is_null(), "no picture: the last rates are stale");
        assert_eq!(s["repeated_window"], 2);
        assert!((s["refresh_hz"].as_f64().unwrap() - 59.94).abs() < 0.01);
        let a = &s["audio"];
        assert_eq!((a["state"].as_str(), a["source"].as_str(), a["sink"].as_str()), (Some("running"), Some("card-in"), Some("speakers")));
        assert_eq!(a["granted_in_ms"], json!([10.0, 10.0]));
        assert!(a["granted_out_ms"].is_null(), "no output callback yet");
        assert_eq!((a["xruns"].as_u64(), a["delay_ms"].as_u64()), (Some(3), Some(80)));
        assert_eq!(a["realtime_refused"].as_bool(), Some(true));
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
    fn a_dashboard_that_stops_reading_never_holds_the_viewer() {
        let dir = tmp("slow");
        let server = Server::start(&socket_path(&dir), Arc::new(Live::new()), None, Duration::from_millis(1)).unwrap();
        // Connected, never read: the server must keep its pace and drop it.
        let mut stuck = transport::connect(&socket_path(&dir)).unwrap();
        let mut healthy = transport::connect(&socket_path(&dir)).unwrap();
        transport::set_read_timeout(&mut healthy, Some(Duration::from_secs(5))).unwrap();
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
        transport::set_read_timeout(&mut stuck, Some(Duration::from_secs(5))).unwrap();
        let mut sink = Vec::new();
        stuck.read_to_end(&mut sink).expect("the server closed it");
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
        let server = Server::start(&path, Arc::new(Live::new()), None, Duration::from_millis(50)).unwrap();
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
            "source": { "new": 372, "duplicate": 372, "fps": 29.97, "signal_hz": 59.94, "cadence": "2:2", "breaks": 3 },
            "dequeue_present": { "median_ms": 9.31, "p99_ms": 14.2, "n": 599 },
            "kernel_present": { "median_ms": 40.46, "p99_ms": 49.49, "n": 599 },
            "refresh_hz": 59.951, "repeated_window": 0, "repeated_total": 3,
            "cpu_pct": 7.25, "rss_mb": 180.0,
            "audio": { "state": "running", "source": "alsa_input.usb-MACROSILICON", "sink": "alsa_output.pci",
                       "buffer_ms": 10, "granted_in_ms": [10.0, 10.0], "granted_out_ms": [10.0, 21.3],
                       "delay_ms": 80, "ring_ms": 80.2, "underruns": 0, "xruns": 1, "errors": 0, "realtime_refused": true },
            "events": ["    3.0 s  capture gap: sequence 179 -> 181, 1 lost"],
        })
    }

    #[test]
    fn frame_generation_has_a_line_only_when_it_runs() {
        use crate::framegen::Show;
        let live = Live::new();
        let text = render(&Rolling::new(&live).snapshot(&live), &Paint { on: false });
        assert!(!text.contains("frame generation"), "{text}");
        live.framegen("copy");
        // Each new frame lands in one count (#78): generated, missed (also
        // when the late window shows a held frame), or shown real; a
        // duplicate shown real or held counts nowhere.
        for (show, missed, kind) in [
            (Show::Generated, false, Kind::New),
            (Show::Held, false, Kind::Duplicate),
            (Show::Real, true, Kind::New),
            (Show::Held, true, Kind::New),
            (Show::Real, false, Kind::New),
            (Show::Real, false, Kind::Duplicate),
            (Show::Withheld, false, Kind::Duplicate),
        ] {
            live.framegen_step(show, missed, kind);
        }
        let s = Rolling::new(&live).snapshot(&live);
        assert_eq!(s["framegen"], json!({ "mode": "copy", "generated": 1, "missed": 2, "shown_real": 1, "withheld": 1 }));
        let text = render(&s, &Paint { on: false });
        assert!(text.lines().any(|l| l == "  frame generation  copy · 1 generated · 2 missed · 1 shown real · 1 withheld"), "{text}");
    }

    #[test]
    fn new_frames_only_is_held_to_the_sources_rate() {
        let paint = Paint { on: true };
        let fps_line = |s: &Value| render(s, &paint).lines().find(|l| l.contains("presented")).unwrap().to_string();
        let mut s = sample();
        s["presented_fps"] = json!(30.0);
        // The sample's source is 30 fps in a 60 Hz signal: 30 presented is
        // short of the card's 60 without new-frames-only, and all of the
        // source's with it.
        assert!(fps_line(&s).contains(&paint.good_warn("30.0", false)), "{}", fps_line(&s));
        s["framegen"] = json!({ "mode": "new-only", "generated": 0, "missed": 0, "withheld": 300 });
        assert!(fps_line(&s).contains(&paint.good_warn("30.0", true)), "{}", fps_line(&s));
    }

    #[test]
    fn the_source_line_carries_the_trackers_numbers() {
        let live = Live::new();
        let mut tracker = crate::duplicate::Tracker::new();
        for i in 0..120u64 {
            let kind = if i % 2 == 0 { Kind::New } else { Kind::Duplicate };
            tracker.push(kind, Duration::from_micros(i * 16_683));
            live.source(kind, &tracker);
        }
        let s = Rolling::new(&live).snapshot(&live);
        let src = &s["source"];
        assert_eq!((src["new"].as_u64(), src["duplicate"].as_u64()), (Some(60), Some(60)));
        assert_eq!(src["cadence"], "2:2");
        assert_eq!(src["breaks"], 0);
        assert!((src["signal_hz"].as_f64().unwrap() - 59.94).abs() < 0.01, "{src}");
        assert!((src["fps"].as_f64().unwrap() - 29.97).abs() < 0.5, "{src}");
        let text = render(&s, &Paint { on: false });
        assert!(text.contains("fps in a 59.94 Hz signal (2:2) · 0 cadence breaks"), "{text}");
    }

    #[test]
    fn warnings_are_orange_and_errors_red_whatever_the_theme() {
        // Fixed 256-colour codes: a theme repaints the 16 palette slots
        // (the user's set "red" to a green), not these (#76).
        let warn = |t: &str| format!("\x1b[38;5;208m{t}\x1b[0m");
        let error = |t: &str| format!("\x1b[38;5;196m{t}\x1b[0m");
        let text = render(&sample(), &Paint { on: true });
        assert!(text.contains(&warn("real-time refused; sound may crackle under load")), "{text}");
        assert!(text.contains(&warn("1")), "a non-zero xruns count is a warning: {text}");
        assert!(!text.contains("\x1b[31m") && !text.contains("\x1b[33m"), "no palette red or yellow: {text}");

        let mut s = sample();
        s["signal"] = json!("never");
        s["audio"] = json!({ "state": "failed", "why": "the card has no PipeWire source" });
        let text = render(&s, &Paint { on: true });
        assert!(text.contains(&error("none since the feed opened")), "{text}");
        assert!(text.contains(&error("failed: the card has no PipeWire source")), "{text}");
        assert!(!render(&s, &Paint { on: false }).contains('\x1b'), "plain without colour");
    }

    fn good(t: &str) -> String {
        format!("\x1b[32m{t}\x1b[0m")
    }
    fn warn(t: &str) -> String {
        format!("\x1b[38;5;208m{t}\x1b[0m")
    }
    fn error(t: &str) -> String {
        format!("\x1b[38;5;196m{t}\x1b[0m")
    }
    /// The coloured dashboard's line starting with `name`.
    fn line(s: &Value, name: &str) -> String {
        let text = render(s, &Paint { on: true });
        text.lines().find(|l| l.contains(name)).unwrap_or_else(|| panic!("no {name:?} line in\n{text}")).to_string()
    }

    #[test]
    fn dequeue_to_presented_is_held_to_the_go_criteria() {
        let mut s = sample();
        assert!(line(&s, "dequeue→presented").contains(&good("9.31 ms")) && line(&s, "dequeue→presented").contains(&good("14.20 ms")));
        s["dequeue_present"] = json!({ "median_ms": 18.0, "p99_ms": 40.0, "n": 599 });
        assert!(line(&s, "dequeue→presented").contains(&warn("18.00 ms")) && line(&s, "dequeue→presented").contains(&warn("40.00 ms")));
    }

    #[test]
    fn kernel_to_presented_is_held_to_the_baseline_a_refresh_more_with_frame_generation() {
        let mut s = sample();
        assert!(line(&s, "kernel→presented").contains(&good("40.46 ms")) && line(&s, "kernel→presented").contains(&good("49.49 ms")));
        s["kernel_present"] = json!({ "median_ms": 60.0, "p99_ms": 66.0, "n": 599 });
        assert!(line(&s, "kernel→presented").contains(&warn("60.00 ms")) && line(&s, "kernel→presented").contains(&warn("66.00 ms")));
        // Frame generation's refresh is allowed for: 44.9 + 16.7, 53.7 + 16.7.
        s["framegen"] = json!({ "mode": "ffx", "generated": 10, "missed": 0, "withheld": 0 });
        assert!(line(&s, "kernel→presented").contains(&good("60.00 ms")) && line(&s, "kernel→presented").contains(&good("66.00 ms")));
        // New-frames-only makes no frames: the plain baseline.
        s["framegen"] = json!({ "mode": "new-only", "generated": 0, "missed": 0, "withheld": 10 });
        assert!(line(&s, "kernel→presented").contains(&warn("60.00 ms")));
    }

    #[test]
    fn the_signal_is_green_when_ok_and_red_without_a_picture() {
        let mut s = sample();
        assert!(line(&s, "signal  ").contains(&good("ok")));
        s["signal"] = json!("stopped");
        assert!(line(&s, "signal  ").contains(&error("stopped (source asleep or off?)")));
    }

    #[test]
    fn counts_are_green_at_zero_and_orange_above() {
        let mut s = sample();
        assert!(line(&s, "capture gaps").contains(&good("0")));
        assert!(line(&s, "repeated").contains(&good("0")));
        assert!(line(&s, "dropped").contains(&good("0")));
        assert!(line(&s, "fps").contains(&good("59.9")));
        assert!(line(&s, "underruns").contains(&good("0")) && line(&s, "underruns").contains(&warn("1")));
        assert!(line(&s, "source").contains(&warn("3")));
        s["gaps"] = json!(2);
        s["repeated_window"] = json!(4);
        s["dropped"] = json!({ "undecodable": 1, "unusable": 0 });
        s["presented_fps"] = json!(50.0);
        s["source"]["breaks"] = json!(0);
        assert!(line(&s, "capture gaps").contains(&warn("2")));
        assert!(line(&s, "repeated").contains(&warn("4")));
        assert!(line(&s, "dropped").contains(&warn("1")));
        assert!(line(&s, "fps").contains(&warn("50.0")));
        assert!(line(&s, "source").contains(&good("0")));
        s["framegen"] = json!({ "mode": "ffx", "generated": 10, "missed": 0, "withheld": 0 });
        assert!(line(&s, "frame generation").contains(&good("0")));
        s["framegen"]["missed"] = json!(2);
        assert!(line(&s, "frame generation").contains(&warn("2")));
    }

    #[test]
    fn the_granted_audio_buffer_is_held_to_what_was_asked() {
        let mut s = sample();
        // Asked 10 ms: in 10.0 is good, out up to 21.3 is over.
        let l = line(&s, "buffer");
        assert!(l.contains(&good("10.0 ms")) && l.contains(&warn("10.0..21.3 ms")), "{l}");
        s["audio"]["granted_out_ms"] = json!([5.3, 5.3]);
        assert!(line(&s, "buffer").contains(&good("5.3 ms")));
    }

    #[test]
    fn gpu_readings_are_held_to_the_cards_own_limits() {
        let mut s = sample();
        let reading = |m: f64| json!({ "median": m, "max": m });
        s["gpu"] = json!({
            "source": "amdgpu sysfs", "samples": 10,
            "power_w": reading(200.0), "edge_c": reading(60.0), "junction_c": reading(111.0), "memory_c": reading(114.0),
            "limits": {
                "edge_c": { "crit": 110.0, "emergency": 115.0 },
                "junction_c": { "crit": 110.0, "emergency": 115.0 },
                "memory_c": { "crit": 108.0, "emergency": 113.0 },
                "power_cap_w": 330.0,
            },
        });
        let t = line(&s, "temperature");
        assert!(t.contains(&good("60 °C")) && t.contains(&warn("111 °C")) && t.contains(&error("114 °C")), "{t}");
        assert!(line(&s, "power").contains(&good("200 W")));
        s["gpu"]["power_w"] = reading(330.0);
        assert!(line(&s, "power").contains(&warn("330 W")));
        // No limits from the driver: plain.
        s["gpu"]["limits"] = json!({});
        assert!(line(&s, "temperature").contains("edge 60 °C · junction 111 °C"));
        let l = line(&s, "power");
        assert!(!l.contains("\x1b[32m") && !l.contains("38;5;"), "no cap, no colour: {l}");
    }

    #[test]
    fn vram_is_orange_from_90_percent_and_red_from_97() {
        let mut s = sample();
        s["gpu"] = json!({ "source": "amdgpu sysfs", "samples": 10, "vram_used_mib": 2048.0, "vram_total_mib": 16384.0 });
        assert!(line(&s, "VRAM").contains(&good("2.0 of 16.0 GB")));
        s["gpu"]["vram_used_mib"] = json!(14745.6); // 90 %
        assert!(line(&s, "VRAM").contains(&warn("14.4 of 16.0 GB")));
        s["gpu"]["vram_used_mib"] = json!(15892.48); // 97 %
        assert!(line(&s, "VRAM").contains(&error("15.5 of 16.0 GB")));
    }

    #[test]
    fn the_dashboards_text() {
        let text = render(&sample(), &Paint { on: false });
        let want = [
            "  protoHDMIcapture · live   running · pid 4242 · up 12 s",
            "  fps               60.0 captured · 59.9 presented · display 59.95 Hz",
            "  capture gaps      0 · 0 frames lost · 744 frames captured",
            "  source            30.0 fps in a 59.94 Hz signal (2:2) · 3 cadence breaks",
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
            "  priority          real-time refused; sound may crackle under load",
            "        3.0 s  capture gap: sequence 179 -> 181, 1 lost",
            "  Ctrl-C closes this window; the feed keeps running.",
        ];
        for w in want {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }

        let mut s = sample();
        s["signal"] = json!("never");
        s["source"]["fps"] = Value::Null;
        s["dequeue_present"] = Value::Null;
        s["audio"] = json!({ "state": "failed", "why": "the card has no PipeWire source" });
        s["events"] = json!([]);
        let text = render(&s, &Paint { on: false });
        for w in [
            "  signal            none since the feed opened",
            "  source            no picture yet",
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
        for w in [
            "  signal            no picture: MJPEG frames do not decode: not a JPEG",
            "  dropped           120 · 120 undecodable · 0 unusable",
        ] {
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
    fn a_dashboard_gets_the_display_section() {
        let dir = tmp("display");
        let display = Display::new(Desktop::Generic, dir.join("refresh.restore"));
        let server = Server::start(&socket_path(&dir), Arc::new(Live::new()), Some(display), Duration::from_millis(20)).unwrap();
        let mut s = transport::connect(&socket_path(&dir)).unwrap();
        transport::set_read_timeout(&mut s, Some(Duration::from_secs(5))).unwrap();
        let mut r = Lines { reader: BufReader::new(s), buf: Vec::new() };
        // The first snapshot may have gone out before this dashboard was
        // accepted; the next ones carry the section.
        let d = (0..50).map(|_| read_line(&mut r)).find(|v| v["display"].is_object()).expect("a Display section")["display"].clone();
        let want = if cfg!(windows) {
            not_on_windows()
        } else {
            json!({ "monitors": null, "why": "the monitors are known on Hyprland only", "refresh": null })
        };
        assert_eq!(d, want);
        drop(server);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Windows has neither the monitors nor REFRESH=match (#38): the section
    /// says so, whatever a record in the run dir holds.
    #[cfg(windows)]
    #[test]
    fn the_display_section_on_windows_says_it_is_not_there() {
        let dir = tmp("display-windows");
        let record = dir.join("refresh.restore");
        std::fs::write(&record, "DP-2|60|59.95|hl.monitor({})\n").unwrap();
        let mut d = Display::new(Desktop::Generic, record);
        assert_eq!(d.json(Instant::now()), not_on_windows());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The refresh record is read on Linux only: Windows has no REFRESH=match.
    #[cfg(not(windows))]
    #[test]
    fn the_display_section_is_read_again_on_a_switch_or_after_a_while() {
        let dir = tmp("display-cache");
        let record = dir.join("refresh.restore");
        let mut d = Display::new(Desktop::Generic, record.clone());
        let t0 = Instant::now();
        let read_at = |d: &Display| d.last.as_ref().map(|(at, _, _)| *at);
        assert!(d.json(t0)["refresh"].is_null());
        d.json(t0 + Duration::from_secs(1));
        assert_eq!(read_at(&d), Some(t0), "not read again within DISPLAY_EVERY");
        // A switch: read at once.
        std::fs::write(&record, "DP-2|60|59.95|hl.monitor({})\n").unwrap();
        let v = d.json(t0 + Duration::from_secs(2));
        assert_eq!(read_at(&d), Some(t0 + Duration::from_secs(2)));
        assert_eq!(v["refresh"], json!({ "monitor": "DP-2", "set_hz": "60", "was_hz": "59.95" }));
        d.json(t0 + Duration::from_secs(3));
        assert_eq!(read_at(&d), Some(t0 + Duration::from_secs(2)));
        d.json(t0 + Duration::from_secs(7));
        assert_eq!(read_at(&d), Some(t0 + Duration::from_secs(7)), "read again after DISPLAY_EVERY");
        std::fs::write(&record, "nonsense\n").unwrap();
        assert_eq!(d.json(t0 + Duration::from_secs(8))["refresh"], json!({ "unreadable": "unreadable refresh record: nonsense" }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_display_sections_text() {
        let monitors = vec![
            crate::desktop::Monitor { name: "DP-1".into(), width: 3440, height: 1440, refresh_hz: 100.0, focused: false, viewer: false },
            crate::desktop::Monitor { name: "DP-2".into(), width: 1920, height: 1200, refresh_hz: 59.950001, focused: true, viewer: true },
            crate::desktop::Monitor {
                name: "HDMI-A-1".into(),
                width: 2560,
                height: 720,
                refresh_hz: 60.266,
                focused: false,
                viewer: false,
            },
        ];
        let mut s = sample();
        s["display"] = display_json(Ok(monitors), Ok(Some("DP-2|60|59.95|hl.monitor({})\n".into())));
        let text = render(&s, &Paint { on: false });
        for w in [
            "  monitor           DP-1 3440x1440@100Hz",
            "  monitor           DP-2 1920x1200@59.95Hz focused · the viewer's",
            "  monitor           HDMI-A-1 2560x720@60.27Hz",
            "  refresh           DP-2 at 60 Hz by REFRESH=match (back to 59.95 Hz on exit)",
        ] {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }
        // After Audio, before the events, as view.sh's.
        let at = |h: &str| text.find(h).unwrap_or_else(|| panic!("no {h} in {text}"));
        assert!(at("  Audio") < at("  Display") && at("  Display") < at("  Recent events"));

        s["display"] = display_json(Err("the monitors are known on Hyprland only".into()), Ok(None));
        let text = render(&s, &Paint { on: false });
        for w in ["  monitors          not known: the monitors are known on Hyprland only", "  refresh           not switched"] {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }
        s["display"] = not_on_windows();
        let text = render(&s, &Paint { on: false });
        for w in [
            "  monitors          not known: they are known on Hyprland only, not on Windows",
            "  refresh           REFRESH=match is not on Windows",
        ] {
            assert!(text.lines().any(|l| l == w), "no line {w:?} in\n{text}");
        }
        // No section from a viewer that sends none.
        assert!(!render(&sample(), &Paint { on: false }).contains("Display"));
    }

    #[test]
    fn signal_as_view_shs_dashboard_names_it() {
        let no = |since, reason| Signal::NoSignal { since, reason };
        assert_eq!(signal_json(&Signal::Picture), ("ok", Value::Null));
        assert_eq!(signal_json(&no(Since::Never, Reason::Nothing)), ("never", Value::Null));
        assert_eq!(signal_json(&no(Since::Stopped, Reason::Nothing)), ("stopped", Value::Null));
        assert_eq!(
            signal_json(&no(Since::Never, Reason::Unusable("short".into()))),
            ("never", json!("frames the viewer cannot use: short"))
        );
    }

    #[test]
    fn the_gpu_section_shows_only_while_a_gpu_enhancement_is_on() {
        // A plain run: no "gpu" in the snapshot, no GPU section (#72).
        let live = Live::new();
        let s = Rolling::new(&live).snapshot(&live);
        assert!(s["gpu"].is_null());
        assert!(!render(&s, &Paint { on: false }).lines().any(|l| l == "  GPU"));
        // On: the section, nvtop-style, from what the sampler read.
        let shared = Arc::new(crate::gpustat::Shared::new(Ok("amdgpu sysfs".into()), false));
        let card = crate::gpustat::Card {
            busy_pct: Some(8.0),
            shader_mhz: Some(1329.0),
            memory_mhz: Some(1258.0),
            vram_used: Some(2 << 30),
            vram_total: Some(16 << 30),
            power_w: Some(49.0),
            edge_c: Some(49.0),
            junction_c: Some(51.0),
            memory_c: Some(60.0),
            fan_rpm: Some(0.0),
            limits: crate::gpustat::Limits::default(),
        };
        shared.push(Instant::now(), crate::gpustat::Sample { card, ours_pct: Some(3.0), ours_vram: Some(450 << 20) });
        live.set_gpu(Some(shared));
        let text = render(&Rolling::new(&live).snapshot(&live), &Paint { on: false });
        let gpu: Vec<&str> = text.lines().skip_while(|l| *l != "  GPU").take_while(|l| !l.is_empty()).collect();
        assert_eq!(
            gpu,
            [
                "  GPU",
                "  read from         amdgpu sysfs · last 10 s",
                "  busy              8 % (max 8 %)",
                "  protoHDMIcapture  3 % (max 3 %) of the engines · 450 MB VRAM",
                "  clocks            shader 1329 MHz · memory 1258 MHz",
                "  VRAM              2.0 of 16.0 GB",
                "  power             49 W (max 49 W)",
                "  temperature       edge 49 °C · junction 51 °C · memory 60 °C",
                "  fan               0 RPM",
            ],
            "{text}"
        );
        // GameMode (#77): its row under the clocks, for each status; none
        // while it is off.
        use crate::gamemode::Status;
        let status = Arc::new(Mutex::new(Status::Off));
        live.set_gamemode(status.clone());
        let row = |live: &Live| {
            render(&Rolling::new(live).snapshot(live), &Paint { on: false })
                .lines()
                .find(|l| l.starts_with("  GameMode"))
                .map(str::to_string)
        };
        assert_eq!(row(&live), None);
        for (s, shown) in [
            (Status::Asking, "asking"),
            (Status::Active, "active for protoHDMIcapture"),
            (Status::Registered(1), "registered, not active (status 1)"),
            (Status::Unavailable("no daemon".into()), "not available: no daemon"),
            (Status::Failed("RegisterGame answered -1".into()), "Warning: RegisterGame answered -1"),
        ] {
            *status.lock().unwrap() = s;
            assert_eq!(row(&live), Some(format!("  GameMode          {shown}")));
        }
        let text = render(&Rolling::new(&live).snapshot(&live), &Paint { on: false });
        let after_clocks = text.lines().skip_while(|l| !l.starts_with("  clocks")).nth(1);
        assert_eq!(after_clocks.map(|l| l.starts_with("  GameMode")), Some(true), "{text}");
        live.set_gpu(None);
        assert!(Rolling::new(&live).snapshot(&live)["gpu"].is_null());
    }

    #[test]
    fn replaced_presents_say_the_latency_is_not_measurable() {
        let live = Live::new();
        live.set_replaced(true);
        let text = render(&Rolling::new(&live).snapshot(&live), &Paint { on: false });
        assert!(text.contains("kernel→presented  not measurable: every present is replaced before display"), "{text}");
        assert!(live.events.lock().unwrap().iter().any(|e| e.contains("latency: not measurable")));
        live.set_replaced(false);
        let text = render(&Rolling::new(&live).snapshot(&live), &Paint { on: false });
        assert!(text.contains("no frame presented yet"), "{text}");
    }
}
