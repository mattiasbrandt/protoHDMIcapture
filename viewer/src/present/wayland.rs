//! `Feedback` on Wayland: `wp_presentation` feedback and the
//! `wl_surface.frame` callback on the window's own `wl_surface`.
//!
//! The feedback objects live on our own event queue on winit's connection.
//! winit's loop reads the socket; `drain` only dispatches what it queued.

use std::ffi::c_void;
use std::time::Duration;

use wayland_backend::client::{Backend, ObjectId, WaylandError};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};

use super::{Feedback, Outcome, Tag};

#[derive(Default)]
struct State {
    clock_id: Option<u32>,
    outcomes: Vec<Outcome>,
}

pub struct Presentation {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    presentation: WpPresentation,
    surface: WlSurface,
}

impl Presentation {
    /// # Safety
    /// `display` and `surface` are winit's live `wl_display` and the
    /// window's `wl_surface`; both must outlive this value.
    pub unsafe fn new(display: *mut c_void, surface: *mut c_void) -> Result<Self, String> {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("wayland registry: {e}"))?;
        let qh = queue.handle();
        let presentation: WpPresentation = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("the compositor offers no wp_presentation: {e}"))?;
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
            .map_err(|e| format!("wl_surface: {e}"))?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| format!("wl_surface: {e}"))?;
        let mut state = State::default();
        queue.roundtrip(&mut state).map_err(|e| format!("wayland roundtrip: {e}"))?;
        Ok(Self { conn, queue, qh, state, presentation, surface })
    }

    /// The clock presentation times are in (`CLOCK_MONOTONIC` is 1).
    pub fn clock_id(&self) -> Option<u32> {
        self.state.clock_id
    }

}

impl Feedback for Presentation {
    /// Asks for feedback and a frame callback on the surface's next commit:
    /// call right before `present()`, whose commit they attach to.
    fn request(&mut self, tag: Tag) -> Result<(), String> {
        self.presentation.feedback(&self.surface, &self.qh, tag);
        self.surface.frame(&self.qh, ());
        self.conn.flush().map_err(|e| format!("wayland flush: {e}"))
    }

    /// Feedback that has arrived since the last call. Reads the socket too
    /// (never blocking): with EGL, Mesa reads it during swap, so winit is
    /// not always woken for ours.
    fn drain(&mut self) -> Result<Vec<Outcome>, String> {
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("wayland read: {e}")),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| format!("wayland dispatch: {e}"))?;
        Ok(std::mem::take(&mut self.state.outcomes))
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.outcomes.push(Outcome::FrameDone);
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<WpPresentationFeedback, Tag> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        tag: &Tag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, refresh, flags, .. } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let flags = match flags {
                    WEnum::Value(k) => k.bits(),
                    WEnum::Unknown(raw) => raw,
                };
                state.outcomes.push(Outcome::Presented {
                    tag: *tag,
                    at: Duration::new(secs, tv_nsec),
                    refresh: Duration::from_nanos(refresh.into()),
                    flags,
                });
            }
            wp_presentation_feedback::Event::Discarded => state.outcomes.push(Outcome::Discarded),
            _ => {}
        }
    }
}
