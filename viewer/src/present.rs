//! When a frame actually reached the screen, and when the compositor is
//! ready for the next: the platform seam `Feedback`. On Wayland,
//! `wp_presentation` feedback and the `wl_surface.frame` callback on
//! the window's own `wl_surface`. wgpu has no presentation timing, and the
//! moment `present()` returns is a submit time, not scanout; the compositor
//! knows the real one.
//!
//! On Windows, wgpu's DX12 swapchain is a flip-model one (FLIP_DISCARD)
//! with a frame-latency waitable object that acquiring the next texture
//! waits on (wgpu-hal 30, dx12/mod.rs), so the frame latency app.rs asks
//! for bounds the queue and paces the presents. DXGI frame statistics time
//! them (dxgi.rs, #19); they do not pace.

use std::time::Duration;

#[cfg(any(windows, test))]
mod dxgi;
#[cfg(windows)]
pub use dxgi::FrameStatistics;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
pub use wayland::Presentation;

/// What we know about a frame when it is handed to the compositor.
#[derive(Clone, Copy)]
pub struct Tag {
    /// Kernel capture time (first USB packet), `CLOCK_MONOTONIC`.
    pub captured: Duration,
    /// Dequeued by the capture loop, `CLOCK_MONOTONIC`.
    pub dequeued: Duration,
    /// The last frame drawn again for the overlay (no new frame came): paced
    /// like any present, not measured.
    pub repaint: bool,
}

/// The platform seam for presentation timing and pacing.
pub trait Feedback {
    /// Asks about the next present: call right before it.
    fn request(&mut self, tag: Tag) -> Result<(), String>;
    /// What has come back since the last call.
    fn drain(&mut self) -> Result<Vec<Outcome>, String>;
    /// Whether presents wait for its `FrameDone` (Wayland's frame
    /// callback). False: it only times them, and the swapchain paces
    /// (DXGI, which has no frame callback).
    fn paces(&self) -> bool {
        true
    }
}

pub enum Outcome {
    Presented { tag: Tag, at: Duration, refresh: Duration, flags: u32 },
    Discarded,
    /// The frame callback: the compositor took the frame for a refresh and
    /// is ready for the next one (before that refresh is scanned out).
    FrameDone,
}
