//! Presentation timing on Windows (#19): DXGI frame statistics on wgpu's
//! flip-model swapchain. They time presents; they do not pace them, which
//! the swapchain's frame-latency wait goes on doing (`paces` is false).
//!
//! A reading names only the latest present that reached the screen: its
//! number (PresentCount), the refresh it was shown at (PresentRefreshCount),
//! and the latest vblank with its time (SyncRefreshCount, SyncQPCTime, on
//! the performance counter that `now_monotonic` reads). The presents
//! between two readings are placed on the refreshes between them. Where
//! more presents than refreshes came, the earliest were replaced before
//! they were shown (Mailbox presents with sync interval 0) and are
//! discarded.

use std::collections::VecDeque;
use std::time::Duration;

use super::{Outcome, Tag};

/// Presents waiting for a reading. A window the statistics stop for
/// (minimised) would grow it without end: past this, the oldest go as
/// discarded.
const MAX_PENDING: usize = 240;

/// One reading of the statistics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// The latest present shown (PresentCount).
    pub present: u32,
    /// The refresh it was shown at (PresentRefreshCount).
    pub shown_refresh: u32,
    /// The latest vblank (SyncRefreshCount) and when it was (SyncQPCTime).
    pub sync_refresh: u32,
    pub sync_at: Duration,
    /// How the frame reached the screen, as `Outcome::Presented`'s flags
    /// carry it: 0 unknown, else DXGI_FRAME_PRESENTATION_MODE + 1, in
    /// DXGI's names: 1 COMPOSED, 2 OVERLAY, 3 NONE, 4 COMPOSITION_FAILURE.
    pub flags: u32,
}

/// The presents asked about and the readings so far: what each reading
/// settles. Platform-free, so its arithmetic is tested everywhere.
#[derive(Default)]
pub struct Settle {
    /// Each present's number, with its tag.
    pending: VecDeque<(u32, Tag)>,
    /// The latest present shown and its refresh, at the last reading.
    shown: Option<(u32, u32)>,
    /// The first and the latest vblank seen: the refresh period between.
    first_sync: Option<(u32, Duration)>,
    last_sync: Option<(u32, Duration)>,
}

/// `a` comes after `b` among wrapping counts.
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Settle {
    /// A present about to be made, numbered as DXGI will count it. The
    /// oldest go as discarded past `MAX_PENDING`.
    pub fn request(&mut self, present: u32, tag: Tag) -> Vec<Outcome> {
        self.pending.push_back((present, tag));
        let over = self.pending.len().saturating_sub(MAX_PENDING);
        self.pending.drain(..over).map(|_| Outcome::Discarded).collect()
    }

    /// The statistics started over (DXGI_ERROR_FRAME_STATISTICS_DISJOINT:
    /// a mode change, or no present shown yet): earlier readings no longer
    /// count.
    pub fn disjoint(&mut self) {
        self.shown = None;
        self.first_sync = None;
        self.last_sync = None;
    }

    /// The refresh period, from the vblanks seen so far.
    pub fn refresh(&self) -> Option<Duration> {
        let ((r0, t0), (r1, t1)) = (self.first_sync?, self.last_sync?);
        let n = r1.wrapping_sub(r0);
        (n > 0 && t1 > t0).then(|| (t1 - t0) / n)
    }

    /// What `r` settles: each present up to the one it names, presented at
    /// its refresh or discarded.
    pub fn read(&mut self, r: Reading) -> Vec<Outcome> {
        if self.first_sync.is_none() {
            self.first_sync = Some((r.sync_refresh, r.sync_at));
        }
        self.last_sync = Some((r.sync_refresh, r.sync_at));
        let prev = self.shown;
        if prev.is_some_and(|(p, _)| p == r.present) {
            return Vec::new();
        }
        self.shown = Some((r.present, r.shown_refresh));
        let refresh = self.refresh();
        // The frame named was shown `sync - shown` refreshes before the
        // latest vblank.
        let behind = r.sync_refresh.wrapping_sub(r.shown_refresh);
        let at_named = match refresh {
            Some(p) => r.sync_at.checked_sub(p * behind),
            None if behind == 0 => Some(r.sync_at),
            None => None,
        };
        let mut settled = Vec::new();
        while let Some(&(id, _)) = self.pending.front() {
            if after(id, r.present) {
                break;
            }
            settled.push(self.pending.pop_front().expect("just seen"));
        }
        let mut out = Vec::new();
        // Before the first reading, only the frame named is known.
        let Some((prev_present, prev_refresh)) = prev else {
            if let (Some(&(id, tag)), Some(at)) = (settled.last(), at_named)
                && id == r.present
            {
                out.push(Outcome::Presented { tag, at, refresh: refresh.unwrap_or_default(), flags: r.flags });
            }
            return out;
        };
        // The refreshes since the last frame shown hold at most one present
        // each: the newest ones, back to back up to the frame named.
        let refreshes = r.shown_refresh.wrapping_sub(prev_refresh);
        for (id, tag) in settled {
            if !after(id, prev_present) {
                continue;
            }
            let back = r.present.wrapping_sub(id);
            match (at_named, refresh) {
                (Some(at), _) if back == 0 => {
                    out.push(Outcome::Presented { tag, at, refresh: refresh.unwrap_or_default(), flags: r.flags })
                }
                _ if back >= refreshes => out.push(Outcome::Discarded),
                (Some(at), Some(p)) => match at.checked_sub(p * back) {
                    Some(at) => out.push(Outcome::Presented { tag, at, refresh: p, flags: r.flags }),
                    None => out.push(Outcome::Discarded),
                },
                // No period yet to place it by: not measured, not a discard.
                _ => {}
            }
        }
        out
    }
}

#[cfg(windows)]
pub use statistics::FrameStatistics;

#[cfg(windows)]
mod statistics {
    use windows::Win32::Graphics::Dxgi::{
        DXGI_ERROR_FRAME_STATISTICS_DISJOINT, DXGI_FRAME_STATISTICS, DXGI_FRAME_STATISTICS_MEDIA, IDXGISwapChain3, IDXGISwapChainMedia,
    };
    use windows::core::Interface;

    use super::{Reading, Settle};
    use crate::present::{Feedback, Outcome, Tag};

    /// The Windows `Feedback`: DXGI frame statistics, timing only.
    pub struct FrameStatistics {
        swap_chain: IDXGISwapChain3,
        /// Where Windows offers it (8.1 and later): how each frame was
        /// shown (`Reading::flags`).
        media: Option<IDXGISwapChainMedia>,
        settle: Settle,
        /// Settled by a `request` past `MAX_PENDING`, for the next drain.
        overflow: Vec<Outcome>,
        /// An unexpected statistics error is said once; the feed goes on
        /// without timing.
        warned: bool,
    }

    impl FrameStatistics {
        /// On wgpu's DX12 surface, once it is configured (its swapchain
        /// exists from then, and resizing keeps it). None: not DX12, or
        /// not configured.
        pub fn new(surface: &wgpu::Surface) -> Option<Self> {
            // Safety: the hal surface is only read, while `surface` lives.
            let hal = unsafe { surface.as_hal::<wgpu::hal::api::Dx12>() }?;
            let swap_chain = hal.swap_chain()?;
            let media = swap_chain.cast::<IDXGISwapChainMedia>().ok();
            Some(Self { swap_chain, media, settle: Settle::default(), overflow: Vec::new(), warned: false })
        }

        fn reading(&self) -> windows::core::Result<Reading> {
            let ticks = |qpc: i64| crate::capture::qpc_duration(qpc);
            Ok(match &self.media {
                Some(m) => {
                    let mut s = DXGI_FRAME_STATISTICS_MEDIA::default();
                    // Safety: an out-struct on a live swapchain.
                    unsafe { m.GetFrameStatisticsMedia(&mut s) }?;
                    Reading {
                        present: s.PresentCount,
                        shown_refresh: s.PresentRefreshCount,
                        sync_refresh: s.SyncRefreshCount,
                        sync_at: ticks(s.SyncQPCTime),
                        flags: s.CompositionMode.0 as u32 + 1,
                    }
                }
                None => {
                    let mut s = DXGI_FRAME_STATISTICS::default();
                    // Safety: as above.
                    unsafe { self.swap_chain.GetFrameStatistics(&mut s) }?;
                    Reading {
                        present: s.PresentCount,
                        shown_refresh: s.PresentRefreshCount,
                        sync_refresh: s.SyncRefreshCount,
                        sync_at: ticks(s.SyncQPCTime),
                        flags: 0,
                    }
                }
            })
        }
    }

    impl Feedback for FrameStatistics {
        fn request(&mut self, tag: Tag) -> Result<(), String> {
            // Safety: a read on a live swapchain.
            let last = unsafe { self.swap_chain.GetLastPresentCount() }.map_err(|e| format!("DXGI present count: {e}"))?;
            // The present about to be made is the next one counted.
            let over = self.settle.request(last.wrapping_add(1), tag);
            self.overflow.extend(over);
            Ok(())
        }

        fn drain(&mut self) -> Result<Vec<Outcome>, String> {
            let mut out = std::mem::take(&mut self.overflow);
            match self.reading() {
                Ok(r) => out.extend(self.settle.read(r)),
                // No present shown yet, or the statistics started over.
                Err(e) if e.code() == DXGI_ERROR_FRAME_STATISTICS_DISJOINT => self.settle.disjoint(),
                Err(e) => {
                    if !self.warned {
                        self.warned = true;
                        eprintln!("warning: DXGI frame statistics: {e}; presents go untimed while it fails");
                    }
                }
            }
            Ok(out)
        }

        fn paces(&self) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Duration = Duration::from_micros(16_667);

    fn tag(n: u64) -> Tag {
        Tag { captured: Duration::from_millis(n), dequeued: Duration::from_millis(n), repaint: false }
    }

    fn reading(present: u32, shown: u32, sync: u32) -> Reading {
        Reading { present, shown_refresh: shown, sync_refresh: sync, sync_at: P * sync, flags: 1 }
    }

    /// (dequeued ms, presented at) for each presented outcome; "x" counts
    /// the discarded.
    fn summary(out: &[Outcome]) -> (Vec<(u64, Duration)>, usize) {
        let mut shown = Vec::new();
        let mut discarded = 0;
        for o in out {
            match o {
                Outcome::Presented { tag, at, .. } => shown.push((tag.dequeued.as_millis() as u64, *at)),
                Outcome::Discarded => discarded += 1,
                Outcome::FrameDone => panic!("DXGI has no frame callback"),
            }
        }
        (shown, discarded)
    }

    #[test]
    fn one_present_a_refresh_is_each_timed_at_its_refresh() {
        let mut s = Settle::default();
        for (i, id) in (1..=3).enumerate() {
            assert!(s.request(id, tag(i as u64)).is_empty());
        }
        // The first reading: only the frame it names is known.
        assert_eq!(summary(&s.read(reading(1, 100, 100))), (vec![(0, P * 100)], 0));
        // Two refreshes later, two more shown, back to back.
        assert_eq!(summary(&s.read(reading(3, 102, 102))), (vec![(1, P * 101), (2, P * 102)], 0));
        assert_eq!(s.refresh(), Some(P));
        // Nothing new shown: nothing settled.
        assert!(s.read(reading(3, 102, 105)).is_empty());
    }

    #[test]
    fn presents_replaced_within_a_refresh_are_discarded() {
        let mut s = Settle::default();
        s.request(1, tag(0));
        s.read(reading(1, 100, 100));
        // Three presents, one refresh: the last shown, the two before replaced.
        for (i, id) in (2..=4).enumerate() {
            s.request(id, tag(10 + i as u64));
        }
        assert_eq!(summary(&s.read(reading(4, 101, 101))), (vec![(12, P * 101)], 2));
    }

    #[test]
    fn the_frame_named_is_placed_back_from_a_later_vblank() {
        let mut s = Settle::default();
        s.request(1, tag(0));
        s.read(reading(1, 100, 100));
        s.request(2, tag(1));
        // Read three refreshes after it was shown.
        assert_eq!(summary(&s.read(reading(2, 101, 104))), (vec![(1, P * 101)], 0));
    }

    #[test]
    fn a_disjoint_start_over_and_presents_not_yet_shown() {
        let mut s = Settle::default();
        s.request(1, tag(0));
        s.request(2, tag(1));
        s.read(reading(1, 100, 100));
        // Not yet shown: waits for a later reading.
        assert!(summary(&s.read(reading(1, 100, 101))).0.is_empty());
        s.disjoint();
        assert_eq!(s.refresh(), None);
        // After the start over, the next reading is a first one again.
        assert_eq!(summary(&s.read(reading(2, 200, 200))), (vec![(1, P * 200)], 0));
    }

    #[test]
    fn counts_wrap() {
        let mut s = Settle::default();
        s.request(u32::MAX, tag(0));
        s.read(reading(u32::MAX, 100, 100));
        s.request(0, tag(1));
        s.request(1, tag(2));
        assert_eq!(summary(&s.read(reading(1, 102, 102))), (vec![(1, P * 101), (2, P * 102)], 0));
    }

    #[test]
    fn a_window_never_shown_does_not_grow_without_end() {
        let mut s = Settle::default();
        let discarded: usize = (0..MAX_PENDING as u32 + 5).map(|id| s.request(id, tag(0)).len()).sum();
        assert_eq!(discarded, 5);
    }
}
