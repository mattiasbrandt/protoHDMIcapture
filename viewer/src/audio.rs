//! Audio: the card audio input -> a delay ring -> the sink, through cpal's
//! PipeWire host, as view.sh's `pw-loopback` does: SRC (else the card's own
//! source, `pw::Snapshot::card_source`), SINK (else the default), LAT_MS
//! buffers, DELAY. cpal is the seam for now; a native PipeWire path can
//! replace it without touching the rest (decided in #10).
//!
//! The delay is the ring's fill. Changing it live never inserts silence or
//! drops a block: the output plays faster or slower (linear interpolation),
//! in proportion to how far the fill is from the target and at most
//! `CATCH_UP`, until it gets there.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, HostId, StreamConfig};

use crate::pw::{Level, LevelChange};

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
}

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
}

fn seen(min: &AtomicU32, max: &AtomicU32, n: u32) {
    min.fetch_min(n, Ordering::Relaxed);
    max.fetch_max(n, Ordering::Relaxed);
}

fn on_error(counters: &Arc<Counters>, side: &'static str) -> impl FnMut(cpal::Error) + Send + 'static {
    let counters = counters.clone();
    move |e| {
        if counters.closing.load(Ordering::Relaxed) {
            return;
        }
        if e.kind() == ErrorKind::Xrun {
            counters.xruns.fetch_add(1, Ordering::Relaxed);
        } else {
            counters.other_errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("audio {side}: {e}");
        }
    }
}

/// How long the session manager gets to link the input.
const LINK_WAIT: Duration = Duration::from_secs(2);

/// Fails unless the source linked into our capture stream is `want`, and
/// only it, within LINK_WAIT.
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.counters.closing.store(true, Ordering::Relaxed);
    }
}

impl Audio {
    /// `source` and `sink` are PipeWire node names (no sink: the default);
    /// `buffer_ms` is LAT_MS.
    pub fn start(source: &str, sink: Option<&str>, delay_ms: u32, buffer_ms: u32) -> Result<Self, String> {
        let delay_ms = check_delay(delay_ms)?;
        let host = cpal::host_from_id(HostId::PipeWire).map_err(|e| format!("PipeWire host: {e}"))?;
        // DeviceId displays as "<host>:<node name>".
        let by_name = |want: &str| -> Result<Option<cpal::Device>, String> {
            for d in host.devices().map_err(|e| format!("audio devices: {e}"))? {
                let id = d.id().map_err(|e| e.to_string())?.to_string();
                if id.split_once(':').is_some_and(|(_, n)| n == want) {
                    return Ok(Some(d));
                }
            }
            Ok(None)
        };
        let in_dev = by_name(source)?.ok_or_else(|| format!("no PipeWire source {source} (SRC)"))?;
        let in_name = source.to_string();
        let (out_dev, out_name) = match sink {
            Some(sink) => (by_name(sink)?.ok_or_else(|| format!("no PipeWire sink {sink} (SINK)"))?, sink.to_string()),
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
                    on_error(&counters, "input"),
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
                        let target = frames_for(c.target_ms.load(Ordering::Relaxed));
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
                    on_error(&counters, "output"),
                    None,
                )
                .map_err(|e| format!("output stream on {out_name}: {e}"))?
        };
        input.play().map_err(|e| format!("input play: {e}"))?;
        // The output starts only once the input is proven linked to the
        // card, so a fallback source never reaches the speakers.
        verify_linked(source)?;
        output.play().map_err(|e| format!("output play: {e}"))?;
        Ok(Self { _input: input, _output: output, input: in_name, output: out_name, buffer_frames, counters })
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

    /// The live keys' handle: sync and the output stream's level.
    pub fn controls(&self) -> Controls {
        let (tx, rx) = mpsc::channel::<Option<LevelChange>>();
        let level: Arc<Mutex<Option<Result<Level, String>>>> = Arc::default();
        let l = level.clone();
        // PipeWire round trips off the window's thread, one change at a
        // time (lib/audio.sh serialises them with a lock); the thread ends
        // with the last sender.
        thread::spawn(move || {
            for change in rx {
                let r = crate::pw::level(change);
                match &r {
                    Ok(v) if change.is_some() => {
                        eprintln!("{}", if v.mute { "audio muted".into() } else { format!("volume {}%", v.volume) })
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("audio level: {e}"),
                }
                *l.lock().expect("the level's lock is never poisoned") = Some(r);
            }
        });
        // What WirePlumber restored for the stream, for the panel.
        let _ = tx.send(None);
        Controls { counters: self.counters.clone(), tx, level }
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

/// The audio keys' side of the audio: the delay target, read by the
/// output callback (a change plays faster or slower until the ring gets
/// there, never a restart), and the output stream's level.
pub struct Controls {
    counters: Arc<Counters>,
    tx: mpsc::Sender<Option<LevelChange>>,
    /// The last level read or set, or why it could not be.
    #[expect(dead_code, reason = "the overlay's Audio row reads it (#12)")]
    pub level: Arc<Mutex<Option<Result<Level, String>>>>,
}

impl Controls {
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

    /// m, 9 and 0: done on the controls' thread.
    pub fn change_level(&self, change: LevelChange) {
        let _ = self.tx.send(Some(change));
    }
}

fn nudged(ms: u32, by_ms: i32) -> u32 {
    (i64::from(ms) + i64::from(by_ms)).clamp(0, i64::from(MAX_DELAY_MS)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// m, 9 and 0 against PipeWire itself, read back through pactl as
    /// lib/audio.sh reads them: `cargo test -- --ignored level_on_pipewire`.
    /// Plays silence on the default output for a moment.
    #[test]
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
        }
    }

    #[test]
    fn a_delay_over_the_maximum_is_refused_at_start() {
        assert_eq!(check_delay(MAX_DELAY_MS), Ok(MAX_DELAY_MS));
        assert!(check_delay(MAX_DELAY_MS + 1).is_err());
        assert_eq!(check_delay(0), Ok(0));
    }
}
