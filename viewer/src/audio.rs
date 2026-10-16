//! Audio: the card audio input -> a delay ring -> the sink, through cpal's
//! PipeWire host, as view.sh's `pw-loopback` does: SRC (else the card's own
//! source, `pw::Snapshot::card_source`), SINK (else the default), LAT_MS
//! buffers, DELAY. cpal is the seam for now; a native PipeWire path can
//! replace it without touching the rest (decided in #10). On Windows (#18)
//! the host is WASAPI and SRC/SINK name endpoints; the output's level keys
//! are not there yet.
//!
//! The delay is the ring's fill over one buffer (`target_frames`).
//! Changing it live never inserts silence or drops a block: the output
//! plays faster or slower (linear interpolation), in proportion to how far
//! the fill is from the target and at most `CATCH_UP`, until it gets there.
//!
//! A loopback whose stream goes away mid-feed (the card's audio unplugged,
//! the node removed) is dead: said once, and the next sync or volume key
//! starts a new one (`Loopback`, #29), as view.sh's sync keys do.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, HostId, StreamConfig};

use crate::overlay::Notify;
use crate::level::{Level, LevelChange};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// Fastest the output speeds up or slows down to reach a new delay.
const CATCH_UP: f64 = 0.03;
/// Speed change per second of delay error: 10 ms off plays 0.5 % off.
const GAIN: f64 = 0.5;
/// Closer than this to the target, play at exactly 1x.
const DEAD_BAND: f64 = 0.001;
/// The largest delay: view.sh's DELAY setting is 0..1 s.
pub const MAX_DELAY_MS: u32 = 1000;
/// The ring holds the largest delay plus room for the input running up to
/// two buffers ahead (LAT_MS goes to 1000 ms) and a margin; a delay it
/// cannot hold would never prime (silence).
const fn ring_frames(buffer_frames: u32) -> usize {
    frames_for(MAX_DELAY_MS) + 2 * buffer_frames as usize + RATE as usize / 4
}

/// The ring's fill the output steers to: DELAY over one buffer (LAT_MS).
/// The input arrives a block at a time, so a fill of DELAY alone runs dry
/// between blocks at small delays (DELAY=0: about a third of the output
/// callbacks found the ring empty); pw-loopback likewise keeps its
/// graph's latency under `--delay`.
const fn target_frames(delay_ms: u32, buffer_frames: u32) -> usize {
    frames_for(delay_ms) + buffer_frames as usize
}

const fn frames_for(ms: u32) -> usize {
    (ms as u64 * RATE as u64 / 1000) as usize
}

/// A delay the ring can hold, or why not.
pub fn check_delay(ms: u32) -> Result<u32, String> {
    if ms > MAX_DELAY_MS {
        return Err(format!("delay {ms} ms is over the {MAX_DELAY_MS} ms maximum"));
    }
    Ok(ms)
}

type Frame = [f32; 2];

/// Shared between the two PipeWire threads and whoever reads the numbers.
#[derive(Default)]
pub struct Counters {
    /// The output wanted a frame the ring did not have (after priming).
    pub ring_underruns: AtomicU64,
    /// PipeWire reported an xrun on either stream.
    pub xruns: AtomicU64,
    pub other_errors: AtomicU64,
    /// Frames per callback, smallest and largest seen.
    pub in_min: AtomicU32,
    pub in_max: AtomicU32,
    pub out_min: AtomicU32,
    pub out_max: AtomicU32,
    /// The ring's fill, in frames, at the last output callback.
    pub fill: AtomicU32,
    pub target_ms: AtomicU32,
    /// Set while the streams are torn down, which PipeWire reports as a
    /// disconnect.
    closing: AtomicBool,
    /// Why the loopback stopped mid-feed, once a stream reported itself
    /// gone (#29).
    dead: Mutex<Option<String>>,
}

impl Counters {
    /// Why the loopback stopped mid-feed, if it did.
    pub fn dead(&self) -> Option<String> {
        self.dead.lock().expect("the death's lock is never poisoned").clone()
    }
}

/// Where a message for the window goes, once it runs (`Controls::set_notify`).
type NotifySlot = Arc<Mutex<Option<Notify>>>;

struct Ring {
    frames: VecDeque<Frame>,
    /// Fractional read position into `frames`.
    pos: f64,
    /// Output starts once the ring first holds the delay.
    primed: bool,
}

pub struct Audio {
    _input: cpal::Stream,
    _output: cpal::Stream,
    input: String,
    output: String,
    buffer_frames: u32,
    pub counters: Arc<Counters>,
    notify: NotifySlot,
}

fn seen(min: &AtomicU32, max: &AtomicU32, n: u32) {
    min.fetch_min(n, Ordering::Relaxed);
    max.fetch_max(n, Ordering::Relaxed);
}

fn on_error(counters: &Arc<Counters>, notify: &NotifySlot, side: &'static str) -> impl FnMut(cpal::Error) + Send + 'static {
    let (counters, notify) = (counters.clone(), notify.clone());
    move |e| {
        if counters.closing.load(Ordering::Relaxed) {
            return;
        }
        match e.kind() {
            ErrorKind::Xrun => {
                counters.xruns.fetch_add(1, Ordering::Relaxed);
            }
            // The stream is gone: cpal reports PipeWire's stream state
            // going Unconnected or Error this way (host/pipewire/stream.rs
            // `state_changed`), and WASAPI's device removal and invalidated
            // resources.
            ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => died(&counters, &notify, format!("{side}: {e}")),
            _ => {
                counters.other_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("audio {side}: {e}");
            }
        }
    }
}

/// The loopback stopped mid-feed (#29): said once (the other stream goes
/// too), in the terminal and in the window, as view.sh's `audio_failed`.
fn died(counters: &Counters, notify: &NotifySlot, why: String) {
    {
        let mut dead = counters.dead.lock().expect("the death's lock is never poisoned");
        if dead.is_some() {
            return;
        }
        *dead = Some(why.clone());
    }
    eprintln!("warning: the audio loopback stopped mid-feed: {why}");
    eprintln!("warning: the feed plays on without sound; ctrl -/+, m, 9 or 0 start the loopback again");
    if let Some(notify) = notify.lock().expect("the notify lock is never poisoned").as_ref() {
        notify(Some((format!("Audio stopped ({why}): ctrl -/+, m, 9 or 0 start it again"), Duration::from_secs(5))));
    }
}

/// How long the session manager gets to link the input.
#[cfg(target_os = "linux")]
const LINK_WAIT: Duration = Duration::from_secs(2);

/// How often `verify_linked` looks: WirePlumber links within a few ms, a
/// look costs about 2 ms, and a restart from a sync key waits on it in the
/// window (every 100 ms froze the picture for about 110 ms, 2026-10-16).
#[cfg(target_os = "linux")]
const LINK_POLL: Duration = Duration::from_millis(5);

/// Fails unless the source linked into our capture stream is `want`, and
/// only it, within LINK_WAIT.
#[cfg(target_os = "linux")]
fn verify_linked(want: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + LINK_WAIT;
    loop {
        let linked = crate::pw::snapshot()?.sources_linked_to_us();
        if !linked.is_empty() {
            if linked.iter().all(|n| n == want) {
                return Ok(());
            }
            return Err(format!("the audio input linked to {linked:?}, not {want}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("the audio input ({want}) was not linked within {} s", LINK_WAIT.as_secs()));
        }
        std::thread::sleep(LINK_POLL);
    }
}

/// The audio host: PipeWire, as view.sh's pw-loopback; WASAPI on Windows.
#[cfg(target_os = "linux")]
const HOST: HostId = HostId::PipeWire;
#[cfg(windows)]
const HOST: HostId = HostId::Wasapi;

/// What SRC or SINK may say to name `d`: on PipeWire its node name (the
/// device id displays as "<host>:<node name>").
#[cfg(target_os = "linux")]
fn names(d: &cpal::Device) -> Result<Vec<String>, String> {
    let id = d.id().map_err(|e| e.to_string())?.to_string();
    Ok(id.split_once(':').map(|(_, n)| n.to_string()).into_iter().collect())
}

/// On WASAPI: the endpoint's friendly name, as Windows' sound settings
/// show it, or its endpoint id.
#[cfg(windows)]
fn names(d: &cpal::Device) -> Result<Vec<String>, String> {
    let id = d.id().map_err(|e| e.to_string())?;
    let name = d.description().map_err(|e| e.to_string())?.name().to_string();
    Ok(vec![name, id.id().to_string()])
}

/// The audio inputs' names, for a message that lists what SRC may say.
#[cfg(windows)]
pub fn input_names() -> Result<Vec<String>, String> {
    let host = cpal::host_from_id(HOST).map_err(|e| format!("{HOST} host: {e}"))?;
    let mut out = Vec::new();
    for d in host.input_devices().map_err(|e| format!("audio inputs: {e}"))? {
        out.extend(names(&d)?.into_iter().take(1));
    }
    Ok(out)
}

/// The output stream's level: read (None) or changed, on PipeWire.
#[cfg(target_os = "linux")]
fn output_level(change: Option<LevelChange>) -> Result<Level, String> {
    crate::pw::level(change)
}

/// No level keys on Windows yet (#18).
#[cfg(windows)]
fn output_level(_: Option<LevelChange>) -> Result<Level, String> {
    Err("volume and mute are not on Windows yet".into())
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.counters.closing.store(true, Ordering::Relaxed);
    }
}

impl Audio {
    /// `source` and `sink` are PipeWire node names (no sink: the default);
    /// on Windows, endpoint names or ids (`input_names`). `buffer_ms` is
    /// LAT_MS.
    pub fn start(source: &str, sink: Option<&str>, delay_ms: u32, buffer_ms: u32) -> Result<Self, String> {
        let delay_ms = check_delay(delay_ms)?;
        let host = cpal::host_from_id(HOST).map_err(|e| format!("{HOST} host: {e}"))?;
        let by_name = |want: &str| -> Result<Option<cpal::Device>, String> {
            for d in host.devices().map_err(|e| format!("audio devices: {e}"))? {
                if names(&d)?.iter().any(|n| n == want) {
                    return Ok(Some(d));
                }
            }
            Ok(None)
        };
        let in_dev = by_name(source)?.ok_or_else(|| format!("no {HOST} source {source} (SRC)"))?;
        let in_name = source.to_string();
        let (out_dev, out_name) = match sink {
            Some(sink) => (by_name(sink)?.ok_or_else(|| format!("no {HOST} sink {sink} (SINK)"))?, sink.to_string()),
            None => (host.default_output_device().ok_or("no default output device")?, "the default output".to_string()),
        };

        let buffer_frames = (buffer_ms * RATE / 1000).max(1);
        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: RATE,
            buffer_size: BufferSize::Fixed(buffer_frames),
        };
        let counters = Arc::new(Counters {
            in_min: AtomicU32::new(u32::MAX),
            out_min: AtomicU32::new(u32::MAX),
            target_ms: AtomicU32::new(delay_ms),
            ..Default::default()
        });
        let ring = Arc::new(Mutex::new(Ring { frames: VecDeque::with_capacity(ring_frames(buffer_frames)), pos: 0.0, primed: false }));
        let notify = NotifySlot::default();

        let input = {
            let ring = ring.clone();
            let c = counters.clone();
            in_dev
                .build_input_stream::<f32, _, _>(
                    config,
                    move |data, _| {
                        let n = data.len() / CHANNELS as usize;
                        seen(&c.in_min, &c.in_max, n as u32);
                        let mut r = ring.lock().unwrap();
                        r.frames.extend(data.as_chunks::<2>().0.iter().copied());
                        // A stalled output must not grow the delay
                        // without bound.
                        let excess = r.frames.len().saturating_sub(ring_frames(buffer_frames));
                        r.frames.drain(..excess);
                    },
                    on_error(&counters, &notify, "input"),
                    None,
                )
                .map_err(|e| format!("input stream on {in_name}: {e}"))?
        };
        let output = {
            let ring = ring.clone();
            let c = counters.clone();
            out_dev
                .build_output_stream::<f32, _, _>(
                    config,
                    move |out, _| {
                        let n = out.len() / CHANNELS as usize;
                        seen(&c.out_min, &c.out_max, n as u32);
                        let target = target_frames(c.target_ms.load(Ordering::Relaxed), buffer_frames);
                        let mut r = ring.lock().unwrap();
                        c.fill.store(r.frames.len() as u32, Ordering::Relaxed);
                        if !r.primed {
                            if r.frames.len() < target {
                                out.fill(0.0);
                                return;
                            }
                            r.primed = true;
                        }
                        // Too much delay: play faster; too little: slower.
                        let error = (r.frames.len() as f64 - target as f64) / RATE as f64;
                        let step = if error.abs() <= DEAD_BAND {
                            1.0
                        } else {
                            1.0 + (error * GAIN).clamp(-CATCH_UP, CATCH_UP)
                        };
                        let mut pos = r.pos;
                        let mut dry_from = None;
                        for (i, o) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                            let k = pos as usize;
                            let (Some(a), Some(b)) = (r.frames.get(k), r.frames.get(k + 1)) else {
                                c.ring_underruns.fetch_add(1, Ordering::Relaxed);
                                dry_from = Some(i);
                                break;
                            };
                            let t = (pos - k as f64) as f32;
                            o[0] = a[0] + (b[0] - a[0]) * t;
                            o[1] = a[1] + (b[1] - a[1]) * t;
                            pos += step;
                        }
                        if let Some(i) = dry_from {
                            out[i * 2..].fill(0.0);
                        }
                        let used = (pos as usize).min(r.frames.len());
                        r.frames.drain(..used);
                        r.pos = pos - used as f64;
                    },
                    on_error(&counters, &notify, "output"),
                    None,
                )
                .map_err(|e| format!("output stream on {out_name}: {e}"))?
        };
        input.play().map_err(|e| format!("input play: {e}"))?;
        // The output starts only once the input is proven linked to the
        // card, so a fallback source never reaches the speakers. WASAPI
        // opens the endpoint named, with no session manager to relink it.
        #[cfg(target_os = "linux")]
        verify_linked(source)?;
        output.play().map_err(|e| format!("output play: {e}"))?;
        Ok(Self { _input: input, _output: output, input: in_name, output: out_name, buffer_frames, counters, notify })
    }

    pub fn describe(&self) -> String {
        format!(
            "audio: {} -> {}, asked {} frames ({} ms) at {} Hz, delay {} ms",
            self.input,
            self.output,
            self.buffer_frames,
            self.buffer_frames * 1000 / RATE,
            RATE,
            self.counters.target_ms.load(Ordering::Relaxed)
        )
    }

    /// Buffer sizes PipeWire actually ran the streams at.
    pub fn granted(&self) -> String {
        let c = &self.counters;
        let ms = |f: u32| f as f64 * 1000.0 / RATE as f64;
        let (imin, imax) = (c.in_min.load(Ordering::Relaxed), c.in_max.load(Ordering::Relaxed));
        let (omin, omax) = (c.out_min.load(Ordering::Relaxed), c.out_max.load(Ordering::Relaxed));
        if imin == u32::MAX || omin == u32::MAX {
            return "granted: no callbacks yet".into();
        }
        format!(
            "granted: input {imin}..{imax} frames ({:.1}..{:.1} ms), output {omin}..{omax} frames ({:.1}..{:.1} ms)",
            ms(imin),
            ms(imax),
            ms(omin),
            ms(omax)
        )
    }

    pub fn delay_ms(&self) -> u32 {
        self.counters.target_ms.load(Ordering::Relaxed)
    }

    /// The source and sink linked, and the buffer asked for in ms (the
    /// dashboard, #16).
    pub fn link(&self) -> (&str, &str, u32) {
        (&self.input, &self.output, self.buffer_frames * 1000 / RATE)
    }

    /// The live keys' handle: sync and the output stream's level.
    pub fn controls(&self) -> Controls {
        Controls::new(self.counters.clone(), self.notify.clone(), Arc::new(output_level))
    }

    /// The ring's fill as a delay.
    pub fn fill(&self) -> Duration {
        Duration::from_secs_f64(self.counters.fill.load(Ordering::Relaxed) as f64 / RATE as f64)
    }

    pub fn status(&self) -> String {
        let c = &self.counters;
        format!(
            "delay {} ms (ring {:.1} ms), ring underruns {}, xruns {}, errors {}",
            self.delay_ms(),
            self.fill().as_secs_f64() * 1000.0,
            c.ring_underruns.load(Ordering::Relaxed),
            c.xruns.load(Ordering::Relaxed),
            c.other_errors.load(Ordering::Relaxed)
        )
    }
}

/// The last level read or set, or why it could not be.
pub type LevelSlot = Arc<Mutex<Option<Result<Level, String>>>>;

/// Reads (None) or changes the output stream's level: `output_level`, or
/// a stand-in in tests.
type LevelFn = Arc<dyn Fn(Option<LevelChange>) -> Result<Level, String> + Send + Sync>;

/// The audio keys' side of the audio: the delay target, read by the
/// output callback (a change plays faster or slower until the ring gets
/// there, never a restart), and the output stream's level.
pub struct Controls {
    counters: Arc<Counters>,
    tx: mpsc::Sender<Option<LevelChange>>,
    /// The last level read or set, or why it could not be.
    pub level: LevelSlot,
    /// Told when the level changes, or the loopback dies (the window, once
    /// it runs).
    notify: NotifySlot,
}

impl Controls {
    /// The keys' handle on a loopback's numbers; the level is read and set
    /// through `level`, on a thread of its own.
    fn new(counters: Arc<Counters>, notify: NotifySlot, level_fn: LevelFn) -> Self {
        let (tx, rx) = mpsc::channel::<Option<LevelChange>>();
        let level = LevelSlot::default();
        let (l, n) = (level.clone(), notify.clone());
        // PipeWire round trips off the window's thread, one change at a
        // time (lib/audio.sh serialises them with a lock); the thread ends
        // with the last sender.
        thread::spawn(move || {
            for change in rx {
                // The first read, and a restarted loopback's level, can come
                // before PipeWire lists the new stream: a second's worth of
                // retries. A key's change is not repeated.
                let retries = matches!(change, None | Some(LevelChange::Set(_)));
                let mut r = level_fn(change);
                for _ in 0..10 {
                    if !retries || r.is_ok() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                    r = level_fn(change);
                }
                // The messages the mpv script shows for m, 9 and 0.
                let message = match &r {
                    Ok(v) if matches!(change, Some(LevelChange::Mute | LevelChange::Volume(_))) => Some((
                        if v.mute { "Audio muted".to_string() } else { format!("Volume {}%", v.volume) },
                        Duration::from_millis(1500),
                    )),
                    Ok(_) => None,
                    Err(e) => {
                        eprintln!("audio level: {e}");
                        Some((format!("Audio: {e}"), Duration::from_secs(3)))
                    }
                };
                *l.lock().expect("the level's lock is never poisoned") = Some(r);
                if let Some(notify) = n.lock().expect("the notify lock is never poisoned").as_ref() {
                    notify(message);
                }
            }
        });
        // What WirePlumber restored for the stream, for the panel. The
        // thread holds the receiver until this handle goes, so the send
        // cannot fail.
        tx.send(None).expect("the level thread runs while its sender lives");
        Controls { counters, tx, level, notify }
    }

    /// Why the loopback stopped mid-feed, if it did (#29).
    pub fn dead(&self) -> Option<String> {
        self.counters.dead()
    }

    pub fn delay_ms(&self) -> u32 {
        self.counters.target_ms.load(Ordering::Relaxed)
    }

    /// The sync keys: the delay moved by `by_ms`, clamped to 0..MAX_DELAY_MS
    /// (DELAY's range). Returns the new delay.
    pub fn nudge_delay(&self, by_ms: i32) -> u32 {
        let ms = nudged(self.delay_ms(), by_ms);
        self.counters.target_ms.store(ms, Ordering::Relaxed);
        ms
    }

    pub fn set_notify(&self, notify: Notify) {
        *self.notify.lock().expect("the notify lock is never poisoned") = Some(notify);
    }

    /// The level as last read, if it was.
    pub fn current_level(&self) -> Option<Result<Level, String>> {
        self.level.lock().expect("the level's lock is never poisoned").clone()
    }

    /// m, 9 and 0, and a restarted loopback's level: done on the
    /// controls' thread, in the order asked.
    pub fn change_level(&self, change: LevelChange) {
        // The thread holds the receiver while this handle lives.
        self.tx.send(Some(change)).expect("the level thread runs while its sender lives");
    }
}

/// One running loopback, as `Loopback` holds it (#29): `Audio` on the
/// platform's host, a stand-in in tests.
pub trait Running {
    fn controls(&self) -> Controls;
    fn counters(&self) -> &Arc<Counters>;
    /// The source and sink linked, and the buffer asked for in ms.
    fn link(&self) -> (&str, &str, u32);
    fn describe(&self) -> String;
    fn granted(&self) -> String;
    fn status(&self) -> String;
}

impl Running for Audio {
    fn controls(&self) -> Controls {
        Audio::controls(self)
    }

    fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    fn link(&self) -> (&str, &str, u32) {
        Audio::link(self)
    }

    fn describe(&self) -> String {
        Audio::describe(self)
    }

    fn granted(&self) -> String {
        Audio::granted(self)
    }

    fn status(&self) -> String {
        Audio::status(self)
    }
}

/// Starts a loopback at a delay in ms, from the settings (SRC or the card's
/// own source, SINK, LAT_MS): what a restart repeats.
pub type Start = Box<dyn Fn(u32) -> Result<Box<dyn Running>, String>>;

struct Shared {
    start: Start,
    running: RefCell<Option<Box<dyn Running>>>,
    /// The delay last asked for: the next start's, while none runs.
    delay_ms: Cell<u32>,
    /// The running loopback's level, as its controls last read or set it.
    level: RefCell<Option<LevelSlot>>,
    /// The last level known, given to the next loopback (view.sh's
    /// `audio.state`).
    last_level: Cell<Option<Level>>,
    /// The feed ended it: no more starts.
    stopped: Cell<bool>,
}

/// The session's audio loopback, AUDIO=loopback (#29): the one running, if
/// any, and how to start another. The feed owner holds it and stops it in
/// its order (feed.rs) by dropping it; the window holds a `LoopbackKeys`,
/// whose keys start a new one when it is not running.
pub struct Loopback(Rc<Shared>);

/// The window's handle on the session's loopback.
#[derive(Clone)]
pub struct LoopbackKeys(Rc<Shared>);

impl Loopback {
    /// Nothing runs yet: `LoopbackKeys::start` starts it.
    pub fn new(start: Start, delay_ms: u32) -> Self {
        Self(Rc::new(Shared {
            start,
            running: RefCell::default(),
            delay_ms: Cell::new(delay_ms),
            level: RefCell::default(),
            last_level: Cell::default(),
            stopped: Cell::new(false),
        }))
    }

    pub fn keys(&self) -> LoopbackKeys {
        LoopbackKeys(self.0.clone())
    }

    /// The running loopback's buffer sizes and status (`--stats`), or that
    /// none runs.
    pub fn summary(&self) -> [String; 2] {
        match self.0.running.borrow().as_ref() {
            Some(r) => [r.granted(), r.status()],
            None => ["not running".into(), "not running".into()],
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.0.stopped.set(true);
        drop(self.0.running.borrow_mut().take());
    }
}

impl LoopbackKeys {
    /// Starts the loopback at `delay_ms`, the running one stopped first
    /// (dead or not: one loopback at a time). The new one gets the last
    /// level known, before any key's change. Err: why it did not start;
    /// none runs then.
    pub fn start(&self, delay_ms: u32) -> Result<Controls, String> {
        let s = &self.0;
        if s.stopped.get() {
            return Err("the feed is ending".into());
        }
        let delay_ms = check_delay(delay_ms)?;
        if let Some(slot) = s.level.borrow_mut().take()
            && let Some(Ok(level)) = slot.lock().expect("the level's lock is never poisoned").clone()
        {
            s.last_level.set(Some(level));
        }
        drop(s.running.borrow_mut().take());
        s.delay_ms.set(delay_ms);
        let running = (s.start)(delay_ms)?;
        let controls = running.controls();
        if let Some(level) = s.last_level.get() {
            controls.change_level(LevelChange::Set(level));
        }
        *s.level.borrow_mut() = Some(controls.level.clone());
        *s.running.borrow_mut() = Some(running);
        Ok(controls)
    }

    /// The delay: the running loopback's, else the one last asked for.
    pub fn delay_ms(&self) -> u32 {
        match self.0.running.borrow().as_ref() {
            Some(r) => r.counters().target_ms.load(Ordering::Relaxed),
            None => self.0.delay_ms.get(),
        }
    }

    /// The running loopback's source, sink, buffer in ms and numbers, for
    /// the dashboard.
    pub fn link(&self) -> Option<(String, String, u32, Arc<Counters>)> {
        let running = self.0.running.borrow();
        let r = running.as_ref()?;
        let (source, sink, buffer_ms) = r.link();
        Some((source.into(), sink.into(), buffer_ms, r.counters().clone()))
    }

    pub fn describe(&self) -> Option<String> {
        self.0.running.borrow().as_ref().map(|r| r.describe())
    }
}

/// A delay moved by `by_ms`, clamped to DELAY's range.
pub fn nudged(ms: u32, by_ms: i32) -> u32 {
    (i64::from(ms) + i64::from(by_ms)).clamp(0, i64::from(MAX_DELAY_MS)) as u32
}

/// A stand-in loopback for tests (#29): no PipeWire, a level kept in
/// memory, a new stream at 100 % unmuted as WirePlumber's default, and a
/// death on demand.
#[cfg(test)]
pub mod fake {
    use super::*;

    struct Fake {
        counters: Arc<Counters>,
        notify: NotifySlot,
        level: Arc<Mutex<Level>>,
    }

    impl Running for Fake {
        fn controls(&self) -> Controls {
            let level = self.level.clone();
            let level_fn: LevelFn = Arc::new(move |change| {
                let mut l = level.lock().unwrap();
                if let Some(c) = change {
                    *l = l.changed(c);
                }
                Ok(*l)
            });
            Controls::new(self.counters.clone(), self.notify.clone(), level_fn)
        }

        fn counters(&self) -> &Arc<Counters> {
            &self.counters
        }

        fn link(&self) -> (&str, &str, u32) {
            ("card-in", "speakers", 10)
        }

        fn describe(&self) -> String {
            "audio: card-in -> speakers".into()
        }

        fn granted(&self) -> String {
            "granted: none".into()
        }

        fn status(&self) -> String {
            "status: none".into()
        }
    }

    /// The delays each start was asked for; set `fail` for the next starts
    /// to fail with it.
    #[derive(Clone, Default)]
    pub struct Script {
        pub starts: Rc<RefCell<Vec<u32>>>,
        pub fail: Rc<RefCell<Option<String>>>,
    }

    pub fn loopback(delay_ms: u32) -> (Loopback, Script) {
        let script = Script::default();
        let s = script.clone();
        let start: Start = Box::new(move |delay_ms| {
            s.starts.borrow_mut().push(delay_ms);
            if let Some(e) = s.fail.borrow().clone() {
                return Err(e);
            }
            let counters = Arc::new(Counters { target_ms: AtomicU32::new(delay_ms), ..Default::default() });
            Ok(Box::new(Fake { counters, notify: NotifySlot::default(), level: Arc::new(Mutex::new(Level { volume: 100, mute: false })) }))
        });
        (Loopback::new(start, delay_ms), script)
    }

    /// The running loopback's stream goes away, as cpal reports it.
    pub fn kill(keys: &LoopbackKeys) {
        let (_, _, _, counters) = keys.link().expect("a loopback runs");
        died(&counters, &NotifySlot::default(), "output: Device disconnected".into());
    }

    /// The level once the controls' thread has done what was asked: `want`
    /// within a second, or what it is then.
    pub fn level_settles(c: &Controls, want: Level) -> Option<Result<Level, String>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let now = c.current_level();
            if now == Some(Ok(want)) || std::time::Instant::now() >= deadline {
                return now;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// m, 9 and 0 against PipeWire itself, read back through pactl as
    /// lib/audio.sh reads them: `cargo test -- --ignored level_on_pipewire`.
    /// Plays silence on the default output for a moment.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "needs a PipeWire session"]
    fn level_on_pipewire() {
        use crate::pw::{level, LevelChange};
        let host = cpal::host_from_id(HostId::PipeWire).unwrap();
        let dev = host.default_output_device().unwrap();
        let config = StreamConfig { channels: CHANNELS, sample_rate: RATE, buffer_size: BufferSize::Default };
        let stream = dev.build_output_stream::<f32, _, _>(config, |out, _| out.fill(0.0), |e| eprintln!("{e}"), None).unwrap();
        stream.play().unwrap();
        std::thread::sleep(Duration::from_millis(500));

        // pactl's view of this process's stream: "volume mute".
        let pactl = || {
            let out = std::process::Command::new("pactl").args(["list", "sink-inputs"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let pid = format!("media.name = \"cpal-playback-{}\"", std::process::id());
            let block = text.split("Sink Input #").find(|b| b.contains(&pid)).unwrap_or_else(|| panic!("our stream ({pid}) not in pactl:\n{text}"));
            let field = |k: &str| block.lines().find_map(|l| l.trim().strip_prefix(k)).unwrap().trim().to_string();
            let vol: u32 = field("Volume:").split_whitespace().nth(3).unwrap().trim_end_matches('%').parse().unwrap();
            (vol, field("Mute:") == "yes")
        };

        let start = level(None).unwrap();
        assert_eq!((start.volume, start.mute), pactl(), "read as pactl reads it");
        let down = level(Some(LevelChange::Volume(-5))).unwrap();
        assert_eq!(down.volume, start.volume.saturating_sub(5));
        assert_eq!((down.volume, down.mute), pactl());
        let muted = level(Some(LevelChange::Mute)).unwrap();
        assert_eq!((muted.volume, muted.mute), (down.volume, !start.mute));
        assert_eq!(pactl(), (down.volume, !start.mute));
        // Back as it was.
        level(Some(LevelChange::Mute)).unwrap();
        let back = level(Some(LevelChange::Volume(start.volume as i32 - down.volume as i32))).unwrap();
        assert_eq!(back, start);
        assert_eq!(pactl(), (start.volume, start.mute));
    }

    #[test]
    fn a_stream_gone_is_a_dead_loopback_said_once() {
        let counters = Arc::new(Counters::default());
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let notify = NotifySlot::default();
        let s = said.clone();
        *notify.lock().unwrap() = Some(Arc::new(move |m: Option<(String, Duration)>| s.lock().unwrap().extend(m.map(|m| m.0))));
        let mut output = on_error(&counters, &notify, "output");
        let mut input = on_error(&counters, &notify, "input");
        output(cpal::ErrorKind::Xrun.into());
        assert_eq!((counters.xruns.load(Ordering::Relaxed), counters.dead()), (1, None), "an xrun is not a death");
        output(cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "Device disconnected"));
        input(cpal::Error::with_message(ErrorKind::StreamInvalidated, "error"));
        assert_eq!(counters.dead().as_deref(), Some("output: Device disconnected"), "the first one says why");
        assert_eq!(*said.lock().unwrap(), ["Audio stopped (output: Device disconnected): ctrl -/+, m, 9 or 0 start it again"]);
        // Torn down on purpose: PipeWire reports that as a disconnect too.
        let closing = Arc::new(Counters::default());
        closing.closing.store(true, Ordering::Relaxed);
        on_error(&closing, &notify, "output")(cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "Device disconnected"));
        assert_eq!(closing.dead(), None);
    }

    #[test]
    fn a_new_loopback_gets_the_last_ones_level_and_the_delay_asked_for() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        let first = keys.start(keys.delay_ms()).unwrap();
        first.change_level(LevelChange::Volume(-30));
        first.change_level(LevelChange::Mute);
        let was = Level { volume: 70, mute: true };
        assert_eq!(fake::level_settles(&first, was), Some(Ok(was)));
        fake::kill(&keys);
        assert!(first.dead().is_some());

        let second = keys.start(60).unwrap();
        assert_eq!(*script.starts.borrow(), [50, 60]);
        assert_eq!(keys.delay_ms(), 60);
        assert_eq!(second.dead(), None, "a new stream, not the dead one");
        // The new stream starts at 100 %, unmuted, and gets the last level.
        assert_eq!(fake::level_settles(&second, was), Some(Ok(was)));
    }

    #[test]
    fn a_start_that_fails_runs_nothing_and_keeps_the_delay() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        *script.fail.borrow_mut() = Some("no PipeWire source card-in (SRC)".into());
        assert_eq!(keys.start(80).err().as_deref(), Some("no PipeWire source card-in (SRC)"));
        assert!(keys.link().is_none());
        assert_eq!(keys.delay_ms(), 80, "the next start's");
        assert!(keys.start(MAX_DELAY_MS + 1).is_err(), "over DELAY's range");
        // The feed ended it: the keys start nothing more.
        drop(loopback);
        *script.fail.borrow_mut() = None;
        assert_eq!(keys.start(50).err().as_deref(), Some("the feed is ending"));
        assert_eq!(*script.starts.borrow(), [80]);
    }

    #[test]
    fn sync_keys_clamp_to_delays_range() {
        assert_eq!(nudged(50, 10), 60);
        assert_eq!(nudged(5, -10), 0);
        assert_eq!(nudged(990, 50), MAX_DELAY_MS);
        // DELAY's range in lib/settings.sh is the ring's.
        let rows = crate::settings::rows();
        let delay = rows.iter().find(|r| r.name == "DELAY").unwrap();
        assert_eq!(delay.values, format!("num:0..{}", MAX_DELAY_MS / 1000));
    }

    #[test]
    fn the_ring_holds_the_largest_delay_with_room_to_spare() {
        // Priming needs the fill to reach the target while the input
        // callback trims to the ring's size: a ring the size of the delay
        // never primes (the bug: 1 s ring, delay over 1 s, silence). The
        // extremes: LAT_MS from 1 to 1000 ms.
        for buffer_ms in [1, 10, 1000] {
            let buffer = buffer_ms * RATE / 1000;
            assert!(ring_frames(buffer) >= frames_for(MAX_DELAY_MS) + 2 * buffer as usize, "LAT_MS={buffer_ms}");
            // The largest target plus an input block still fits.
            assert!(ring_frames(buffer) >= target_frames(MAX_DELAY_MS, buffer) + buffer as usize, "LAT_MS={buffer_ms}");
        }
    }

    #[test]
    fn a_zero_delay_still_keeps_a_buffer_in_the_ring() {
        // DELAY=0 steered the fill to nothing: 890 underruns in 25 s on
        // the MS2130 (2026-10-16), none at 10 ms.
        let buffer = 10 * RATE / 1000;
        assert_eq!(target_frames(0, buffer), buffer as usize);
        assert_eq!(target_frames(50, buffer), frames_for(50) + buffer as usize);
    }

    #[test]
    fn a_delay_over_the_maximum_is_refused_at_start() {
        assert_eq!(check_delay(MAX_DELAY_MS), Ok(MAX_DELAY_MS));
        assert!(check_delay(MAX_DELAY_MS + 1).is_err());
        assert_eq!(check_delay(0), Ok(0));
    }
}
