//! The window: wgpu draws each frame the moment it arrives (not on a redraw
//! request, which Wayland throttles), YUYV -> RGB at the capture's own size
//! and then scaled into a letterboxed viewport (ADR 0001), handed to the
//! compositor only once it has taken the previous one (#5: a 60.000 Hz card
//! outruns a 59.95 Hz display, and every frame handed over early queues).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::{Controls, LoopbackKeys};
use crate::capture::{Format, FrameSource, Reason, Signal, now_monotonic};
use crate::desktop::Desktop;
use crate::gputime::GpuTimer;
use crate::keys::{self, Action};
use crate::osd::{Osd, PageKind, Scene};
use crate::overlay::{self, Notify, Overlay, Page};
use crate::picture::{self, Colour, Picture};
#[cfg(target_os = "linux")]
use crate::present::Presentation;
use crate::present::{self, Feedback, Tag};
use crate::run::{End, Host, Outcome, Plan, Quit, Slot, Wake, Waker};
use crate::scale::{self, Axis, Filter, Scaling};
use crate::screenshot::Screenshot;
use crate::settings::{Row, Settings};
use crate::sources;
use crate::stats::Stats;
use crate::wizard::Wizard;

/// A frame callback this late counts as lost: pacing stops waiting for it.
const PACE_TIMEOUT: Duration = Duration::from_millis(100);
/// A held frame this fresh goes out on the compositor's frame callback
/// instead of waiting to be replaced: it came within the capture's jitter
/// of the callback. With none, every phase crossing of the capture and
/// display clocks (one each ~37 s at 60.00 against 59.97 Hz) dropped
/// frames in bursts, about 4.7 % of them; 5 ms leaves about one per two
/// minutes for 3.5 ms more latency (MS2130, 2026-10-16, decided with the
/// user).
const HOLD: Duration = Duration::from_millis(5);
/// How long a new dashboard has to map (`Desktop::move_dashboard`'s wait):
/// d meanwhile starts no second one.
const DASHBOARD_MAPS: Duration = Duration::from_secs(3);
/// How often the device is polled while a screenshot comes back.
const READBACK_POLL: Duration = Duration::from_millis(10);

/// WINDOW and WINDOW_SIZE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    Fullscreen,
    Borderless,
    Windowed { width: u32, height: u32 },
}

impl WindowMode {
    pub fn from_settings(window: &str, size: &str) -> Result<Self, String> {
        Ok(match window {
            "fullscreen" => Self::Fullscreen,
            "borderless" => Self::Borderless,
            "windowed" => {
                let (w, h) = size.split_once('x').ok_or(format!("WINDOW_SIZE={size} is not WIDTHxHEIGHT"))?;
                Self::Windowed {
                    width: w.parse().map_err(|_| format!("WINDOW_SIZE={size}: bad width"))?,
                    height: h.parse().map_err(|_| format!("WINDOW_SIZE={size}: bad height"))?,
                }
            }
            other => return Err(format!("WINDOW={other} is not fullscreen, borderless or windowed")),
        })
    }
}

pub struct Options {
    pub window: WindowMode,
    /// Fullscreen on this output instead of the focused one (measurements).
    pub monitor: Option<String>,
    /// End this window after this long (measurements): `Outcome::TimeUp`.
    pub seconds: Option<u64>,
    /// Print timing every 10 s and a summary (measurements).
    pub stats: bool,
    /// LEVELS and the colour values at start.
    pub picture: Picture,
    /// SCALING at start.
    pub scaling: Scaling,
    pub audio: AudioSetup,
    /// WINDOW_SIZE, for f's windowed mode.
    pub window_size: (u32, u32),
    /// Finishes the window's shape where the toolkit's requests are ignored.
    pub desktop: Desktop,
    /// SOURCE: whose setup tips the overlay opens on.
    pub source: String,
    /// The settings, for w (None: w cannot save).
    pub settings: Option<Settings>,
    /// The setup wizard, when this window is its (#13): its page replaces
    /// the overlay and it takes every key.
    pub wizard: Option<Rc<RefCell<Wizard>>>,
    /// This run's dashboard numbers, from its caller (#21).
    pub live: Arc<crate::dashboard::Live>,
    /// A dashboard opened with the window (DEBUG=window): the dashboard
    /// windows there before it, for placing it with the window.
    pub dashboard: Option<Vec<String>>,
    /// The monitor that had focus before --monitor took it: given back once
    /// the window is placed, so typing meant for it does not reach the
    /// viewer.
    pub refocus: Option<String>,
}

/// A job for the placement worker: the window's mode, and a dashboard to
/// move off its monitor first (the windows there before it was started).
struct Placement {
    mode: &'static str,
    size: (u32, u32),
    dashboard: Option<Vec<String>>,
    /// A monitor to focus once placed (`Options::refocus`).
    refocus: Option<String>,
}

impl Placement {
    /// A newer job: its mode wins; a dashboard still to move and a focus
    /// still to give back are kept.
    fn then(self, newer: Placement) -> Placement {
        Placement { dashboard: self.dashboard.or(newer.dashboard), refocus: self.refocus.or(newer.refocus), ..newer }
    }
}

/// The sound, as the audio keys and the panel see it.
pub enum AudioSetup {
    /// AUDIO is not loopback: its value.
    Off(String),
    /// AUDIO=loopback: its controls while it runs, else why it does not
    /// (it did not start, or stopped mid-feed). The sync and volume keys
    /// start it again (#29).
    Loopback { keys: LoopbackKeys, now: Result<Controls, String> },
}

/// A loopback start's outcome, said where view.sh says it: the terminal,
/// the dashboard and, for a failure, a desktop notification. The first
/// start's (main.rs) and every restart's.
pub fn audio_started(live: &crate::dashboard::Live, keys: &LoopbackKeys, started: Result<Controls, String>) -> Result<Controls, String> {
    use crate::dashboard::AudioLive;
    match (&started, keys.link()) {
        (Ok(_), Some((source, sink, buffer_ms, counters))) => {
            if let Some(d) = keys.describe() {
                eprintln!("{d}");
            }
            live.set_audio(AudioLive::Running { source, sink, buffer_ms, counters });
        }
        (Ok(_), None) => unreachable!("a loopback that started runs"),
        (Err(e), _) => audio_failed(live, e),
    }
    started
}

/// The loopback is not running (it did not start, or stopped mid-feed):
/// the feed plays on without sound.
fn audio_failed(live: &crate::dashboard::Live, why: &str) {
    eprintln!("warning: no audio: {why}");
    no_sound(why.to_string());
    live.set_audio(crate::dashboard::AudioLive::Failed(why.to_string()));
}

/// The no-sound desktop notification (`desktop::Notice`); a test run
/// would reach the desktop, so it sends none.
fn no_sound(why: String) {
    if !cfg!(test) {
        crate::desktop::Notice::NoSound(why).send();
    }
}

impl WindowMode {
    /// f: fullscreen -> borderless -> windowed (WINDOW's order).
    fn next(self, size: (u32, u32)) -> Self {
        match self {
            Self::Fullscreen => Self::Borderless,
            Self::Borderless => Self::Windowed { width: size.0, height: size.1 },
            Self::Windowed { .. } => Self::Fullscreen,
        }
    }

    /// As the settings spell it, and as the panel names it.
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Self::Fullscreen => ("fullscreen", "Fullscreen"),
            Self::Borderless => ("borderless", "Borderless"),
            Self::Windowed { .. } => ("windowed", "Windowed"),
        }
    }
}

/// The settings the overlay's w saves (flagged `save` in lib/settings.sh),
/// in that file's order, as the static names `now` holds. Each has a live
/// value, DELAY only while the loopback runs: a flagged name with none is a
/// defect here, and it fails loudly instead of never being saved.
fn saveable(rows: &[Row], now: &overlay::Values, loopback: bool) -> Vec<&'static str> {
    rows.iter()
        .filter(|r| r.has_flag("save"))
        .filter_map(|r| match now.keys().find(|k| **k == r.name) {
            Some(k) => Some(*k),
            None if r.name == "DELAY" && !loopback => None,
            None => panic!("{} is flagged save in lib/settings.sh but has no live value in App::values", r.name),
        })
        .collect()
}

/// The overlay's text for one frame (osd.rs draws it).
struct SceneText {
    page: Option<Vec<String>>,
    kind: PageKind,
    key_bar: Option<String>,
    no_signal: Option<String>,
    message: Option<String>,
}

/// What reaches the window's loop from other threads, with the number of
/// the window it is for: winit has one event loop per process, so an event
/// sent as one window ended reaches the next, which ignores it.
struct UserEvent {
    window: u64,
    what: What,
}

enum What {
    /// From the window run (run.rs).
    Run(Wake),
    /// A thread has something to show (`Notify`).
    Notice,
}

/// Numbers each window opens, for `UserEvent`.
// process-global: window numbers across the runs of one process
static WINDOWS: AtomicU64 = AtomicU64::new(0);

struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    convert_pipeline: wgpu::RenderPipeline,
    convert_bind_group: wgpu::BindGroup,
    picture_buffer: wgpu::Buffer,
    /// The converted frame at the capture's size (what s saves).
    rgb: wgpu::Texture,
    rgb_view: wgpu::TextureView,
    screenshot: Screenshot,
    scale_pipeline: wgpu::RenderPipeline,
    scale_bind_group: wgpu::BindGroup,
    filters: FilterPasses,
    /// The scaler in use: None is the bilinear sampler.
    filter: Option<Filter>,
    /// Where the frame lands on the surface: x, y, width, height.
    viewport: [u32; 4],
    /// --stats, where the GPU can time its passes.
    timer: Option<GpuTimer>,
    osd: Osd,
    /// A card frame has been uploaded (before that the YUYV texture is all
    /// zeros, which converts to green, not black).
    have_frame: bool,
    /// The window has no area (minimised, or resized to nothing): nothing
    /// is drawn until it has.
    hidden: bool,
    texture: wgpu::Texture,
    upload: Vec<u8>,
    /// None: the desktop gives no presentation feedback; frames go out
    /// unpaced on Mailbox. Borrows winit's display: dropped in `exiting`.
    feedback: Option<Box<dyn Feedback>>,
}

struct App {
    /// This window's number (`WINDOWS`).
    window_no: u64,
    /// A frame wake is posted and not yet taken (`Winit::open`).
    frame_wake: Arc<AtomicBool>,
    opts: Options,
    format: Format,
    slot: Arc<Mutex<Slot>>,
    gfx: Option<Gfx>,
    picture: Picture,
    scaling: Scaling,
    overlay: Overlay,
    /// The saveable settings at start or at the last save: what w compares.
    baseline: overlay::Values,
    /// Messages from other threads, taken on `UserEvent::Notice`.
    notices: Arc<Mutex<Vec<(String, Duration)>>>,
    notify: Notify,
    /// Draw again without a new frame (the overlay changed).
    repaint: bool,
    /// A card frame came on this wake (`about_to_wait` sends it).
    arrived: bool,
    /// The card was pulled out (#40): with the first picture after, the
    /// loopback that died with its audio input starts again.
    sound_with_card: bool,
    /// When d last started a dashboard (`DASHBOARD_MAPS`).
    dashboard_opened: Option<Instant>,
    /// LEVELS or a colour value changed: convert the last frame again on a
    /// repaint, so a frozen feed shows it too.
    reconvert: bool,
    /// When the overlay next changes on its own.
    deadline: Option<Instant>,
    /// The window placement worker, once started.
    placer: Option<std::sync::mpsc::Sender<Placement>>,
    modifiers: ModifiersState,
    stats: Stats,
    in_flight: u32,
    last_submit: Duration,
    /// The run's end (run.rs): set here when the window closes or fails.
    end: End,
}

// One triangle covering the viewport.
const VERTEX: &str = r#"
struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
};

@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VOut;
    o.pos = vec4f(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2f(p.x, 1.0 - p.y);
    return o;
}
"#;

/// Pass 1, at the capture's own size: YUYV -> RGB. A texel is one
/// macropixel, Y0 U Y1 V for two pixels; chroma is sited with the even pixel
/// and interpolated for the odd one. Then LEVELS and the colour values
/// (picture.rs).
fn convert_shader(width: u32) -> String {
    let last = width / 2 - 1;
    format!(
        r#"{wgsl}
@group(0) @binding(0) var yuyv: texture_2d<f32>;
@group(0) @binding(1) var<uniform> pic: Picture;

@fragment fn fs(in: VOut) -> @location(0) vec4f {{
    let x = u32(in.pos.x);
    let y = u32(in.pos.y);
    let t = textureLoad(yuyv, vec2u(x / 2u, y), 0);
    var luma = t.r;
    var c = t.ga;
    if ((x & 1u) == 1u) {{
        luma = t.b;
        let next = textureLoad(yuyv, vec2u(min(x / 2u + 1u, {last}u), y), 0);
        c = (t.ga + next.ga) * 0.5;
    }}
    return vec4f(picture(pic, vec3f(luma, c.x, c.y)), 1.0);
}}
"#,
        wgsl = picture::WGSL
    )
}

/// Pass 2: the RGB frame scaled into the letterboxed viewport, with the
/// bilinear sampler (SCALING=fast, and 1:1 where it is the frame itself) or
/// with a filter (scale.rs).
const SCALE: &str = r#"
@group(0) @binding(0) var rgb: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    return textureSample(rgb, smp, in.uv);
}
"#;

/// The frame at the largest size that fits the surface, centred, in whole
/// pixels as mpv places it: x, y, width, height.
fn letterbox(surface: (u32, u32), frame: (u32, u32)) -> [u32; 4] {
    let (sw, sh) = (f64::from(surface.0), f64::from(surface.1));
    let (w, h) = (f64::from(frame.0), f64::from(frame.1));
    let scale = (sw / w).min(sh / h);
    let (vw, vh) = ((w * scale).round().clamp(1.0, sw), (h * scale).round().clamp(1.0, sh));
    [((sw - vw) / 2.0).round() as u32, ((sh - vh) / 2.0).round() as u32, vw as u32, vh as u32]
}

/// The graphics backends offered to wgpu. DX12 on Windows: with Vulkan
/// also offered, wgpu took Intel's Vulkan driver on a UHD 620, which
/// crashed the viewer at its first frame (#19).
pub(crate) const BACKENDS: wgpu::Backends = if cfg!(windows) { wgpu::Backends::DX12 } else { wgpu::Backends::PRIMARY };

pub(crate) fn pipeline(
    device: &wgpu::Device,
    label: &str,
    fragment: &str,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(format!("{VERTEX}{fragment}").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

pub(crate) fn texture_entry(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

pub(crate) fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

// process-global: winit allows one event loop per process
thread_local! {
    /// The event loop between windows: one per process (winit cannot build
    /// a second on Linux), run once per window, as the wizard opens one
    /// window for choosing the card and another for its feed (#13).
    static EVENT_LOOP: RefCell<Option<EventLoop<UserEvent>>> = const { RefCell::new(None) };
}

/// Whether a wake is posted: a frame wake only when none waits (`pending`,
/// cleared as the window takes it); the others always.
fn posts(pending: &AtomicBool, w: &Wake) -> bool {
    !matches!(w, Wake::Frame) || !pending.swap(true, Ordering::AcqRel)
}

/// One window over `source` (run.rs), on winit: until it ends, and why.
/// With `find`, a card pulled out is waited for (#40).
pub fn run(source: Box<dyn FrameSource>, find: Option<Box<dyn crate::capture::Finder>>, opts: Options, quit: &Quit) -> Outcome {
    let plan = Plan {
        seconds: opts.seconds.map(Duration::from_secs),
        live: opts.live.clone(),
        // The wizard's page says what its feed does (its signal check).
        log_signal: opts.wizard.is_none(),
        stats: opts.stats,
        find,
    };
    let mut host = Winit { opts: Some(opts), event_loop: None, window_no: 0, frame_wake: Arc::default() };
    crate::run::run(&mut host, source, plan, quit)
}

/// The window run's host on winit (the host seam, run.rs): the window,
/// its rendering, keys and overlay, and the screenshot finished before it
/// closes. Holds the process's event loop from `open` until it is dropped.
struct Winit {
    /// Taken by `show`.
    opts: Option<Options>,
    event_loop: Option<EventLoop<UserEvent>>,
    window_no: u64,
    frame_wake: Arc<AtomicBool>,
}

impl Host for Winit {
    fn open(&mut self) -> Result<Waker, String> {
        let event_loop = match EVENT_LOOP.with(|l| l.borrow_mut().take()) {
            Some(l) => l,
            None => EventLoop::<UserEvent>::with_user_event().build().map_err(|e| format!("event loop: {e}"))?,
        };
        let proxy = event_loop.create_proxy();
        self.event_loop = Some(event_loop);
        let window = WINDOWS.fetch_add(1, Ordering::Relaxed);
        self.window_no = window;
        // At most one frame wake waits: the slot holds only the newest
        // frame. A wake per frame, while each draw took longer than a frame
        // (#18, WARP), queued thousands of them.
        let pending = self.frame_wake.clone();
        Ok(Arc::new(move |w| {
            if posts(&pending, &w) {
                let _ = proxy.send_event(UserEvent { window, what: What::Run(w) });
            }
        }))
    }

    fn show(&mut self, format: Format, slot: &Arc<Mutex<Slot>>, end: &End) {
        let (Some(event_loop), Some(opts)) = (self.event_loop.as_mut(), self.opts.take()) else {
            end.set_or_warn(Outcome::Failed("the window was shown without being opened".into()));
            return;
        };
        let window_no = self.window_no;
        let notices: Arc<Mutex<Vec<(String, Duration)>>> = Arc::default();
        let notify: Notify = {
            let (n, p) = (notices.clone(), event_loop.create_proxy());
            Arc::new(move |message| {
                if let Some(m) = message {
                    n.lock().expect("the notices' lock is never poisoned").push(m);
                }
                let _ = p.send_event(UserEvent { window: window_no, what: What::Notice });
            })
        };
        if let AudioSetup::Loopback { now: Ok(c), .. } = &opts.audio {
            c.set_notify(notify.clone());
        }
        let start = now_monotonic();
        let mut app = App {
            window_no,
            frame_wake: self.frame_wake.clone(),
            format,
            stats: Stats::new(start, opts.stats, opts.live.clone()),
            picture: opts.picture,
            scaling: opts.scaling,
            overlay: Overlay::new(&opts.source, Instant::now()),
            baseline: overlay::Values::new(),
            notices,
            notify,
            repaint: false,
            arrived: false,
            sound_with_card: false,
            dashboard_opened: None,
            reconvert: false,
            deadline: None,
            placer: None,
            modifiers: ModifiersState::empty(),
            opts,
            slot: slot.clone(),
            gfx: None,
            in_flight: 0,
            last_submit: start,
            end: end.clone(),
        };
        app.baseline = app.values();
        // Ended before it began (a signal as the run started): no window.
        if !end.is_set()
            && let Err(e) = event_loop.run_app_on_demand(&mut app)
        {
            end.set_or_warn(Outcome::Failed(format!("event loop: {e}")));
        }
        if app.opts.stats {
            let superseded = slot.lock().expect("the slot's lock is never poisoned").superseded;
            app.stats.summary(superseded);
        }
    }
}

impl Drop for Winit {
    /// The event loop back for the next window.
    fn drop(&mut self) {
        if let Some(l) = self.event_loop.take() {
            EVENT_LOOP.with(|cell| *cell.borrow_mut() = Some(l));
        }
    }
}

/// The presentation feedback for the window: Wayland's (present.rs), so
/// presents are paced and timed by the compositor; None sends frames out
/// unpaced.
#[cfg(target_os = "linux")]
fn feedback(window: &Window) -> Result<Option<Box<dyn Feedback>>, String> {
    let feedback: Option<Box<dyn Feedback>> = match (
        window.display_handle().map_err(|e| e.to_string())?.as_raw(),
        window.window_handle().map_err(|e| e.to_string())?.as_raw(),
    ) {
        (RawDisplayHandle::Wayland(dh), RawWindowHandle::Wayland(wh)) => {
            // Safety: winit's display and the window's surface live until
            // `exiting`, where `Gfx` (and this) is dropped first.
            match unsafe { Presentation::new(dh.display.as_ptr(), wh.surface.as_ptr()) } {
                Ok(p) if p.clock_id() == Some(libc::CLOCK_MONOTONIC as u32) => Some(Box::new(p)),
                Ok(_) => {
                    eprintln!("note: the presentation clock is not CLOCK_MONOTONIC; frames go out unpaced");
                    None
                }
                Err(e) => {
                    eprintln!("note: {e}; frames go out unpaced");
                    None
                }
            }
        }
        _ => {
            eprintln!("note: not on Wayland; frames go out unpaced");
            None
        }
    };
    Ok(feedback)
}

/// On Windows the feedback needs the configured surface's swapchain:
/// `frame_statistics`, after the first `configure`.
#[cfg(not(target_os = "linux"))]
fn feedback(_: &Window) -> Result<Option<Box<dyn Feedback>>, String> {
    Ok(None)
}

/// DXGI frame statistics on the configured surface (present.rs, #19):
/// presents timed, paced by DX12's waitable swapchain as before.
#[cfg(windows)]
fn frame_statistics(surface: &wgpu::Surface) -> Option<Box<dyn Feedback>> {
    match present::FrameStatistics::new(surface) {
        Some(f) => Some(Box::new(f)),
        None => {
            eprintln!("note: no DXGI swapchain to time presents by; frames are paced by the swapchain only");
            None
        }
    }
}

/// The filter scalers' passes (scale.rs): a polar filter's one, or an
/// orthogonal filter's two, across the rows into an intermediate texture
/// and then down the columns into the viewport.
struct FilterPasses {
    /// Into the viewport: the polar pass from the frame, or an orthogonal
    /// filter's second pass from the intermediate texture.
    into_viewport: wgpu::RenderPipeline,
    /// An orthogonal filter's first pass, into the intermediate texture.
    across: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// The frame and `buffer`: the polar pass, or the first pass.
    bind_group: wgpu::BindGroup,
    buffer: wgpu::Buffer,
    down_buffer: wgpu::Buffer,
    /// An orthogonal filter's intermediate texture and its second pass's
    /// bind group, made for the viewport's width in `relayout`.
    down: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<Gfx, String> {
        let (w, h) = (self.format.width, self.format.height);
        let mut attrs = Window::default_attributes().with_title("protoHDMIcapture");
        #[cfg(target_os = "linux")]
        {
            attrs = attrs.with_name("protohdmicapture", "protohdmicapture");
        }
        // The exe's icon resource (build.rs, #35): winit's window class has
        // none, so the title bar and the taskbar are given it. On Wayland the
        // icon comes from the .desktop file, through the app id above.
        #[cfg(windows)]
        {
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            let icon = |px| winit::window::Icon::from_resource(1, Some(PhysicalSize::new(px, px)));
            match (icon(32), icon(256)) {
                (Ok(small), Ok(big)) => attrs = attrs.with_window_icon(Some(small)).with_taskbar_icon(Some(big)),
                (Err(e), _) | (_, Err(e)) => eprintln!("warning: the window's icon: {e}"),
            }
        }
        attrs = match self.opts.window {
            WindowMode::Fullscreen => {
                let monitor = match &self.opts.monitor {
                    None => None,
                    Some(name) => Some(
                        event_loop
                            .available_monitors()
                            .find(|m| m.name().as_deref() == Some(name.as_str()))
                            .ok_or_else(|| format!("no monitor named {name}"))?,
                    ),
                };
                attrs.with_fullscreen(Some(Fullscreen::Borderless(monitor)))
            }
            WindowMode::Borderless => attrs.with_decorations(false).with_maximized(true),
            WindowMode::Windowed { width, height } => attrs.with_inner_size(PhysicalSize::new(width, height)),
        };
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| format!("window: {e}"))?);

        let feedback = feedback(&window)?;

        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = BACKENDS;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone()).map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("graphics adapter: {e}"))?;
        // Pass times only when measuring, and only where the GPU has them.
        let timestamps = wgpu::Features::TIMESTAMP_QUERY;
        let required_features = if self.opts.stats && adapter.features().contains(timestamps) {
            timestamps
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() }))
                .map_err(|e| format!("graphics device: {e}"))?;
        let timer = if self.opts.stats {
            let t = GpuTimer::new(&device, &queue);
            if t.is_none() {
                eprintln!("note: the GPU cannot time its passes; no GPU times");
            }
            t
        } else {
            None
        };

        let caps = surface.get_capabilities(&adapter);
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the window's surface is not supported by the graphics adapter")?;
        // Not sRGB: the shader's output is already gamma-encoded video.
        config.format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
            .ok_or("no non-sRGB surface format")?;
        // Mailbox: the newest frame replaces one still waiting (paced, the
        // best #5 measured; unpaced, the only way without a queue).
        config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        // wgpu-hal keeps the swapchain across later configures (it resizes
        // its buffers), so it is taken once, here.
        #[cfg(windows)]
        let feedback = feedback.or_else(|| frame_statistics(&surface));
        if self.opts.stats {
            let info = adapter.get_info();
            eprintln!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
            eprintln!(
                "surface: {}x{} {:?}, present mode {:?}, paced {}",
                config.width,
                config.height,
                config.format,
                config.present_mode,
                feedback.as_ref().is_some_and(|f| f.paces())
            );
        }

        let frame_texture = |label, width, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        let texture = frame_texture("yuyv", w / 2, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let rgb = frame_texture(
            "rgb",
            w,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let rgb_view = rgb.create_view(&Default::default());

        let convert_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("convert"),
            entries: &[
                texture_entry(0, false),
                uniform_entry(1),
            ],
        });
        let picture_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("picture"),
            size: size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&picture_buffer, 0, &picture::bytes(&self.picture.uniform(w, h, self.format.coding())));
        let convert_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("convert"),
            layout: &convert_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture.create_view(&Default::default())),
                },
                wgpu::BindGroupEntry { binding: 1, resource: picture_buffer.as_entire_binding() },
            ],
        });
        let convert_pipeline =
            pipeline(&device, "convert", &convert_shader(w), &convert_layout, wgpu::TextureFormat::Rgba8Unorm);

        let scale_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scale"),
            entries: &[
                texture_entry(0, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scale"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let scale_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scale"),
            layout: &scale_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&rgb_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let scale_pipeline = pipeline(&device, "scale", SCALE, &scale_layout, config.format);

        let filter_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("filter"),
            entries: &[
                texture_entry(0, false),
                uniform_entry(1),
            ],
        });
        let uniform = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: scale::UNIFORM_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let (filter_buffer, filter_y_buffer) = (uniform("filter"), uniform("filter y"));
        let filter_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("filter"),
            layout: &filter_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&rgb_view) },
                wgpu::BindGroupEntry { binding: 1, resource: filter_buffer.as_entire_binding() },
            ],
        });
        let filter_pipeline = pipeline(&device, "filter", scale::WGSL, &filter_layout, config.format);
        let filter_x_pipeline = pipeline(&device, "filter x", scale::WGSL, &filter_layout, scale::INTERMEDIATE);

        let screenshot = Screenshot::new(&device, w, h);
        let osd = Osd::new(&device, config.format);
        Ok(Gfx {
            window,
            surface,
            device,
            queue,
            config,
            convert_pipeline,
            convert_bind_group,
            picture_buffer,
            screenshot,
            rgb,
            rgb_view,
            scale_pipeline,
            scale_bind_group,
            filters: FilterPasses {
                into_viewport: filter_pipeline,
                across: filter_x_pipeline,
                layout: filter_layout,
                bind_group: filter_bind_group,
                buffer: filter_buffer,
                down_buffer: filter_y_buffer,
                down: None,
            },
            filter: None,
            viewport: [0, 0, 1, 1],
            timer,
            osd,
            have_frame: false,
            hidden: false,
            texture,
            upload: vec![0; self.format.frame_size()],
            feedback,
        })
    }

    /// The viewport and the scaler for the surface's size and SCALING; after
    /// a resize and on c.
    fn relayout(&mut self) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        let frame = (self.format.width, self.format.height);
        gfx.viewport = letterbox((gfx.config.width, gfx.config.height), frame);
        let [_, _, vw, vh] = gfx.viewport;
        gfx.filter = scale::filter(self.scaling, frame, (vw, vh));
        gfx.filters.down = None;
        if let Some(f) = &gfx.filter {
            gfx.queue.write_buffer(&gfx.filters.buffer, 0, &f.bytes(frame, Axis::X));
            if !f.polar {
                let between = (vw, frame.1);
                let view = gfx
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("filter x"),
                        size: wgpu::Extent3d { width: between.0, height: between.1, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: scale::INTERMEDIATE,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    })
                    .create_view(&Default::default());
                gfx.queue.write_buffer(&gfx.filters.down_buffer, 0, &f.bytes(between, Axis::Y));
                let bind = gfx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("filter y"),
                    layout: &gfx.filters.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                        wgpu::BindGroupEntry { binding: 1, resource: gfx.filters.down_buffer.as_entire_binding() },
                    ],
                });
                gfx.filters.down = Some((view, bind));
            }
        }
        if self.opts.stats {
            eprintln!(
                "scaling: {} -> {vw}x{vh}, {}",
                self.scaling.as_str(),
                gfx.filter.as_ref().map_or("bilinear sampler", |f| f.name)
            );
        }
    }

    /// A colour key (mpv's 1..8): the next frame shows the change.
    fn adjust_colour(&mut self, colour: Colour, by: i32) {
        self.picture.adjust(colour, by);
        let p = self.picture;
        // mpv's message for its colour keys.
        let (name, v) = match colour {
            Colour::Contrast => ("Contrast", p.contrast),
            Colour::Brightness => ("Brightness", p.brightness),
            Colour::Gamma => ("Gamma", p.gamma),
            Colour::Saturation => ("Saturation", p.saturation),
        };
        self.overlay.message(format!("{name}: {v}"), Duration::from_millis(1500), Instant::now());
        self.picture_changed();
    }

    fn picture_changed(&mut self) {
        if let Some(gfx) = &self.gfx {
            let u = self.picture.uniform(self.format.width, self.format.height, self.format.coding());
            gfx.queue.write_buffer(&gfx.picture_buffer, 0, &picture::bytes(&u));
        }
        self.repaint = true;
        self.reconvert = true;
    }

    /// The window closes: q, the close button, or the wizard moving on.
    fn close(&self, event_loop: &ActiveEventLoop) {
        self.end.set(Outcome::Closed);
        event_loop.exit();
    }

    /// The window cannot go on.
    fn fail(&self, event_loop: &ActiveEventLoop, e: String) {
        self.end.set_or_warn(Outcome::Failed(e));
        event_loop.exit();
    }

    /// What the keys see of this window (keys.rs).
    fn key_mode(&self) -> keys::Mode {
        if self.opts.wizard.is_some() { keys::Mode::Wizard } else { keys::Mode::Viewer(self.overlay.page) }
    }

    /// A key's action (keys.rs), applied. True when the window closes.
    fn apply(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::Wizard(k) => {
                let Some(w) = &self.opts.wizard else { return false };
                w.borrow_mut().key(&k);
                if w.borrow().wants_exit() {
                    return true;
                }
            }
            Action::ToggleOverlay => self.overlay.toggle(),
            Action::CloseOverlay => self.overlay.page = None,
            Action::CycleTips(by) => self.overlay.cycle_source(by),
            Action::CycleLevels => {
                self.picture.cycle_levels();
                self.picture_changed();
            }
            Action::CycleScaling => {
                self.scaling = self.scaling.next();
                self.relayout();
            }
            Action::ResetColour => {
                self.picture.reset_colour();
                self.picture_changed();
            }
            Action::Save => self.save(),
            Action::ToggleTips => self.overlay.toggle_tips(),
            Action::CycleDisplay => self.cycle_display(),
            Action::Dashboard => self.open_dashboard(),
            // Without frames from the card, the repaint below takes the shot.
            Action::Screenshot => {
                if let Some(gfx) = self.gfx.as_mut()
                    && !gfx.screenshot.request()
                {
                    eprintln!("screenshot: the last one is still being saved");
                }
            }
            // The audio keys act only with AUDIO=loopback; with the
            // loopback not running, they start it again (#29).
            Action::Sync(by) => {
                let AudioSetup::Loopback { keys, now } = &self.opts.audio else { return false };
                let ms = match now {
                    Ok(c) if c.dead().is_none() => c.nudge_delay(by),
                    _ => {
                        let ms = crate::audio::nudged(keys.delay_ms(), by);
                        if self.restart_audio(ms).is_none() {
                            self.repaint = true;
                            return false;
                        }
                        ms
                    }
                };
                self.overlay.message(format!("Audio sync {ms} ms"), Duration::from_millis(1500), Instant::now());
            }
            // Its message comes from the controls' thread (Notify).
            Action::Level(change) => {
                let AudioSetup::Loopback { keys, now } = &self.opts.audio else { return false };
                let controls = match now {
                    Ok(c) if c.dead().is_none() => c,
                    _ => {
                        let delay_ms = keys.delay_ms();
                        let Some(c) = self.restart_audio(delay_ms) else {
                            self.repaint = true;
                            return false;
                        };
                        c
                    }
                };
                controls.change_level(change);
            }
            Action::Picture(colour, by) => self.adjust_colour(colour, by),
        }
        self.repaint = true;
        false
    }

    /// A new loopback at `delay_ms`, in place of one that is not running
    /// (#29), said in the overlay as view.sh's overlay says it; its
    /// controls, or None when it did not start.
    fn restart_audio(&mut self, delay_ms: u32) -> Option<&Controls> {
        let AudioSetup::Loopback { keys, now } = &mut self.opts.audio else { return None };
        let started = audio_started(&self.opts.live, keys, keys.start(delay_ms));
        let message = match &started {
            Ok(c) => {
                c.set_notify(self.notify.clone());
                self.opts.live.event(format!("audio back, sync {delay_ms} ms"));
                ("Audio back".to_string(), Duration::from_millis(1500))
            }
            Err(e) => {
                self.opts.live.event(format!("audio did not start again: {e}"));
                (format!("Audio: {e}"), Duration::from_secs(3))
            }
        };
        *now = started;
        self.overlay.message(message.0, message.1, Instant::now());
        match &self.opts.audio {
            AudioSetup::Loopback { now: Ok(c), .. } => Some(c),
            _ => None,
        }
    }

    /// The frame policy's signal state changed: the wizard's check, the
    /// banner, and the sound back with the card after a pull (#40).
    fn signal(&mut self, s: Signal) {
        if let Some(w) = &self.opts.wizard {
            w.borrow_mut().signal(&s);
        }
        let picture = s == Signal::Picture;
        if matches!(s, Signal::NoSignal { reason: Reason::Unplugged(_), .. }) {
            self.sound_with_card = true;
        }
        // Back after no signal: the refreshes without frames are no repeats.
        if picture && matches!(self.overlay.signal, Signal::NoSignal { .. }) {
            self.stats.picture_back();
        }
        self.overlay.signal = s;
        if picture && std::mem::take(&mut self.sound_with_card) {
            self.sound_back();
        }
        self.repaint = true;
    }

    /// The card back after it was pulled out (#40): its loopback, which
    /// stopped with the card's audio input, starts again as the keys
    /// would start it (#29). One still running is left alone.
    fn sound_back(&mut self) {
        // A death not yet taken from its notice counts as stopped.
        self.audio_check();
        let AudioSetup::Loopback { keys, now: Err(_) } = &self.opts.audio else { return };
        let delay_ms = keys.delay_ms();
        self.restart_audio(delay_ms);
    }

    /// A loopback that stopped mid-feed (its controls say so, and wake the
    /// window): no longer running, and said in the dashboard and as a
    /// notification; the overlay's message and the terminal's came from
    /// the audio's thread (audio.rs `died`).
    fn audio_check(&mut self) {
        let AudioSetup::Loopback { now, .. } = &mut self.opts.audio else { return };
        let Some(why) = now.as_ref().ok().and_then(Controls::dead) else { return };
        // The terminal has it already, from the audio's own thread.
        let said = format!("the audio loopback stopped mid-feed ({why})");
        no_sound(said.clone());
        self.opts.live.event(said.clone());
        self.opts.live.set_audio(crate::dashboard::AudioLive::Failed(said));
        *now = Err(why);
    }

    /// The loopback runs: its controls.
    fn loopback(&self) -> Option<&Controls> {
        match &self.opts.audio {
            AudioSetup::Loopback { now: Ok(c), .. } => Some(c),
            _ => None,
        }
    }

    /// d: the dashboard in a terminal of its own (#16).
    fn open_dashboard(&mut self) {
        let now = Instant::now();
        // One dashboard: d again raises it, or waits for the one opening
        // (not mapped yet, so not listed).
        if self.dashboard_opened.is_some_and(|t| now.duration_since(t) < DASHBOARD_MAPS) {
            self.overlay.message("The debug dashboard is opening", Duration::from_millis(1500), now);
            self.repaint = true;
            return;
        }
        let before = match self.opts.desktop.dashboards() {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("warning: dashboard: cannot list the windows ({e}); it opens where the desktop puts it");
                None
            }
        };
        if let Some(open) = before.as_ref().and_then(|b| b.first())
            && self.opts.desktop.focus_window(open)
        {
            self.overlay.message("The debug dashboard is open", Duration::from_millis(1500), now);
            self.repaint = true;
            return;
        }
        match crate::dashboard::open(Some(self.notify.clone())) {
            Ok(()) => {
                self.overlay.message("Opening the debug dashboard", Duration::from_millis(1500), now);
                self.dashboard_opened = Some(now);
                if before.is_some() {
                    self.place_window_with(before);
                }
            }
            Err(e) => {
                eprintln!("warning: {e}");
                self.overlay.message(format!("Dashboard: {e}"), Duration::from_secs(3), now);
            }
        }
        self.repaint = true;
    }

    /// f: the next display mode, through the toolkit and then the desktop
    /// adapter, as the mpv script sets mpv's properties and then runs
    /// `view.sh --place-window`.
    fn cycle_display(&mut self) {
        let mode = self.opts.window.next(self.opts.window_size);
        self.opts.window = mode;
        if let Some(gfx) = &self.gfx {
            let w = &gfx.window;
            match mode {
                WindowMode::Fullscreen => w.set_fullscreen(Some(Fullscreen::Borderless(None))),
                WindowMode::Borderless => {
                    w.set_fullscreen(None);
                    w.set_decorations(false);
                    w.set_maximized(true);
                }
                WindowMode::Windowed { width, height } => {
                    w.set_fullscreen(None);
                    w.set_maximized(false);
                    w.set_decorations(true);
                    let _ = w.request_inner_size(PhysicalSize::new(width, height));
                }
            }
        }
        self.place_window();
        // The panel shows it; with the panel closed, a message does.
        if self.overlay.page.is_none() {
            self.overlay.message(format!("Display: {}", mode.names().1), Duration::from_millis(1500), Instant::now());
        }
        self.repaint = true;
    }

    /// The desktop adapter's part of the current mode, off this thread (it
    /// waits for the window to map, then runs a few hyprctl calls). One
    /// worker does them in turn and skips to the latest, so quick f
    /// presses cannot interleave their dispatches.
    fn place_window(&mut self) {
        let dashboard = self.opts.dashboard.take();
        self.place_window_with(dashboard);
    }

    /// `place_window`, moving a newly started dashboard first (`d`,
    /// DEBUG=window): the same worker, so an f pressed while it maps is
    /// applied after it, not undone by it.
    fn place_window_with(&mut self, dashboard: Option<Vec<String>>) {
        if matches!(self.opts.desktop, Desktop::Generic) {
            return;
        }
        let placer = self.placer.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Placement>();
            let desktop = self.opts.desktop.clone();
            let say = |r: crate::desktop::Report| match r {
                Ok(m) => eprintln!("{m}"),
                Err(m) => eprintln!("warning: {m}"),
            };
            thread::spawn(move || {
                let pid = std::process::id();
                // Dashboards moved already: never "new" again.
                let mut moved: Vec<String> = Vec::new();
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = job.then(newer);
                    }
                    if let Some(before) = job.dashboard.take() {
                        let skip: Vec<String> = before.into_iter().chain(moved.iter().cloned()).collect();
                        match desktop.move_dashboard(pid, &skip) {
                            Ok((address, said)) => {
                                eprintln!("{said}");
                                moved.push(address);
                            }
                            Err(m) => eprintln!("warning: {m}"),
                        }
                        // f pressed while it mapped: the latest mode.
                        while let Ok(newer) = rx.try_recv() {
                            job = job.then(newer);
                        }
                    }
                    say(desktop.place_window(pid, job.mode, job.size));
                    if let Some(m) = job.refocus.take() {
                        desktop.focus_monitor(&m);
                    }
                }
            });
            tx
        });
        let refocus = self.opts.refocus.take();
        let _ = placer.send(Placement { mode: self.opts.window.names().0, size: self.opts.window_size, dashboard, refocus });
    }

    /// The saveable settings' live values, spelled as the settings file
    /// spells them (DELAY in seconds, as the mpv script writes it).
    fn values(&self) -> overlay::Values {
        self.values_with(self.loopback().map(Controls::delay_ms))
    }

    /// `values`, with the loopback's delay in ms while it runs.
    fn values_with(&self, delay_ms: Option<u32>) -> overlay::Values {
        let p = &self.picture;
        let mut v: overlay::Values = [
            ("WINDOW", self.opts.window.names().0.to_string()),
            ("LEVELS", p.levels.as_str().to_string()),
            ("SCALING", self.scaling.as_str().to_string()),
            ("BRIGHTNESS", p.brightness.to_string()),
            ("CONTRAST", p.contrast.to_string()),
            ("SATURATION", p.saturation.to_string()),
            ("GAMMA", p.gamma.to_string()),
        ]
        .into();
        if let Some(ms) = delay_ms {
            v.insert("DELAY", format!("{:.3}", f64::from(ms) / 1000.0));
        }
        v
    }

    fn saveable(&self) -> Vec<&'static str> {
        let Some(s) = &self.opts.settings else { return Vec::new() };
        saveable(&s.rows, &self.values(), self.loopback().is_some())
    }

    /// w: what changed since the start (or the last save), as `view.sh
    /// --save` writes it.
    fn save(&mut self) {
        let now = Instant::now();
        let values = self.values();
        let loopback = self.loopback().is_some();
        let changed = overlay::changes(&self.saveable(), &self.baseline, &values, loopback);
        if changed.is_empty() {
            self.overlay.message("Nothing changed since the feed started", Duration::from_secs(2), now);
            return;
        }
        let result = match &self.opts.settings {
            Some(s) => {
                let pairs: Vec<(&str, &str)> = changed.iter().filter_map(|kv| kv.split_once('=')).collect();
                s.save(&pairs)
            }
            None => Err("no settings to save to".to_string()),
        };
        match result {
            Ok(()) => {
                self.baseline = values;
                self.overlay.message(format!("Saved: {}", changed.join(" ")), Duration::from_secs(3), now);
            }
            Err(e) => {
                eprintln!("save failed: {e}");
                self.overlay.message(format!("Save failed: {e}"), Duration::from_secs(5), now);
            }
        }
    }

    /// The overlay's text for this frame, or None when nothing shows.
    fn scene_text(&self, now: Instant) -> Option<SceneText> {
        // The wizard's page instead of the overlay: no key bar, and no
        // banner (its signal check says what the feed does).
        if let Some(w) = &self.opts.wizard {
            return Some(SceneText {
                page: Some(w.borrow().lines()),
                kind: PageKind::Wizard,
                key_bar: None,
                no_signal: None,
                message: self.overlay.current_message(now).map(str::to_string),
            });
        }
        if !self.overlay.visible(now) {
            return None;
        }
        let loopback = self.loopback().is_some();
        let page = match self.overlay.page {
            None => None,
            Some(Page::Tips) => {
                let id = &self.overlay.tips_id;
                Some(overlay::tips_lines(id, sources::get(id).as_ref()))
            }
            Some(Page::Panel) => {
                let values = self.values();
                let changes = overlay::changes(&self.saveable(), &self.baseline, &values, loopback);
                let id = &self.overlay.tips_id;
                let p = &self.picture;
                Some(overlay::panel_lines(&overlay::Panel {
                    display: self.opts.window.names().1,
                    levels: p.levels.as_str(),
                    scaling: self.scaling.as_str(),
                    colour: [p.contrast, p.brightness, p.gamma, p.saturation],
                    audio: match &self.opts.audio {
                        AudioSetup::Off(v) => overlay::Audio::Other(v.clone()),
                        AudioSetup::Loopback { now: Err(why), .. } => overlay::Audio::Failed(why.clone()),
                        // Dead, before its wake is handled.
                        AudioSetup::Loopback { now: Ok(a), .. } => match a.dead() {
                            Some(why) => overlay::Audio::Failed(why),
                            None => overlay::Audio::Running { level: a.current_level(), delay_ms: a.delay_ms() },
                        },
                    },
                    tips_name: sources::get(id).map_or_else(|| id.clone(), |s| s.name),
                    changes: &changes,
                }))
            }
        };
        Some(SceneText {
            kind: if matches!(self.overlay.page, Some(Page::Tips)) { PageKind::Tips } else { PageKind::Panel },
            page,
            key_bar: self.overlay.key_bar_showing(now).then(|| overlay::key_bar(loopback)),
            no_signal: self.overlay.no_signal(),
            message: self.overlay.current_message(now).map(str::to_string),
        })
    }

    /// Whether `about_to_wait` sends now: a frame came on this wake, the
    /// overlay changed, or a held frame is fresh (`HOLD`) or overdue (held
    /// for longer than one and a half frame times: the next one is late).
    /// One in between waits for the next card frame to replace it.
    fn send_now(&self) -> bool {
        let slot = self.slot.lock().expect("the slot's lock is never poisoned");
        let held = slot.tag.map(|t| now_monotonic().saturating_sub(t.dequeued));
        let overdue = Duration::from_secs_f64(1.5 / self.format.fps.max(1.0));
        self.arrived || self.repaint || held.is_some_and(|age| age < HOLD || age > overdue)
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        // Runs the readbacks' callbacks (screenshot, pass times).
        gfx.device.poll(wgpu::PollType::Poll).map_err(|e| format!("device poll: {e}"))?;
        if let Err(e) = gfx.screenshot.poll(&self.notify) {
            eprintln!("{e}");
        }
        if let Some(timer) = gfx.timer.as_mut()
            && let Some([convert, scale]) = timer.take()?
        {
            self.stats.gpu(convert, scale);
        }
        let Some(feedback) = gfx.feedback.as_mut() else { return Ok(()) };
        for o in feedback.drain()? {
            match o {
                present::Outcome::Presented { tag, at, refresh, flags } => self.stats.presented(tag, at, refresh, flags),
                present::Outcome::Discarded => self.stats.discarded(),
                present::Outcome::FrameDone => self.in_flight = self.in_flight.saturating_sub(1),
            }
        }
        Ok(())
    }

    /// Uploads the newest frame and presents it, now; paced, only once the
    /// compositor has taken the previous one (its frame callback). Meanwhile
    /// the slot keeps only the newest frame.
    fn render(&mut self) -> Result<(), String> {
        let paced = self.gfx.as_ref().and_then(|g| g.feedback.as_ref()).is_some_and(|f| f.paces());
        if paced && self.in_flight > 0 {
            if now_monotonic() - self.last_submit < PACE_TIMEOUT {
                return Ok(());
            }
            self.stats.pace_timeout();
            self.in_flight = 0;
        }
        if self.gfx.as_ref().is_none_or(|g| g.hidden) {
            return Ok(());
        }
        // Built before the frame is taken: it reads the app's state.
        let scene = self.scene_text(Instant::now());
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        let tag = {
            let mut s = self.slot.lock().expect("the slot's lock is never poisoned");
            match s.tag.take() {
                Some(tag) => {
                    std::mem::swap(&mut s.data, &mut gfx.upload);
                    tag
                }
                // Nothing new from the card, but the overlay changed: the
                // last frame (black before the first) drawn again.
                None if self.repaint => {
                    let now = now_monotonic();
                    Tag { captured: now, dequeued: now, repaint: true }
                }
                None => return Ok(()),
            }
        };
        let new_frame = !tag.repaint;

        let acquire_start = now_monotonic();
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.stats.not_acquired();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                self.stats.not_acquired();
                return Ok(());
            }
            other => return Err(format!("get_current_texture: {other:?}")),
        };
        self.stats.acquired(now_monotonic() - acquire_start);

        let (w, h) = (self.format.width, self.format.height);
        if new_frame {
            gfx.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &gfx.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &gfx.upload,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w / 2, height: h, depth_or_array_layers: 1 },
            );
            gfx.have_frame = true;
            // The wizard's signal check looks at the picture.
            if let Some(wiz) = &self.opts.wizard {
                wiz.borrow_mut().picture(&gfx.upload, self.format);
            }
        }
        // A new frame, or the last one again after a picture change.
        let convert = new_frame || (self.reconvert && gfx.have_frame);

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gfx.device.create_command_encoder(&Default::default());
        if let Some(t) = gfx.timer.as_mut()
            && new_frame
        {
            t.arm();
        }
        let target = |view| {
            [Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })]
        };
        if convert {
            let attachments = target(&gfx.rgb_view);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("convert"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: gfx.timer.as_ref().and_then(|t| t.writes(0)),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&gfx.convert_pipeline);
            pass.set_bind_group(0, &gfx.convert_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gfx.screenshot.copy(&mut encoder, &gfx.rgb);
        {
            // An orthogonal filter: its first pass, across the rows.
            let timestamps = match (&gfx.timer, &gfx.filters.down) {
                (Some(t), Some(_)) => t.writes_split(1),
                (Some(t), None) => [None, t.writes(1)],
                (None, _) => [None, None],
            };
            let [first, last] = timestamps;
            if let Some((between, _)) = &gfx.filters.down {
                let attachments = target(between);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("filter x"),
                    color_attachments: &attachments,
                    depth_stencil_attachment: None,
                    timestamp_writes: first,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&gfx.filters.across);
                pass.set_bind_group(0, &gfx.filters.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            let attachments = target(&view);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scale"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: last,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [x, y, vw, vh] = gfx.viewport.map(|v| v as f32);
            pass.set_viewport(x, y, vw, vh, 0.0, 1.0);
            if let Some((_, bind)) = &gfx.filters.down {
                pass.set_pipeline(&gfx.filters.into_viewport);
                pass.set_bind_group(0, bind, &[]);
            } else if gfx.filter.is_some() {
                pass.set_pipeline(&gfx.filters.into_viewport);
                pass.set_bind_group(0, &gfx.filters.bind_group, &[]);
            } else {
                pass.set_pipeline(&gfx.scale_pipeline);
                pass.set_bind_group(0, &gfx.scale_bind_group, &[]);
            }
            pass.draw(0..3, 0..1);
        }
        let mut extra = Vec::new();
        if let Some(text) = &scene {
            let lines = text.page.as_deref();
            let s = Scene {
                page: lines,
                kind: text.kind,
                key_bar: text.key_bar.as_deref(),
                no_signal: text.no_signal.as_deref(),
                message: text.message.as_deref(),
            };
            let size = (gfx.config.width, gfx.config.height);
            extra = gfx.osd.draw(&gfx.device, &gfx.queue, &mut encoder, &view, size, &s);
        }
        if let Some(t) = &gfx.timer {
            t.resolve(&mut encoder);
        }
        gfx.queue.submit(extra.into_iter().chain([encoder.finish()]));
        gfx.screenshot.submitted();
        if let Some(t) = gfx.timer.as_mut() {
            t.submitted();
        }

        // `present` commits the surface; feedback asked for first attaches
        // to that commit.
        if let Some(feedback) = gfx.feedback.as_mut() {
            feedback.request(tag)?;
        }
        gfx.window.pre_present_notify();
        gfx.queue.present(frame);
        self.in_flight += 1;
        self.last_submit = now_monotonic();
        self.repaint = false;
        if convert {
            self.reconvert = false;
        }
        if new_frame {
            self.stats.submitted();
        }
        self.drain()
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        match self.init(event_loop) {
            Ok(gfx) => {
                self.gfx = Some(gfx);
                self.relayout();
                // Drawn at once, card frame or not: on Wayland a window maps
                // with its first buffer, and the key bar shows from the start.
                self.repaint = true;
                // As view.sh's place job: the start mode, finished.
                self.place_window();
            }
            Err(e) => self.fail(event_loop, e),
        }
    }

    /// Marks what came; `about_to_wait` draws it. Not drawn here: winit
    /// calls `about_to_wait` once the queued events are taken, input
    /// included, while on Windows a wake is a posted message, which comes
    /// before any input. A draw here outlasting a frame (WARP: 0.9 s) left
    /// a wake always queued ahead of the keys (#18).
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        // An earlier window's, sent as it ended.
        if event.window != self.window_no {
            return;
        }
        let result = match event.what {
            What::Run(Wake::Frame) => {
                self.frame_wake.store(false, Ordering::Release);
                if let Some(w) = &self.opts.wizard {
                    w.borrow_mut().frame(Instant::now());
                }
                self.arrived = true;
                Ok(())
            }
            What::Run(Wake::Signal(s)) => {
                self.signal(s);
                Ok(())
            }
            What::Notice => {
                self.audio_check();
                let now = Instant::now();
                for (text, for_) in self.notices.lock().expect("the notices' lock is never poisoned").drain(..) {
                    self.overlay.message(text, for_, now);
                }
                self.repaint = true;
                Ok(())
            }
            What::Run(Wake::End) => {
                if self.end.is_set() {
                    event_loop.exit();
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            self.fail(event_loop, e);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // The wizard's close button ends the wizard, unsaved.
                if let Some(w) = &self.opts.wizard {
                    w.borrow_mut().abandon();
                }
                self.close(event_loop);
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(action) = keys::action(&event.logical_key, self.modifiers, event.repeat, self.key_mode())
                    && self.apply(action)
                {
                    self.close(event_loop);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(gfx) = self.gfx.as_mut() {
                    // A surface cannot be configured at zero size.
                    gfx.hidden = size.width == 0 || size.height == 0;
                    if !gfx.hidden {
                        gfx.config.width = size.width;
                        gfx.config.height = size.height;
                        gfx.surface.configure(&gfx.device, &gfx.config);
                        self.relayout();
                        // The new size drawn even without a card frame.
                        self.repaint = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Something shown changed on its own (a message went, the banner
        // came): draw again even without a frame.
        if self.deadline.is_some_and(|d| d <= Instant::now()) {
            self.repaint = true;
        }
        if let Some(w) = &self.opts.wizard
            && w.borrow_mut().tick(Instant::now())
        {
            self.repaint = true;
        }
        // Feedback may have freed the way for a frame that had to wait: one
        // that came on this wake, a fresh one, or one overdue (the card
        // stopped). Not any held frame on the compositor's frame callback:
        // the next card frame replaces a stale one. Sending every one there
        // kept each later frame a refresh behind (kernel -> presented 55 ms,
        // not 41; the wake comes here under WaitUntil, 2026-10-16).
        let send = self.send_now();
        self.arrived = false;
        if let Err(e) = self.drain().and_then(|()| if send { self.render() } else { Ok(()) }) {
            self.fail(event_loop, e);
        }
        // Wake for the next change, and while a repaint waits on pacing
        // (no frame from the card may come to wake us); not while the
        // window has no area, which nothing is drawn into.
        let now = Instant::now();
        self.deadline = self.overlay.next_change(now);
        if let Some(w) = self.opts.wizard.as_ref().and_then(|w| w.borrow().next_change(now)) {
            self.deadline = Some(self.deadline.map_or(w, |d| d.min(w)));
        }
        let hidden = self.gfx.as_ref().is_some_and(|g| g.hidden);
        // A card frame held back by pacing: if the card stops now, nothing
        // else would wake the loop for it.
        let held = self.slot.lock().expect("the slot's lock is never poisoned").tag.is_some();
        // A screenshot on its way back is taken by polling the device.
        let reading = self.gfx.as_ref().is_some_and(|g| g.screenshot.reading());
        let soon = if reading {
            Some(now + READBACK_POLL)
        } else if (self.repaint || held) && !hidden {
            Some(now + PACE_TIMEOUT)
        } else {
            None
        };
        let wake = match (self.deadline, soon) {
            (Some(d), Some(s)) => Some(d.min(s)),
            (d, s) => d.or(s),
        };
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        // The last feedback, then let go of winit's display while it lives.
        if let Err(e) = self.drain() {
            self.end.set_or_warn(Outcome::Failed(e));
        }
        // A screenshot still coming back is finished, not dropped.
        if let Some(gfx) = self.gfx.as_mut() {
            gfx.screenshot.finish(&gfx.device, &self.notify);
            // Before the surface: DXGI's feedback holds a reference to its
            // swapchain (present.rs), let go while wgpu's device still lives.
            gfx.feedback = None;
        }
        self.gfx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, NamedKey};

    use crate::level::LevelChange;

    /// An app with no window, its settings in a temp file.
    fn app_with(config: &str) -> (App, std::path::PathBuf) {
        // as_ptr: a &str's {:p} carries its length after a ':' (no file name on Windows).
        let dir = std::env::temp_dir().join(format!("phc-app-{}-{:p}", std::process::id(), config.as_ptr()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(&file, config).unwrap();
        let path = file.display().to_string();
        let s = Settings::resolve_with(&move |k| (k == "CONFIG_FILE").then(|| path.clone()));
        let picture =
            Picture::from_settings(s.get("LEVELS"), s.get("CONTRAST"), s.get("BRIGHTNESS"), s.get("GAMMA"), s.get("SATURATION"))
                .unwrap();
        let scaling = Scaling::from_settings(s.get("SCALING")).unwrap();
        let format = Format { width: 1920, height: 1080, fps: 60.0, pixels: crate::capture::Pixels::Yuyv };
        let mut app = App {
            window_no: 0,
            frame_wake: Arc::default(),
            format,
            slot: Arc::new(Mutex::new(Slot { data: Vec::new(), tag: None, superseded: 0 })),
            gfx: None,
            picture,
            scaling,
            overlay: Overlay::new("ps5", Instant::now()),
            baseline: overlay::Values::new(),
            notices: Arc::default(),
            notify: Arc::new(|_| {}),
            repaint: false,
            arrived: false,
            sound_with_card: false,
            dashboard_opened: None,
            reconvert: false,
            deadline: None,
            placer: None,
            modifiers: ModifiersState::empty(),
            stats: Stats::new(Duration::ZERO, false, Arc::new(crate::dashboard::Live::new())),
            in_flight: 0,
            last_submit: Duration::ZERO,
            end: End::new(Arc::new(|_| {})),
            opts: Options {
                window: WindowMode::Fullscreen,
                monitor: None,
                seconds: None,
                stats: false,
                picture,
                scaling,
                audio: AudioSetup::Off("none".into()),
                window_size: (1280, 720),
                desktop: Desktop::Generic,
                source: "ps5".into(),
                settings: Some(s),
                wizard: None,
                live: Arc::new(crate::dashboard::Live::new()),
                dashboard: None,
                refocus: None,
            },
        };
        app.baseline = app.values();
        (app, file)
    }

    /// A plain key pressed: true when it asked for something.
    fn press(app: &mut App, k: &str) -> bool {
        let key = match k {
            "Tab" => Key::Named(NamedKey::Tab),
            "Esc" => Key::Named(NamedKey::Escape),
            c => Key::Character(c.into()),
        };
        let Some(action) = keys::action(&key, ModifiersState::empty(), false, app.key_mode()) else { return false };
        assert!(!app.apply(action), "{k} closes no window here");
        true
    }

    fn panel(app: &App) -> Vec<String> {
        app.scene_text(Instant::now()).and_then(|t| t.page).expect("the panel is open")
    }

    /// The app with a stand-in loopback started at 50 ms (#29).
    /// `config`: a different one per test, as each names its temp dir.
    fn app_with_loopback(config: &'static str) -> (App, crate::audio::Loopback, crate::audio::fake::Script, std::path::PathBuf) {
        let (mut app, file) = app_with(config);
        let (loopback, script) = crate::audio::fake::loopback(50);
        let keys = loopback.keys();
        let now = audio_started(&app.opts.live, &keys, keys.start(50));
        app.opts.audio = AudioSetup::Loopback { keys, now };
        (app, loopback, script, file)
    }

    fn keys(app: &App) -> LoopbackKeys {
        let AudioSetup::Loopback { keys, .. } = &app.opts.audio else { panic!("no loopback") };
        keys.clone()
    }

    fn audio_line(app: &mut App) -> String {
        if app.overlay.page.is_none() {
            press(app, "Tab");
        }
        panel(app)[6].clone()
    }

    #[test]
    fn a_dashboard_to_move_survives_newer_placements_and_the_latest_mode_wins() {
        // d, then f before the dashboard maps: both happen, f's mode last
        // (it ran on its own thread with d's mode, and undid f's).
        let job = |mode, dashboard: Option<Vec<String>>| Placement { mode, size: (1280, 720), dashboard, refocus: None };
        let merged = job("fullscreen", Some(vec!["0x1".into()])).then(job("borderless", None)).then(job("windowed", None));
        assert_eq!((merged.mode, merged.dashboard), ("windowed", Some(vec!["0x1".to_string()])));
        assert_eq!(job("fullscreen", None).then(job("borderless", None)).dashboard, None);
        // --monitor's focus to give back is kept too, until placed.
        let first = Placement { refocus: Some("DP-2".into()), ..job("fullscreen", None) };
        assert_eq!(first.then(job("windowed", None)).refocus.as_deref(), Some("DP-2"));
    }

    #[test]
    fn a_held_frame_waits_for_the_next_one_unless_it_is_fresh_or_overdue() {
        // Sent on the compositor's frame callback alone, a held frame kept
        // every later one a refresh behind (55 ms kernel -> presented, not
        // 41, on the MS2130 2026-10-16): the next card frame replaces it.
        let (mut app, _) = app_with("# held frame\n");
        let held = |app: &App, ago: Duration| {
            let at = now_monotonic().saturating_sub(ago);
            app.slot.lock().unwrap().tag = Some(Tag { captured: at, dequeued: at, repaint: false });
        };
        held(&app, Duration::from_millis(2));
        assert!(app.send_now(), "within HOLD of the callback: sent, not dropped");
        held(&app, Duration::from_millis(10));
        assert!(!app.send_now(), "a stale held frame waits for the next");
        app.arrived = true;
        assert!(app.send_now(), "a frame that came on this wake goes out");
        app.arrived = false;
        app.repaint = true;
        assert!(app.send_now(), "an overlay change goes out");
        app.repaint = false;
        held(&app, Duration::from_millis(30));
        assert!(app.send_now(), "past 1.5 frame times (25 ms at 60 fps) the card has stopped: sent alone");
    }

    #[test]
    fn the_refreshes_without_a_picture_are_not_counted_as_repeats() {
        // The laptop (#40): "repeated refreshes 1614" after a pull. The last
        // presents' outcomes before the pull came in after the no-signal
        // state, with their pre-pull times; the first present after the
        // return counted every refresh since.
        let (mut app, _) = app_with("# repeats\n");
        let live = Arc::new(crate::dashboard::Live::new());
        app.stats = Stats::new(Duration::ZERO, false, live.clone());
        let refresh = Duration::from_micros(16_681);
        let present = |app: &mut App, at: Duration| {
            let tag = Tag { captured: at - Duration::from_millis(30), dequeued: at - Duration::from_millis(20), repaint: false };
            app.stats.presented(tag, at, refresh, 0);
        };
        // The card pulled out, then an HDMI pull (an ordinary no signal).
        for (base, reason) in [(0, Reason::Unplugged("gone".into())), (100, Reason::Nothing)] {
            let at = |s: u64| Duration::from_secs(base + s);
            present(&mut app, at(1));
            let before = live.counts().repeated;
            app.signal(Signal::NoSignal { since: crate::capture::Since::Stopped, reason: reason.clone() });
            // An outcome arriving after the no-signal state, from before it.
            present(&mut app, at(1) + refresh);
            app.signal(Signal::Picture);
            present(&mut app, at(26));
            present(&mut app, at(26) + refresh);
            assert_eq!(live.counts().repeated - before, 0, "{reason:?}");
        }
    }

    #[test]
    fn the_sound_comes_back_with_a_card_pulled_out_and_plugged_back() {
        let (mut app, _loopback, script, _) = app_with_loopback("# sound back\n");
        let unplugged = Signal::NoSignal { since: crate::capture::Since::Stopped, reason: Reason::Unplugged("gone".into()) };
        // The loopback dies with the card's audio input; its notice may
        // not have been taken when the picture is back.
        crate::audio::fake::kill(&keys(&app));
        app.signal(unplugged.clone());
        assert_eq!(script.starts.borrow().len(), 1, "not while the card is out");
        app.signal(Signal::Picture);
        assert_eq!(*script.starts.borrow(), [50, 50], "started again at its delay");
        assert!(app.loopback().is_some_and(|c| c.dead().is_none()));
        // Once: a later picture after an ordinary no-signal starts nothing.
        app.signal(Signal::NoSignal { since: crate::capture::Since::Stopped, reason: Reason::Nothing });
        app.signal(Signal::Picture);
        assert_eq!(script.starts.borrow().len(), 2);
        // A loopback still running through the pull is left alone.
        app.signal(unplugged);
        app.signal(Signal::Picture);
        assert_eq!(script.starts.borrow().len(), 2);
    }

    #[test]
    fn a_dead_loopback_starts_again_on_a_sync_key_at_the_new_delay() {
        let (mut app, _loopback, script, file) = app_with_loopback("DELAY=0.05\n# sync\n");
        crate::audio::fake::kill(&keys(&app));
        // Its wake: no longer running, in the panel and for w.
        app.audio_check();
        assert_eq!(audio_line(&mut app), "m 9 0 Audio        not running, see the log: output: Device disconnected");
        assert!(app.loopback().is_none());
        assert!(!app.apply(Action::Sync(10)));
        assert_eq!(*script.starts.borrow(), [50, 60]);
        assert_eq!(app.loopback().map(Controls::delay_ms), Some(60));
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Audio sync 60 ms"));
        audio_line(&mut app);
        assert_eq!(panel(&app)[7], "^-/^+ Audio sync   60 ms   (alt -/+: 50 ms steps)");
        // A running one only moves its sync: no restart.
        assert!(!app.apply(Action::Sync(-10)));
        assert_eq!(script.starts.borrow().len(), 2);
        // Dead again, the key before the death's wake: still a restart.
        crate::audio::fake::kill(&keys(&app));
        assert!(!app.apply(Action::Sync(10)));
        assert_eq!(*script.starts.borrow(), [50, 60, 60]);
        assert!(app.loopback().is_some_and(|c| c.dead().is_none()));
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_dead_loopback_starts_again_on_m_with_its_level_carried_over() {
        let (mut app, _loopback, script, file) = app_with_loopback("DELAY=0.05\n# m\n");
        app.loopback().unwrap().change_level(LevelChange::Volume(-30));
        let was = crate::level::Level { volume: 70, mute: false };
        assert_eq!(crate::audio::fake::level_settles(app.loopback().unwrap(), was), Some(Ok(was)));
        crate::audio::fake::kill(&keys(&app));
        // m before the death's wake is handled: the dead one is not used.
        assert!(!app.apply(Action::Level(LevelChange::Mute)));
        assert_eq!(*script.starts.borrow(), [50, 50], "at the delay it had");
        let want = crate::level::Level { volume: 70, mute: true };
        assert_eq!(crate::audio::fake::level_settles(app.loopback().unwrap(), want), Some(Ok(want)), "70 % carried over, then muted");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_restart_that_fails_says_the_new_reason() {
        let (mut app, _loopback, script, file) = app_with_loopback("DELAY=0.05\n# fails\n");
        crate::audio::fake::kill(&keys(&app));
        app.audio_check();
        *script.fail.borrow_mut() = Some("no PipeWire source card-in (SRC)".into());
        assert!(!app.apply(Action::Sync(10)));
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Audio: no PipeWire source card-in (SRC)"));
        assert_eq!(audio_line(&mut app), "m 9 0 Audio        not running, see the log: no PipeWire source card-in (SRC)");
        // The next key tries again, at the delay asked for last.
        *script.fail.borrow_mut() = None;
        assert!(!app.apply(Action::Level(LevelChange::Volume(5))));
        assert_eq!(*script.starts.borrow(), [50, 60, 60]);
        assert!(app.loopback().is_some());
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn overlay_keys_and_w_save_through_the_settings() {
        let (mut app, file) = app_with("# mine\nLEVELS=auto\nCONTRAST=2\n");
        assert!(!press(&mut app, "l"), "l only with the overlay open");
        assert!(press(&mut app, "Tab"));
        let p = panel(&app);
        assert_eq!(p[3], "l     Levels       auto");
        assert_eq!(p[5], "1-8   Colour       contrast 2 · brightness 0 · gamma 0 · saturation 0");
        assert_eq!(p[6], "      Audio        none");
        assert_eq!(p[8], "t     Setup tips   PlayStation 5");
        assert_eq!(p[9], "w     Save         nothing changed");

        assert!(press(&mut app, "l"));
        assert!(press(&mut app, "c"));
        assert_eq!(panel(&app)[9], "w     Save         SCALING=fast LEVELS=limited", "lib/settings.sh's order");
        assert!(press(&mut app, "w"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# mine\nLEVELS=limited\nCONTRAST=2\nSCALING=fast\n");
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Saved: SCALING=fast LEVELS=limited"));
        assert_eq!(panel(&app)[9], "w     Save         nothing changed", "the save is the new baseline");
        // view.sh reads what w wrote as saved and valid (Linux: view.sh is bash).
        #[cfg(target_os = "linux")]
        {
            let out = std::process::Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../view.sh"))
                .arg("--settings")
                .env("CONFIG_FILE", &file)
                .output()
                .unwrap();
            let report = String::from_utf8_lossy(&out.stdout);
            let row =
                |k: &str| report.lines().find(|l| l.split_whitespace().next() == Some(k)).unwrap_or("").split_whitespace().collect::<Vec<_>>();
            assert_eq!(row("LEVELS"), ["LEVELS", "limited", "saved"], "{report}");
            assert_eq!(row("SCALING"), ["SCALING", "fast", "saved"], "{report}");
            assert!(out.status.success() && report.contains("all valid"), "{report}");
        }

        assert!(press(&mut app, "r"));
        assert_eq!(panel(&app)[9], "w     Save         CONTRAST=0");
        assert!(press(&mut app, "t"));
        assert_eq!(app.overlay.page, Some(Page::Tips));
        assert!(press(&mut app, "Esc"));
        assert_eq!(app.overlay.page, None);
        assert!(app.repaint, "every change draws again");
        app.repaint = false;
        app.reconvert = false;
        assert!(press(&mut app, "2"));
        assert!(app.repaint && app.reconvert, "a colour key redraws a frozen frame converted again");

        // f: the panel's Display line and w's WINDOW follow it.
        press(&mut app, "Tab");
        press(&mut app, "f");
        assert_eq!(panel(&app)[2], "f     Display      Borderless");
        assert!(panel(&app)[9].contains("WINDOW=borderless"));
        press(&mut app, "f");
        assert_eq!(app.opts.window, WindowMode::Windowed { width: 1280, height: 720 });
        press(&mut app, "f");
        assert_eq!(app.opts.window, WindowMode::Fullscreen);
        press(&mut app, "Esc");
        press(&mut app, "f");
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Display: Borderless"));

        // w with nothing changed says so and writes nothing.
        let (mut quiet, quiet_file) = app_with("LEVELS=full\n");
        press(&mut quiet, "Tab");
        press(&mut quiet, "w");
        assert_eq!(quiet.overlay.current_message(Instant::now()), Some("Nothing changed since the feed started"));
        assert_eq!(std::fs::read_to_string(&quiet_file).unwrap(), "LEVELS=full\n");
        for f in [file, quiet_file] {
            std::fs::remove_dir_all(f.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn every_saveable_setting_has_a_live_value() {
        let (app, file) = app_with("# saveable\n");
        let rows = crate::settings::rows();
        let flagged: Vec<&str> = rows.iter().filter(|r| r.has_flag("save")).map(|r| r.name.as_str()).collect();
        assert!(flagged.contains(&"DELAY") && flagged.contains(&"WINDOW"), "{flagged:?}");
        let without_delay: Vec<&str> = flagged.iter().copied().filter(|n| *n != "DELAY").collect();
        assert_eq!(saveable(&rows, &app.values_with(None), false), without_delay);
        assert_eq!(saveable(&rows, &app.values_with(Some(50)), true), flagged);
        assert_eq!(app.values_with(Some(50))["DELAY"], "0.050", "in seconds, as the mpv script writes it");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    /// Live values for these names, all empty.
    fn live(names: &[&'static str]) -> overlay::Values {
        names.iter().map(|n| (*n, String::new())).collect()
    }

    const LIVE: [&str; 7] = ["WINDOW", "SCALING", "LEVELS", "BRIGHTNESS", "CONTRAST", "SATURATION", "GAMMA"];

    #[test]
    #[should_panic(expected = "GAMMA is flagged save in lib/settings.sh but has no live value")]
    fn a_saveable_setting_without_a_live_value_fails_loudly() {
        saveable(&crate::settings::rows(), &live(&LIVE[..6]), false);
    }

    #[test]
    #[should_panic(expected = "DELAY is flagged save")]
    fn the_delay_has_a_live_value_while_the_loopback_runs() {
        saveable(&crate::settings::rows(), &live(&LIVE), true);
    }

    #[test]
    fn letterbox_in_whole_pixels() {
        assert_eq!(letterbox((1920, 1200), (1920, 1080)), [0, 60, 1920, 1080], "1:1, bars above and below");
        assert_eq!(letterbox((2560, 1440), (1920, 1080)), [0, 0, 2560, 1440]);
        assert_eq!(letterbox((3440, 1440), (1920, 1080)), [440, 0, 2560, 1440]);
        assert_eq!(letterbox((950, 1127), (1920, 1080)), [0, 297, 950, 534]);
    }

    #[test]
    fn window_modes_from_the_settings() {
        assert_eq!(WindowMode::from_settings("fullscreen", "1280x720"), Ok(WindowMode::Fullscreen));
        assert_eq!(
            WindowMode::from_settings("windowed", "1280x720"),
            Ok(WindowMode::Windowed { width: 1280, height: 720 })
        );
        assert!(WindowMode::from_settings("windowed", "big").is_err());
    }

    #[test]
    fn one_frame_wake_waits_at_a_time() {
        let pending = AtomicBool::new(false);
        assert!(posts(&pending, &Wake::Frame));
        assert!(!posts(&pending, &Wake::Frame), "a second frame before the first is taken: no wake");
        assert!(posts(&pending, &Wake::End), "other wakes always go");
        assert!(posts(&pending, &Wake::Signal(crate::capture::Signal::Picture)));
        pending.store(false, Ordering::Release);
        assert!(posts(&pending, &Wake::Frame), "taken: the next frame wakes again");
    }
}
