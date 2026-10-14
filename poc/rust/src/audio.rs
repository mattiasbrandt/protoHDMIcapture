//! Audio: the card's PipeWire source -> a delay ring -> the default sink,
//! through cpal's PipeWire host, as today's `pw-loopback` does (10 ms
//! buffers, 0.05 s delay by default).
//!
//! The delay is the ring's fill. Changing it live never inserts silence or
//! drops a block: the output plays faster or slower (linear interpolation),
//! in proportion to how far the fill is from the target and at most
//! `CATCH_UP`, until it gets there.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, HostId, StreamConfig};

pub const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// The buffer asked of PipeWire: 480 frames = 10 ms.
pub const BUFFER_FRAMES: u32 = RATE / 100;
/// Fastest the output speeds up or slows down to reach a new delay.
const CATCH_UP: f64 = 0.03;
/// Speed change per second of delay error: 10 ms off plays 0.5 % off.
const GAIN: f64 = 0.5;
/// Closer than this to the target, play at exactly 1x.
const DEAD_BAND: f64 = 0.001;
pub const DEFAULT_DELAY_MS: u32 = 50;
/// The largest delay: view.sh's DELAY setting is 0..1 s.
pub const MAX_DELAY_MS: u32 = 1000;
/// The ring holds the largest delay plus room for the input running a
/// little ahead; a delay it cannot hold would never prime (silence).
const RING_FRAMES: usize = frames_for(MAX_DELAY_MS) + RATE as usize / 4;

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

impl Drop for Audio {
    fn drop(&mut self) {
        self.counters.closing.store(true, Ordering::Relaxed);
    }
}

impl Audio {
    /// `usb_id` is the card's `vvvv:pppp`: its PipeWire source is the
    /// `alsa_input.usb-vvvv_pppp…` node.
    pub fn start(usb_id: &str, delay_ms: u32) -> Result<Self, String> {
        let delay_ms = check_delay(delay_ms)?;
        let host = cpal::host_from_id(HostId::PipeWire).map_err(|e| format!("PipeWire host: {e}"))?;
        let want = format!("alsa_input.usb-{}", usb_id.replace(':', "_"));
        let mut input = None;
        for d in host.devices().map_err(|e| format!("devices: {e}"))? {
            let id = d.id().map_err(|e| e.to_string())?;
            let name = id.to_string();
            // DeviceId displays as "<host>:<node name>".
            if name.split_once(':').is_some_and(|(_, n)| n.starts_with(&want) && !n.ends_with(".monitor")) {
                input = Some((d, name));
                break;
            }
        }
        let (in_dev, in_name) = input.ok_or_else(|| format!("no PipeWire source {want}*"))?;
        let out_dev = host.default_output_device().ok_or("no default output device")?;
        let out_name = out_dev.id().map_err(|e| e.to_string())?.to_string();

        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: RATE,
            buffer_size: BufferSize::Fixed(BUFFER_FRAMES),
        };
        let counters = Arc::new(Counters {
            in_min: AtomicU32::new(u32::MAX),
            out_min: AtomicU32::new(u32::MAX),
            target_ms: AtomicU32::new(delay_ms),
            ..Default::default()
        });
        let ring = Arc::new(Mutex::new(Ring { frames: VecDeque::with_capacity(RING_FRAMES), pos: 0.0, primed: false }));

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
                        r.frames.extend(data.chunks_exact(2).map(|s| [s[0], s[1]]));
                        // A stalled output must not grow the delay
                        // without bound.
                        let excess = r.frames.len().saturating_sub(RING_FRAMES);
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
                        for (i, o) in out.chunks_exact_mut(2).enumerate() {
                            let k = pos as usize;
                            let (Some(a), Some(b)) = (r.frames.get(k), r.frames.get(k + 1)) else {
                                c.ring_underruns.fetch_add(1, Ordering::Relaxed);
                                out[i * 2..].fill(0.0);
                                break;
                            };
                            let t = (pos - k as f64) as f32;
                            o[0] = a[0] + (b[0] - a[0]) * t;
                            o[1] = a[1] + (b[1] - a[1]) * t;
                            pos += step;
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
        output.play().map_err(|e| format!("output play: {e}"))?;
        Ok(Self { _input: input, _output: output, input: in_name, output: out_name, counters })
    }

    pub fn describe(&self) -> String {
        format!(
            "audio: {} -> {}, asked {} frames ({} ms) at {} Hz, delay {} ms",
            self.input,
            self.output,
            BUFFER_FRAMES,
            BUFFER_FRAMES * 1000 / RATE,
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

    /// Sets the delay, capped at `MAX_DELAY_MS`; returns the delay set.
    pub fn set_delay_ms(&self, ms: u32) -> u32 {
        let ms = ms.min(MAX_DELAY_MS);
        self.counters.target_ms.store(ms, Ordering::Relaxed);
        ms
    }

    pub fn delay_ms(&self) -> u32 {
        self.counters.target_ms.load(Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_holds_the_largest_delay_with_room_to_spare() {
        // Priming needs the fill to reach the target while the input
        // callback trims to RING_FRAMES: a ring the size of the delay
        // never primes (the bug: 1 s ring, delay over 1 s, silence).
        assert!(RING_FRAMES >= frames_for(MAX_DELAY_MS) + 2 * BUFFER_FRAMES as usize);
    }

    #[test]
    fn a_delay_over_the_maximum_is_refused_at_start() {
        assert_eq!(check_delay(MAX_DELAY_MS), Ok(MAX_DELAY_MS));
        assert!(check_delay(MAX_DELAY_MS + 1).is_err());
        assert_eq!(check_delay(0), Ok(0));
    }
}
