//! A test pattern in place of the card (`--test-pattern`, #18): colour
//! bars with a stripe that moves one step a frame, at the settings' RES and
//! FPS. It shows the display and audio paths working where no card can be
//! reached (the Windows VM when USB passthrough fails) and makes judder
//! visible as an uneven stripe.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::capture::{Event, Format, Frame, FrameSource, Pixels, now_monotonic};

/// 75% colour bars, BT.709 limited range (Y, Cb, Cr): white, yellow, cyan,
/// green, magenta, red, blue, black.
const BARS: [(u8, u8, u8); 8] =
    [(180, 128, 128), (168, 44, 136), (145, 147, 44), (133, 63, 52), (63, 193, 204), (51, 109, 212), (28, 212, 120), (16, 128, 128)];
/// The moving stripe: full white, this many pixels wide (even, for YUYV).
const STRIPE: u32 = 16;
/// How far the stripe moves a frame, in pixels (even).
const STEP: u32 = 8;

pub struct Pattern {
    format: Format,
    /// The bars, one YUYV frame, drawn once.
    bars: Vec<u8>,
}

impl Pattern {
    /// At the size and rate asked for; always YUYV, whatever FMT says,
    /// since nothing is decoded.
    pub fn new(want: Format) -> Self {
        let format = Format { pixels: Pixels::Yuyv, ..want };
        let (w, h) = (format.width as usize, format.height as usize);
        let mut bars = vec![0u8; format.frame_size()];
        for row in bars.chunks_exact_mut(w * 2).take(h) {
            for (pair, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (y, cb, cr) = BARS[pair * 2 * BARS.len() / w];
                px.copy_from_slice(&[y, cb, y, cr]);
            }
        }
        Self { format, bars }
    }

    /// Frame `n`: the bars with the stripe at its place for that frame.
    fn draw(&self, n: u64, out: &mut [u8]) {
        out.copy_from_slice(&self.bars);
        let w = self.format.width;
        let travel = u64::from(w.saturating_sub(STRIPE).max(STEP));
        let x0 = ((n * u64::from(STEP)) % travel) as u32 & !1;
        for row in out.chunks_exact_mut(w as usize * 2) {
            for x in (x0..(x0 + STRIPE).min(w)).step_by(2) {
                let i = x as usize * 2;
                row[i..i + 4].copy_from_slice(&[235, 128, 235, 128]);
            }
        }
    }
}

impl FrameSource for Pattern {
    fn format(&self) -> Format {
        self.format
    }

    /// One frame per period of FPS, on an absolute schedule; a frame late by
    /// more than a period starts the schedule again rather than catching up.
    fn run(&mut self, quit: &AtomicBool, on_event: &mut dyn FnMut(Event<'_>)) -> io::Result<()> {
        let period = Duration::from_secs_f64(1.0 / self.format.fps.max(1.0));
        let mut frame = vec![0u8; self.format.frame_size()];
        let mut next = Instant::now();
        let mut n = 0u64;
        while !quit.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                // Short sleeps, so a quit is seen within one.
                thread::sleep((next - now).min(Duration::from_millis(100)));
                continue;
            }
            self.draw(n, &mut frame);
            let t = now_monotonic();
            on_event(Event::Frame(Frame { data: &frame, captured: t, dequeued: t }));
            n += 1;
            next += period;
            if Instant::now() > next + period {
                next = Instant::now() + period;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_then_a_stripe_that_moves() {
        let p = Pattern::new(Format { width: 64, height: 2, fps: 60.0, pixels: Pixels::Mjpeg });
        assert_eq!(p.format().pixels, Pixels::Yuyv, "nothing to decode");
        let (mut a, mut b) = (vec![0u8; p.format().frame_size()], vec![0u8; p.format().frame_size()]);
        p.draw(0, &mut a);
        p.draw(1, &mut b);
        // Left of the stripe at frame 1: the first bar (white); the stripe is full white.
        assert_eq!(&a[0..4], &[235, 128, 235, 128]);
        assert_eq!(&b[0..4], &[180, 128, 180, 128]);
        assert_eq!(&b[16..20], &[235, 128, 235, 128], "8 pixels on");
        // The last bar is black, on every row.
        assert_eq!(&a[124..128], &[16, 128, 16, 128]);
        assert_eq!(&a[252..256], &[16, 128, 16, 128]);
    }

    #[test]
    fn frames_come_at_the_rate_and_stop_on_quit() {
        let mut p = Pattern::new(Format { width: 64, height: 2, fps: 100.0, pixels: Pixels::Yuyv });
        let quit = AtomicBool::new(false);
        let start = Instant::now();
        let mut frames = 0;
        p.run(&quit, &mut |e| {
            if let Event::Frame(_) = e {
                frames += 1;
                if frames == 20 {
                    quit.store(true, Ordering::Relaxed);
                }
            }
        })
        .unwrap();
        let took = start.elapsed();
        assert_eq!(frames, 20);
        assert!(took >= Duration::from_millis(185) && took < Duration::from_secs(2), "{took:?}");
    }
}
