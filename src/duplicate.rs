//! Duplicate frames (#65, GLOSSARY "Duplicate frame"): which card frames
//! are new and which show the source's last picture again, told by their
//! content, and the source's own cadence from that.
//!
//! - **The detector** is FFmpeg `mpdecimate`'s test on luma: 8×8 blocks,
//!   each block's sum of absolute differences against the last new frame.
//!   A frame is a duplicate when no block's sum exceeds `HI` and at most
//!   `FRAC` of the blocks exceed `LO`. As in mpdecimate, the reference is
//!   the last *new* frame, not the previous card frame: a slow fade, every
//!   step under `LO`, still adds up to a new frame. Counting cannot tell
//!   this: a card repeats and drops frames itself, and V4L2's sequence
//!   numbers do not show drops inside the card.
//! - **The cadence** follows from the kinds: how many card frames each new
//!   frame is shown for (its hold). Holds of 2 are a 30 fps source in a
//!   60 Hz signal (2:2); holds of 1 a source that matches the signal.
//!   A hold other than 2 once 2:2 is established is a cadence break (base
//!   PS5 dropping under 30 fps gives a hold of 3).
//!
//! Both run on the capture thread (run.rs), per card frame, before the
//! frame goes into the slot.

use std::collections::VecDeque;
use std::time::Duration;

/// A block whose sum of absolute luma differences exceeds this makes the
/// frame new on its own. `HI`, `LO` and `FRAC` are mpdecimate's defaults
/// (`hi=64*12`, `lo=64*5`, `frac=0.33`), untuned until the first card run:
/// MJPEG and card noise sit somewhere between them and real motion.
pub const HI: u32 = 64 * 12;
/// A block over this counts towards `FRAC`.
pub const LO: u32 = 64 * 5;
/// More than this share of the blocks over `LO` makes the frame new.
pub const FRAC: f64 = 0.33;
/// The block's side, in pixels.
const BLOCK: usize = 8;

/// What a card frame is, against the last new frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A picture the source had not sent before (the first frame, too).
    New,
    /// The last new frame's picture again.
    Duplicate,
}

/// What the detector measured on a frame against the last new frame: the
/// largest 8×8 block's sum of absolute luma differences, and the share of
/// blocks over `LO` (0 for the first frame, which has nothing to compare
/// with). For tuning `HI`, `LO` and `FRAC` from a recording.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scores {
    pub largest: u32,
    pub over_lo: f64,
}

/// The detector: the last new frame's luma, and the block sums' scratch.
pub struct Detector {
    width: usize,
    height: usize,
    /// The last new frame's luma, `width * height`; empty before the first.
    reference: Vec<u8>,
    /// One block row's sums, `width / BLOCK`.
    sums: Vec<u32>,
}

impl Detector {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        Self { width, height, reference: Vec::new(), sums: vec![0; width / BLOCK] }
    }

    /// One YUYV frame (packed rows, `width * 2` bytes each: Y0 U Y1 V).
    /// Only whole 8×8 blocks are compared; a strip of fewer than 8 pixels at
    /// the right or bottom edge is not (none at 1920×1080 or 1280×720).
    /// Stops at the first block that settles it.
    pub fn classify(&mut self, yuyv: &[u8]) -> Kind {
        self.compare(yuyv, false).0
    }

    /// As `classify`, with every block compared for the scores (the
    /// recording's metadata, #66, a dev build's): slower, the same verdict.
    #[cfg_attr(not(feature = "dev"), allow(dead_code))]
    pub fn classify_scored(&mut self, yuyv: &[u8]) -> (Kind, Scores) {
        self.compare(yuyv, true)
    }

    /// The verdict, and the scores of the blocks compared: all of them when
    /// `full`, else up to the one that settled it.
    fn compare(&mut self, yuyv: &[u8], full: bool) -> (Kind, Scores) {
        let (w, h) = (self.width, self.height);
        assert_eq!(yuyv.len(), w * h * 2, "a frame of the detector's size");
        if self.reference.len() != w * h {
            self.keep(yuyv);
            return (Kind::New, Scores::default());
        }
        let blocks = (w / BLOCK) * (h / BLOCK);
        let allowed = (FRAC * blocks as f64) as usize;
        let mut over_lo = 0usize;
        let mut largest = 0u32;
        let mut new = false;
        'rows: for by in 0..h / BLOCK {
            self.sums.iter_mut().for_each(|s| *s = 0);
            for y in by * BLOCK..(by + 1) * BLOCK {
                let row = &yuyv[y * w * 2..(y + 1) * w * 2];
                let reference = &self.reference[y * w..(y + 1) * w];
                for (sum, (pixels, luma)) in
                    self.sums.iter_mut().zip(row.as_chunks::<{ BLOCK * 2 }>().0.iter().zip(reference.as_chunks::<BLOCK>().0))
                {
                    let mut d = 0u32;
                    for i in 0..BLOCK {
                        d += u32::from(pixels[i * 2].abs_diff(luma[i]));
                    }
                    *sum += d;
                }
            }
            for &sum in &self.sums {
                largest = largest.max(sum);
                if sum > LO {
                    over_lo += 1;
                }
                if sum > HI || over_lo > allowed {
                    new = true;
                    if !full {
                        break 'rows;
                    }
                }
            }
        }
        let scores = Scores { largest, over_lo: if blocks == 0 { 0.0 } else { over_lo as f64 / blocks as f64 } };
        if new {
            self.keep(yuyv);
            (Kind::New, scores)
        } else {
            (Kind::Duplicate, scores)
        }
    }

    /// The frame becomes the reference.
    fn keep(&mut self, yuyv: &[u8]) {
        self.reference.clear();
        self.reference.extend(yuyv.iter().step_by(2));
    }
}

/// The source's cadence, as the dashboard names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Too few new frames yet to say.
    Measuring,
    /// Each new frame held for two card frames: a 30 fps source in a 60 Hz
    /// signal.
    TwoTwo,
    /// Every card frame new: the source matches the signal.
    AllNew,
    /// Anything else, a break from 2:2 included, until 2:2 holds again.
    Uneven,
}

impl Cadence {
    pub fn as_str(self) -> &'static str {
        match self {
            Cadence::Measuring => "measuring",
            Cadence::TwoTwo => "2:2",
            Cadence::AllNew => "all new",
            Cadence::Uneven => "uneven",
        }
    }
}

/// Holds this many in a row of one length establish a cadence.
const ESTABLISH: usize = 4;
/// The source's frame rate covers this much time.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The cadence and the source's frame rate, from each card frame's kind and
/// capture time.
pub struct Tracker {
    /// The last holds, newest last: the card frames each new frame was
    /// shown for.
    holds: VecDeque<u32>,
    /// The hold still open: card frames since the last new frame (0 before
    /// the first).
    open: u32,
    /// 2:2 was established and has not broken since.
    two_two: bool,
    breaks: u64,
    /// The last `RATE_WINDOW` of card frames: (captured, new).
    recent: VecDeque<(Duration, bool)>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self { holds: VecDeque::with_capacity(ESTABLISH), open: 0, two_two: false, breaks: 0, recent: VecDeque::with_capacity(256) }
    }

    /// A card frame, in capture order.
    pub fn push(&mut self, kind: Kind, captured: Duration) {
        self.recent.push_back((captured, kind == Kind::New));
        while self.recent.front().is_some_and(|(t, _)| captured.saturating_sub(*t) > RATE_WINDOW) {
            self.recent.pop_front();
        }
        match kind {
            Kind::New if self.open > 0 => self.close(),
            Kind::New => {}
            Kind::Duplicate if self.open == 0 => return, // before the first new frame
            Kind::Duplicate => {}
        }
        self.open += 1;
        // A third card frame of one picture in an established 2:2: broken
        // now, counted once.
        if self.two_two && self.open > 2 {
            self.two_two = false;
            self.breaks += 1;
        }
    }

    /// The open hold ends: a new frame came.
    fn close(&mut self) {
        let hold = std::mem::take(&mut self.open);
        if self.holds.len() == ESTABLISH {
            self.holds.pop_front();
        }
        self.holds.push_back(hold);
        if self.two_two && hold != 2 {
            // A hold of 1 (a longer hold broke it while open).
            self.two_two = false;
            self.breaks += 1;
        } else if !self.two_two && self.holds.len() == ESTABLISH && self.holds.iter().all(|&h| h == 2) {
            self.two_two = true;
        }
    }

    pub fn cadence(&self) -> Cadence {
        if self.two_two {
            Cadence::TwoTwo
        } else if self.holds.len() < ESTABLISH {
            Cadence::Measuring
        } else if self.holds.iter().all(|&h| h == 1) && self.open <= 1 {
            Cadence::AllNew
        } else {
            Cadence::Uneven
        }
    }

    /// Departures from an established 2:2.
    pub fn breaks(&self) -> u64 {
        self.breaks
    }

    /// The card's frames per second and the source's (new frames per
    /// second), over the last second of capture times; None until frames
    /// span some time.
    pub fn rates(&self) -> Option<(f64, f64)> {
        let (first, last) = (self.recent.front()?.0, self.recent.back()?.0);
        let span = last.checked_sub(first)?.as_secs_f64();
        if span <= 0.0 || self.recent.len() < 2 {
            return None;
        }
        let signal = (self.recent.len() - 1) as f64 / span;
        let new = self.recent.iter().filter(|(_, n)| *n).count();
        Some((signal, signal * new as f64 / self.recent.len() as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;
    const H: usize = 32;

    /// A YUYV frame whose luma is `y` everywhere (chroma neutral).
    fn flat(y: u8) -> Vec<u8> {
        [y, 128].repeat(W * H)
    }

    /// A frame with a gradient picture numbered `n`: every block differs
    /// from another `n`'s by far more than `HI`.
    fn picture(n: u8) -> Vec<u8> {
        let mut f = flat(0);
        for (i, px) in f.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            px[0] = ((i % W) as u8).wrapping_mul(3).wrapping_add(n.wrapping_mul(60));
        }
        f
    }

    fn detector() -> Detector {
        Detector::new(W as u32, H as u32)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Kinds in, at 60 Hz: the tracker after them.
    fn track(kinds: &[Kind]) -> Tracker {
        let mut t = Tracker::new();
        for (i, &k) in kinds.iter().enumerate() {
            t.push(k, Duration::from_micros(i as u64 * 16_667));
        }
        t
    }

    use Kind::{Duplicate as D, New as N};

    #[test]
    fn the_first_frame_is_new_and_the_same_picture_again_is_a_duplicate() {
        let mut d = detector();
        assert_eq!(d.classify(&picture(1)), N);
        assert_eq!(d.classify(&picture(1)), D);
        assert_eq!(d.classify(&picture(2)), N);
        assert_eq!(d.classify(&picture(2)), D);
    }

    #[test]
    fn the_scored_verdict_is_classifys_with_every_block_scored() {
        let (mut quick, mut full) = (detector(), detector());
        let frames = [picture(1), picture(1), flat(100), flat(100), picture(2), flat(102), flat(100)];
        for f in &frames {
            let (kind, scores) = full.classify_scored(f);
            assert_eq!(kind, quick.classify(f));
            assert!(scores.over_lo >= 0.0 && scores.over_lo <= 1.0);
        }
        // flat(102) against flat(100): 2 a pixel, 128 a block, every block.
        let mut d = detector();
        d.classify(&flat(100));
        assert_eq!(d.classify_scored(&flat(102)), (D, Scores { largest: 128, over_lo: 0.0 }));
        assert_eq!(d.classify_scored(&flat(110)).1, Scores { largest: 640, over_lo: 1.0 });
    }

    #[test]
    fn noise_under_lo_stays_a_duplicate() {
        let mut d = detector();
        d.classify(&flat(100));
        // ±4 on every pixel: 8×8×4 = 256 a block, under LO (320).
        let mut noisy = flat(100);
        for (i, px) in noisy.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            px[0] = if i % 2 == 0 { 104 } else { 96 };
        }
        assert_eq!(d.classify(&noisy), D);
    }

    #[test]
    fn one_moving_block_is_new() {
        let mut d = detector();
        d.classify(&flat(100));
        let mut moved = flat(100);
        // One 8×8 block, each pixel +13: 832 > HI (768).
        for y in 8..16 {
            for x in 16..24 {
                moved[(y * W + x) * 2] = 113;
            }
        }
        assert_eq!(d.classify(&moved), N);
    }

    #[test]
    fn many_blocks_over_lo_are_new_and_few_are_not() {
        let blocks = (W / 8) * (H / 8); // 32
        let raise = |n: usize| {
            // +6 on each pixel of the first n blocks: 384, over LO, under HI.
            let mut f = flat(100);
            for b in 0..n {
                let (bx, by) = (b % (W / 8), b / (W / 8));
                for y in by * 8..by * 8 + 8 {
                    for x in bx * 8..bx * 8 + 8 {
                        f[(y * W + x) * 2] = 106;
                    }
                }
            }
            f
        };
        let allowed = (FRAC * blocks as f64) as usize; // 10
        let mut d = detector();
        d.classify(&flat(100));
        assert_eq!(d.classify(&raise(allowed)), D);
        assert_eq!(d.classify(&raise(allowed + 1)), N);
    }

    #[test]
    fn a_slow_fade_adds_up_against_the_last_new_frame() {
        // Each step is +2 a pixel (128 a block, under LO); against the last
        // new frame the steps add up until one is new.
        let mut d = detector();
        assert_eq!(d.classify(&flat(100)), N);
        let kinds: Vec<Kind> = (1..=10).map(|i| d.classify(&flat(100 + 2 * i))).collect();
        assert!(kinds.contains(&N), "{kinds:?}");
        assert_eq!(kinds[0], D);
    }

    #[test]
    fn chroma_is_not_compared() {
        let mut d = detector();
        d.classify(&flat(100));
        let mut tinted = flat(100);
        for px in tinted.as_chunks_mut::<2>().0.iter_mut() {
            px[1] = 200;
        }
        assert_eq!(d.classify(&tinted), D);
    }

    #[test]
    fn two_two_is_a_30_fps_source_in_a_60_hz_signal() {
        let kinds: Vec<Kind> = [N, D].repeat(60);
        let t = track(&kinds);
        assert_eq!(t.cadence(), Cadence::TwoTwo);
        assert_eq!(t.breaks(), 0);
        let (signal, source) = t.rates().unwrap();
        assert!((signal - 60.0).abs() < 0.1, "{signal}");
        assert!((source - 30.0).abs() < 0.5, "{source}");
    }

    #[test]
    fn the_detector_feeds_the_tracker_a_b_c_doubled() {
        // A A B B C C …: pictures, through the detector, into the tracker.
        let mut d = detector();
        let mut t = Tracker::new();
        for i in 0..40u64 {
            let kind = d.classify(&picture((i / 2) as u8));
            t.push(kind, Duration::from_micros(i * 16_667));
        }
        assert_eq!(t.cadence(), Cadence::TwoTwo);
        assert!((t.rates().unwrap().1 - 30.0).abs() < 0.5);
    }

    #[test]
    fn every_frame_new_is_all_new() {
        let t = track(&[N; 60]);
        assert_eq!(t.cadence(), Cadence::AllNew);
        assert_eq!(t.breaks(), 0);
        let (signal, source) = t.rates().unwrap();
        assert!((signal - source).abs() < 1e-9 && (source - 60.0).abs() < 0.1, "{signal} {source}");
    }

    #[test]
    fn a_hold_of_three_breaks_2_2_once_until_it_is_back() {
        let mut kinds = [N, D].repeat(8);
        kinds.extend([N, D, D]); // a 2:3
        let t = track(&kinds);
        assert_eq!(t.cadence(), Cadence::Uneven, "broken while the third frame is open");
        assert_eq!(t.breaks(), 1);
        kinds.extend([N, D].repeat(2));
        let t = track(&kinds);
        assert_eq!(t.cadence(), Cadence::Uneven, "two holds of 2 do not re-establish it");
        assert_eq!(t.breaks(), 1);
        kinds.extend([N, D].repeat(3));
        let t = track(&kinds);
        assert_eq!(t.cadence(), Cadence::TwoTwo);
        assert_eq!(t.breaks(), 1);
        kinds.extend([N, N]); // a hold of 1 breaks it too
        assert_eq!(track(&kinds).breaks(), 2);
    }

    #[test]
    fn a_still_scene_is_one_break_not_many() {
        let mut kinds = [N, D].repeat(8);
        kinds.extend([D; 100]);
        let t = track(&kinds);
        assert_eq!(t.breaks(), 1);
        assert_eq!(t.cadence(), Cadence::Uneven);
        assert!(t.rates().unwrap().1 < 1.0, "no new frame in the last second");
    }

    #[test]
    fn too_few_frames_say_measuring() {
        let t = track(&[N, D, N, D]);
        assert_eq!(t.cadence(), Cadence::Measuring);
        assert_eq!(Tracker::new().rates(), None);
        let mut t = Tracker::new();
        t.push(N, ms(5));
        assert_eq!(t.rates(), None, "one frame spans no time");
    }

    /// The cost bound (#65): at most 2 ms per 1080p frame in a release
    /// build, for a duplicate (the whole frame compared) and a new frame
    /// (compared until the first block over HI, then kept). Run with
    /// `cargo test --release -- --ignored a_1080p_frame`.
    #[test]
    #[ignore = "a timing; run in a release build"]
    fn a_1080p_frame_takes_under_2_ms() {
        let (w, h) = (1920usize, 1080usize);
        let frame = |seed: u8| -> Vec<u8> { (0..w * h * 2).map(|i| if i % 2 == 0 { ((i / 2) % 251) as u8 ^ seed } else { 128 }).collect() };
        let (a, b) = (frame(0), frame(0));
        let mut d = Detector::new(w as u32, h as u32);
        d.classify(&a);
        let n = 200;
        let start = std::time::Instant::now();
        for _ in 0..n {
            assert_eq!(d.classify(&b), Kind::Duplicate);
        }
        let duplicate = start.elapsed() / n;
        // The worst new frame: its only change in the last block row, so
        // all of it is compared, then kept.
        let mut late = a.clone();
        for y in h - 8..h {
            for x in 0..8 {
                late[(y * w + x) * 2] ^= 0x80;
            }
        }
        let start = std::time::Instant::now();
        for i in 0..n {
            assert_eq!(d.classify(if i % 2 == 0 { &late } else { &a }), Kind::New);
        }
        let new = start.elapsed() / n;
        eprintln!("1080p: duplicate {duplicate:?}, new (worst) {new:?}");
        assert!(duplicate < Duration::from_millis(2) && new < Duration::from_millis(2), "{duplicate:?} {new:?}");
    }
}
