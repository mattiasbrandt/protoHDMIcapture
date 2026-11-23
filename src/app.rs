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

use crate::audio::{LoopbackKeys, Status};
use crate::capture::{Format, FrameSource, Signal, now_monotonic};
use crate::desktop::Desktop;
use crate::duplicate::Kind;
use crate::framegen::{self, Generator, Mode, Pair, Schedule, Show};
use crate::gamemode::GameMode;
use crate::gpustat;
use crate::gputime::GpuTimer;
use crate::keys::{self, Action};
use crate::live_changes::LiveChanges;
use crate::nis::{self, Nis};
use crate::osd::{Osd, PageKind, Scene};
use crate::overlay::{self, Notify, Overlay, Page};
use crate::pacing::{Pacing, Waiting};
use crate::picture::{self, Colour};
#[cfg(target_os = "linux")]
use crate::present::Presentation;
use crate::present::{Feedback, Tag};
use crate::run::{End, Host, Outcome, Plan, Quit, Slot, Wake, Waker};
use crate::scale::{self, Axis, Filter, Upscaling};
use crate::screenshot::Screenshot;
use crate::settings::Settings;
use crate::sources;
use crate::stats::Stats;
use crate::wizard::Wizard;

/// How long a new dashboard has to map (`Desktop::move_dashboard`'s wait):
/// d meanwhile starts no second one.
const DASHBOARD_MAPS: Duration = Duration::from_secs(3);

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
    /// WINDOW, WINDOW_SIZE, LEVELS, the colour values and SCALING at
    /// start, and what w saves of them.
    pub live_changes: LiveChanges,
    /// Fullscreen on this output instead of the focused one (measurements).
    pub monitor: Option<String>,
    /// End this window after this long (measurements): `Outcome::TimeUp`.
    pub seconds: Option<u64>,
    /// Print timing every 10 s and a summary (measurements).
    pub stats: bool,
    pub audio: AudioSetup,
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
    /// Frame generation's mode (framegen.rs, #66): a dev build's
    /// `--framegen`; None, the frames as the card sends them.
    pub framegen: Option<Mode>,
    /// Where the frames also go: a dev build's `--record` (clip.rs, #79).
    pub tap: Option<Box<dyn crate::run::Tap>>,
}

impl Options {
    /// A window with these picture settings and dashboard numbers, and
    /// nothing else: no measurement, no audio, the generic desktop, no
    /// wizard. Callers set what differs with `..Options::new(..)`.
    pub fn new(live_changes: LiveChanges, live: Arc<crate::dashboard::Live>) -> Self {
        Options {
            live_changes,
            monitor: None,
            seconds: None,
            stats: false,
            audio: AudioSetup::Off(String::new()),
            desktop: Desktop::Generic,
            source: String::new(),
            settings: None,
            wizard: None,
            live,
            dashboard: None,
            refocus: None,
            framegen: None,
            tap: None,
        }
    }
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
    /// AUDIO=loopback: the keys and the status of the session's loopback,
    /// which says itself what happens to it (audio.rs, #42).
    Loopback(LoopbackKeys),
}

impl WindowMode {
    /// f: fullscreen -> borderless -> windowed (WINDOW's order).
    pub fn next(self, size: (u32, u32)) -> Self {
        match self {
            Self::Fullscreen => Self::Borderless,
            Self::Borderless => Self::Windowed { width: size.0, height: size.1 },
            Self::Windowed { .. } => Self::Fullscreen,
        }
    }

    /// As the settings spell it, and as the panel names it.
    pub fn names(self) -> (&'static str, &'static str) {
        match self {
            Self::Fullscreen => ("fullscreen", "Fullscreen"),
            Self::Borderless => ("borderless", "Borderless"),
            Self::Windowed { .. } => ("windowed", "Windowed"),
        }
    }
}

/// The overlay's text for one frame (osd.rs draws it).
struct SceneText {
    page: Option<Vec<String>>,
    kind: PageKind,
    key_bar: Option<String>,
    no_signal: Option<String>,
    message: Option<String>,
    /// The enhancement that runs (dev builds, #80).
    label: Option<String>,
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
    /// UPSCALING=fsr's FSR 1 passes (scale.rs).
    sharp: SharpPasses,
    /// The scaler in use: None is the bilinear sampler.
    filter: Option<Filter>,
    /// UPSCALING=nis's NVScaler pass (nis.rs), and whether it is in use
    /// (then `filter` is None).
    nis_pass: NisPass,
    nis: Option<Nis>,
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

impl Drop for Gfx {
    /// The feedback before the surface, however `Gfx` goes: DXGI's holds a
    /// reference to the surface's swapchain (present.rs), and fields drop
    /// in their order, the surface first.
    fn drop(&mut self) {
        self.feedback = None;
    }
}

/// How long a second u has to confirm the setup over unsaved changes.
const SETUP_CONFIRM: Duration = Duration::from_secs(5);

struct App {
    /// This window's number (`WINDOWS`).
    window_no: u64,
    /// A frame wake is posted and not yet taken (`Winit::open`).
    frame_wake: Arc<AtomicBool>,
    opts: Options,
    format: Format,
    slot: Arc<Mutex<Slot>>,
    gfx: Option<Gfx>,
    overlay: Overlay,
    /// Messages from other threads, taken on `UserEvent::Notice`.
    notices: Arc<Mutex<Vec<(String, Duration)>>>,
    notify: Notify,
    /// When d last started a dashboard (`DASHBOARD_MAPS`).
    dashboard_opened: Option<Instant>,
    /// When u last asked to confirm the setup over unsaved changes.
    setup_armed: Option<Instant>,
    /// LEVELS or a colour value changed: convert the last frame again on a
    /// repaint, so a frozen feed shows it too.
    reconvert: bool,
    /// The window placement worker, once started.
    placer: Option<std::sync::mpsc::Sender<Placement>>,
    modifiers: ModifiersState,
    /// When to draw and wake (pacing.rs), and the presentation numbers.
    pacing: Pacing,
    /// The run's end (run.rs): set here when the window closes or fails.
    end: End,
    /// Frame generation, when it runs (`Options::framegen`, unless the GPU
    /// refused it).
    framegen: Option<Framegen>,
    /// GPU use (gpustat.rs, #72), sampled only while a GPU enhancement is
    /// on.
    gpu: gpustat::Monitor,
    /// GameMode (gamemode.rs, #77), asked for only while a GPU enhancement
    /// is on; unregistered when the window closes.
    gamemode: GameMode,
    /// The adapter's PCI vendor and device ids and its name, for the GPU
    /// use's source.
    adapter: Option<(u32, u32, String)>,
    /// The adapter's kind, for frame generation's refusal when `g` turns
    /// it on mid-run (#83).
    device_type: Option<wgpu::DeviceType>,
    /// The size of the monitor the window is on, in pixels, when the
    /// toolkit tells (#85): the scaling suggestion's input. Read on every
    /// relayout, so it follows a move to another monitor.
    monitor_size: Option<(u32, u32)>,
}

/// Frame generation in the window (framegen.rs, #66): the schedule, and
/// its frames on the GPU from the first card frame that needs them.
/// Switching it on mid-run is making one of these.
struct Framegen {
    schedule: Schedule,
    gpu: Option<FramegenGpu>,
    /// The new frame a generated frame held: its times, for the present
    /// that shows it on the next duplicate.
    held: Option<Tag>,
    /// Duplicate frames withheld since the last present (new-frames-only).
    withheld: u32,
    /// The slot's `superseded` at the last take: a rise means the window
    /// took a frame over one it never took, a refresh behind.
    superseded: u64,
    /// The slot's `superseded_duplicates` at the last take.
    superseded_duplicates: u64,
}

impl Framegen {
    fn new(mode: Mode) -> Self {
        Self { schedule: Schedule::new(mode), gpu: None, held: None, withheld: 0, superseded: 0, superseded_duplicates: 0 }
    }

    /// The window took a card frame, with the slot's counts then: whether
    /// it is late (it replaced a frame the window never took). In
    /// new-frames-only, a duplicate frame replaced in the slot was not
    /// presented either, as a withheld one is not: its refresh is not a
    /// repeated one (#72: 18 of a 20 s run's repeats were these).
    fn took(&mut self, superseded: u64, superseded_duplicates: u64) -> bool {
        let late = superseded > self.superseded;
        let skipped = superseded_duplicates.saturating_sub(self.superseded_duplicates);
        self.superseded = superseded;
        self.superseded_duplicates = superseded_duplicates;
        if self.schedule.mode() == Mode::NewOnly {
            self.withheld = self.withheld.saturating_add(u32::try_from(skipped).unwrap_or(u32::MAX));
        }
        late
    }
}

/// The last two new frames, converted, at the capture's size, and the
/// mode's generator. What is shown is copied or generated from them into
/// `Gfx::rgb`, which the scale pass and the screenshot read as before.
struct FramegenGpu {
    frames: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
    /// Which of `frames` holds the newest new frame.
    newest: usize,
    generator: Option<Box<dyn Generator>>,
}

impl FramegenGpu {
    fn new(device: &wgpu::Device, (w, h): (u32, u32), mode: Mode) -> Self {
        let frame = || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("framegen frame"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let frames = [frame(), frame()];
        let views = [frames[0].create_view(&Default::default()), frames[1].create_view(&Default::default())];
        Self { frames, views, newest: 0, generator: framegen::generator(mode, device) }
    }
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
/// and interpolated with spline36 for the odd one (#85). Then LEVELS and the colour values
/// (picture.rs).
pub(crate) fn convert_shader(width: u32) -> String {
    let last = width / 2 - 1;
    // An odd pixel's chroma: spline36 over the six chroma samples around it
    // (scale.rs `chroma_taps`, #85), clamped at the row's ends.
    let taps = scale::chroma_taps().map(|w| format!("{w:.9}")).join(", ");
    format!(
        r#"{wgsl}
@group(0) @binding(0) var yuyv: texture_2d<f32>;
@group(0) @binding(1) var<uniform> pic: Picture;

const CHROMA_TAPS = array<f32, 6>({taps});

@fragment fn fs(in: VOut) -> @location(0) vec4f {{
    let x = u32(in.pos.x);
    let y = u32(in.pos.y);
    let t = textureLoad(yuyv, vec2u(x / 2u, y), 0);
    var luma = t.r;
    var c = t.ga;
    if ((x & 1u) == 1u) {{
        luma = t.b;
        c = vec2f(0.0);
        for (var i = 0; i < 6; i++) {{
            let k = u32(clamp(i32(x / 2u) + i - 2, 0, {last}));
            c += CHROMA_TAPS[i] * textureLoad(yuyv, vec2u(k, y), 0).ga;
        }}
        c = clamp(c, vec2f(0.0), vec2f(1.0));
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
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
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
pub fn run(source: Box<dyn FrameSource>, mut opts: Options, quit: &Quit) -> Outcome {
    let plan = Plan {
        seconds: opts.seconds.map(Duration::from_secs),
        live: opts.live.clone(),
        // The wizard's page says what its feed does (its signal check).
        log_signal: opts.wizard.is_none(),
        stats: opts.stats,
        tap: opts.tap.take(),
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
        if let AudioSetup::Loopback(keys) = &opts.audio {
            keys.set_notify(notify.clone());
        }
        let start = now_monotonic();
        let stats = opts.stats;
        let mut app = App {
            window_no,
            frame_wake: self.frame_wake.clone(),
            format,
            pacing: Pacing::new(start, format.fps, Stats::new(start, opts.stats, opts.live.clone())),
            overlay: Overlay::new(&opts.source, Instant::now()),
            notices,
            notify,
            dashboard_opened: None,
            setup_armed: None,
            reconvert: false,
            placer: None,
            modifiers: ModifiersState::empty(),
            opts,
            slot: slot.clone(),
            gfx: None,
            end: end.clone(),
            framegen: None,
            gpu: gpustat::Monitor::new(std::path::Path::new("/proc/self/fdinfo")),
            gamemode: GameMode::new(stats),
            adapter: None,
            device_type: None,
            monitor_size: None,
        };
        app.opts.live.set_gamemode(app.gamemode.status());
        let delay_ms = app.delay_ms();
        app.opts.live_changes.mark_baseline(delay_ms);
        // Ended before it began (a signal as the run started): no window.
        if !end.is_set()
            && let Err(e) = event_loop.run_app_on_demand(&mut app)
        {
            end.set_or_warn(Outcome::Failed(format!("event loop: {e}")));
        }
        if app.opts.stats {
            let superseded = slot.lock().expect("the slot's lock is never poisoned").superseded;
            app.pacing.stats().summary(superseded);
            for line in gpustat::summary(&app.gpu.history()) {
                eprintln!("{line}");
            }
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
    let feedback: Option<Box<dyn Feedback>> =
        match (window.display_handle().map_err(|e| e.to_string())?.as_raw(), window.window_handle().map_err(|e| e.to_string())?.as_raw()) {
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
    match crate::present::FrameStatistics::new(surface) {
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

/// UPSCALING=fsr's passes (scale.rs, #70): EASU from the frame into an
/// intermediate texture at the viewport's size, then RCAS from it into the
/// viewport.
struct SharpPasses {
    easu: wgpu::RenderPipeline,
    rcas: wgpu::RenderPipeline,
    /// The frame and `easu_buffer`.
    easu_bind_group: wgpu::BindGroup,
    easu_buffer: wgpu::Buffer,
    rcas_buffer: wgpu::Buffer,
    /// The intermediate texture and RCAS's bind group, made for the
    /// viewport in `relayout` when UPSCALING=fsr enlarges.
    up: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

/// UPSCALING=nis's pass (nis.rs, #71): the frame and the constants into the
/// viewport, in the scale pass's place.
struct NisPass {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    buffer: wgpu::Buffer,
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
        attrs = match self.opts.live_changes.window() {
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
        let required_features =
            if self.opts.stats && adapter.features().contains(timestamps) { timestamps } else { wgpu::Features::empty() };
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
        config.present_mode =
            if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::Fifo };
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
        // Frame generation, unless this GPU is refused it: said once, here.
        let framegen = self.opts.framegen.filter(|&mode| {
            let info = adapter.get_info();
            let why = framegen::refusal(mode, info.device_type, &info.name);
            if let Some(why) = &why {
                eprintln!("note: {why}");
                self.opts.live.event(why.clone());
            }
            why.is_none()
        });
        // With frame generation, what is shown is copied into it; in a dev
        // build `g` can turn it on mid-run (#83).
        let copied = if framegen.is_some() || cfg!(feature = "dev") { wgpu::TextureUsages::COPY_DST } else { wgpu::TextureUsages::empty() };
        let rgb = frame_texture(
            "rgb",
            w,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | copied,
        );
        if let Some(mode) = framegen {
            if self.opts.stats {
                eprintln!("frame generation: {}", mode.as_str());
            }
            self.opts.live.framegen(mode.as_str());
            self.framegen = Some(Framegen::new(mode));
        }
        let info = adapter.get_info();
        // The GPU, driver and display server the run measures on (#87).
        let machine = gpustat::describe(&info, gpustat::display_server(|k| std::env::var(k).ok()));
        self.pacing.stats().set_machine(machine.clone());
        self.opts.live.set_machine(machine);
        self.device_type = Some(info.device_type);
        self.adapter = Some((info.vendor, info.device, info.name));
        self.update_gpu();
        let rgb_view = rgb.create_view(&Default::default());

        let convert_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("convert"),
            entries: &[texture_entry(0, false), uniform_entry(1)],
        });
        let picture_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("picture"),
            size: size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&picture_buffer, 0, &picture::bytes(&self.opts.live_changes.picture().uniform(w, h, self.format.coding())));
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
        let convert_pipeline = pipeline(&device, "convert", &convert_shader(w), &convert_layout, wgpu::TextureFormat::Rgba8Unorm);

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
            entries: &[texture_entry(0, false), uniform_entry(1)],
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
        // UPSCALING=fsr: the filters' layout (a texture and a uniform).
        let fsr_uniform = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: scale::FSR_UNIFORM_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let (easu_buffer, rcas_buffer) = (fsr_uniform("easu"), fsr_uniform("rcas"));
        let easu_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("easu"),
            layout: &filter_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&rgb_view) },
                wgpu::BindGroupEntry { binding: 1, resource: easu_buffer.as_entire_binding() },
            ],
        });
        let sharp = SharpPasses {
            easu: pipeline(&device, "easu", &scale::fsr_shader(scale::FSR_EASU), &filter_layout, scale::INTERMEDIATE),
            rcas: pipeline(&device, "rcas", &scale::fsr_shader(scale::FSR_RCAS), &filter_layout, config.format),
            easu_bind_group,
            easu_buffer,
            rcas_buffer,
            up: None,
        };
        let nis_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nis"),
            size: nis::UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let nis_pass = NisPass {
            pipeline: pipeline(&device, "nis", nis::WGSL, &filter_layout, config.format),
            bind_group: device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nis"),
                layout: &filter_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&rgb_view) },
                    wgpu::BindGroupEntry { binding: 1, resource: nis_buffer.as_entire_binding() },
                ],
            }),
            buffer: nis_buffer,
        };

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
            sharp,
            filter: None,
            nis_pass,
            nis: None,
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

    /// GPU use sampled and shown, and GameMode asked for, while a GPU
    /// enhancement is on (frame generation, an enhancement SCALING preset),
    /// not otherwise (#72, #77): called where either can change.
    fn update_gpu(&mut self) {
        let on = gpustat::enhanced(self.framegen.as_ref().map(|f| f.schedule.mode()), self.upscaling_running());
        self.gamemode.set(on);
        let Some((vendor, device, name)) = self.adapter.clone() else { return };
        match self.gpu.set(on, || gpustat::source_for(vendor, device, &name), self.opts.stats, gpustat::PERIOD) {
            Ok(shown) => {
                self.opts.live.set_gpu(shown.clone());
                self.pacing.stats().set_gpu(shown);
            }
            Err(e) => eprintln!("warning: GPU use: {e}"),
        }
    }

    /// `g` (a dev build, #83): frame generation off, then each judged
    /// approach, then off, live. Through the same path as a start with
    /// `--framegen`: a refused GPU keeps it off (said), the generator is
    /// made on the first card frame that needs it, the last frame is
    /// converted again for the new path, and GameMode and the GPU section
    /// follow (`update_gpu`).
    fn cycle_framegen(&mut self) {
        let mut next = framegen::cycle(self.framegen.as_ref().map(|f| f.schedule.mode()), &framegen::judged());
        let refused = match (next, self.device_type, self.adapter.as_ref()) {
            (Some(mode), Some(kind), Some((_, _, name))) => framegen::refusal(mode, kind, name),
            _ => None,
        };
        if refused.is_some() {
            next = None;
        }
        self.framegen = next.map(Framegen::new);
        match next {
            Some(mode) => self.opts.live.framegen(mode.as_str()),
            None => self.opts.live.framegen_off(),
        }
        let name = next.map_or("off", Mode::as_str);
        if self.opts.stats {
            eprintln!("frame generation: {name}");
        }
        match refused {
            Some(why) => self.overlay.message(format!("Warning: {why}"), Duration::from_millis(3000), Instant::now()),
            None => self.overlay.message(format!("Frame generation: {name}"), Duration::from_millis(1500), Instant::now()),
        }
        self.reconvert = true;
        self.pacing.repaint();
        self.update_gpu();
    }

    /// How the picture fits the monitor the window is on (#85); None while
    /// the monitor is not known.
    fn monitor_fit(&self) -> Option<scale::Fit> {
        self.monitor_size.map(|m| scale::fit((self.format.width, self.format.height), m))
    }

    /// The picture is scaled into the window: the viewport is not the
    /// frame's own size (#85).
    fn scaled(&self) -> bool {
        self.gfx.as_ref().is_some_and(|g| (g.viewport[2], g.viewport[3]) != (self.format.width, self.format.height))
    }

    /// The viewport and the scaler for the surface's size, UPSCALING and
    /// SCALING; after a resize, a move to another monitor and on c. Then
    /// GPU use and GameMode follow whether Upscaling runs (`update_gpu`).
    fn relayout(&mut self) {
        let Some(gfx) = self.gfx.as_mut() else { return };
        let frame = (self.format.width, self.format.height);
        self.monitor_size = gfx.window.current_monitor().map(|m| (m.size().width, m.size().height)).filter(|&(w, h)| w > 0 && h > 0);
        // The wizard's GPU enhancements step decides Upscaling from these (#85).
        if let Some(w) = &self.opts.wizard {
            let mut w = w.borrow_mut();
            w.monitor = self.monitor_size;
            w.gpu = self.adapter.as_ref().map(|(_, _, name)| (name.clone(), self.device_type == Some(wgpu::DeviceType::DiscreteGpu)));
        }
        gfx.viewport = letterbox((gfx.config.width, gfx.config.height), frame);
        let [_, _, vw, vh] = gfx.viewport;
        let scaling = self.opts.live_changes.scaling();
        let upscaling = self.opts.live_changes.upscaling();
        // UPSCALING (#85): NIS where NVScaler applies (1x to 2x a side), FSR
        // 1 on any enlargement; else SCALING's filter (scale::filter).
        gfx.nis = if upscaling == Upscaling::Nis { nis::nis(frame, (vw, vh)) } else { None };
        if let Some(n) = &gfx.nis {
            gfx.queue.write_buffer(&gfx.nis_pass.buffer, 0, &n.bytes((gfx.viewport[0], gfx.viewport[1])));
        }
        let fsr = scale::fsr(upscaling, frame, (vw, vh));
        gfx.filter = if gfx.nis.is_some() || fsr { None } else { scale::filter(scaling, frame, (vw, vh)) };
        gfx.filters.down = None;
        gfx.sharp.up = None;
        if fsr {
            let view = gfx
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("easu"),
                    size: wgpu::Extent3d { width: vw, height: vh, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: scale::INTERMEDIATE,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&Default::default());
            gfx.queue.write_buffer(&gfx.sharp.easu_buffer, 0, &scale::fsr_bytes(frame, (vw, vh)));
            gfx.queue.write_buffer(&gfx.sharp.rcas_buffer, 0, &scale::fsr_bytes((vw, vh), (vw, vh)));
            let bind = gfx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rcas"),
                layout: &gfx.filters.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: gfx.sharp.rcas_buffer.as_entire_binding() },
                ],
            });
            gfx.sharp.up = Some((view, bind));
        }
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
                "scaling: upscaling {}, scaling {} -> {vw}x{vh}, {}",
                upscaling.as_str(),
                scaling.as_str(),
                if gfx.nis.is_some() {
                    "nvidia image scaling"
                } else if gfx.sharp.up.is_some() {
                    "fsr1 (easu + rcas)"
                } else {
                    gfx.filter.as_ref().map_or("bilinear sampler", |f| f.name)
                }
            );
        }
        self.update_gpu();
    }

    /// The Upscaling that runs: UPSCALING where it enlarges the picture,
    /// Off otherwise (1:1, a downscale, NIS past 2x).
    fn upscaling_running(&self) -> Upscaling {
        match &self.gfx {
            Some(g) if g.nis.is_some() => Upscaling::Nis,
            Some(g) if g.sharp.up.is_some() => Upscaling::Fsr,
            _ => Upscaling::Off,
        }
    }

    /// A colour key (mpv's 1..8): the next frame shows the change.
    fn adjust_colour(&mut self, colour: Colour, by: i32) {
        let v = self.opts.live_changes.adjust_colour(colour, by);
        // mpv's message for its colour keys.
        let name = match colour {
            Colour::Contrast => "Contrast",
            Colour::Brightness => "Brightness",
            Colour::Gamma => "Gamma",
            Colour::Saturation => "Saturation",
        };
        self.overlay.message(format!("{name}: {v}"), Duration::from_millis(1500), Instant::now());
        self.picture_changed();
    }

    fn picture_changed(&mut self) {
        if let Some(gfx) = &self.gfx {
            let u = self.opts.live_changes.picture().uniform(self.format.width, self.format.height, self.format.coding());
            gfx.queue.write_buffer(&gfx.picture_buffer, 0, &picture::bytes(&u));
        }
        self.pacing.repaint();
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
                self.opts.live_changes.cycle_levels();
                self.picture_changed();
            }
            Action::CycleUpscaling => {
                let up = self.opts.live_changes.cycle_upscaling();
                self.relayout();
                self.overlay.message(format!("Upscaling: {}", up.name()), Duration::from_millis(1500), Instant::now());
            }
            Action::CycleFramegen => self.cycle_framegen(),
            Action::ResetColour => {
                self.opts.live_changes.reset_colour();
                self.picture_changed();
            }
            Action::Save => self.save(),
            Action::Setup => {
                if self.setup_asked() {
                    self.end.set(Outcome::Setup);
                    return true;
                }
            }
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
                let AudioSetup::Loopback(keys) = &self.opts.audio else { return false };
                keys.sync(by);
            }
            // Its message comes from the controls' thread (Notify).
            Action::Level(change) => {
                let AudioSetup::Loopback(keys) = &self.opts.audio else { return false };
                keys.level(change);
            }
            Action::Picture(colour, by) => self.adjust_colour(colour, by),
        }
        self.pacing.repaint();
        false
    }

    /// The frame policy's signal state changed: the wizard's check and the
    /// banner; the picture back after no signal (an HDMI pull) starts the
    /// repeated-refresh count over (#40).
    fn signal(&mut self, s: Signal) {
        if let Some(w) = &self.opts.wizard {
            w.borrow_mut().signal(&s);
        }
        // Back after no signal: the refreshes without frames are no repeats.
        self.pacing.signal(&s);
        self.overlay.signal = s;
    }

    /// Another thread's wake (`Notify`): a loopback that died is said
    /// (audio.rs, #42), then the messages waiting are shown.
    fn notice(&mut self) {
        if let AudioSetup::Loopback(keys) = &self.opts.audio {
            keys.wake();
        }
        let now = Instant::now();
        for (text, for_) in self.notices.lock().expect("the notices' lock is never poisoned").drain(..) {
            self.overlay.message(text, for_, now);
        }
        self.pacing.repaint();
    }

    /// The loopback runs: its delay in ms.
    fn loopback(&self) -> Option<u32> {
        match &self.opts.audio {
            AudioSetup::Loopback(keys) => match keys.status() {
                Status::Running { delay_ms, .. } => Some(delay_ms),
                Status::Stopped(_) => None,
            },
            AudioSetup::Off(_) => None,
        }
    }

    /// d: the dashboard in a terminal of its own (#16).
    fn open_dashboard(&mut self) {
        let now = Instant::now();
        // One dashboard: d again raises it, or waits for the one opening
        // (not mapped yet, so not listed).
        if self.dashboard_opened.is_some_and(|t| now.duration_since(t) < DASHBOARD_MAPS) {
            self.overlay.message("The debug dashboard is opening", Duration::from_millis(1500), now);
            self.pacing.repaint();
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
            self.pacing.repaint();
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
        self.pacing.repaint();
    }

    /// f: the next display mode, through the toolkit and then the desktop
    /// adapter, as the mpv script sets mpv's properties and then runs
    /// `view.sh --place-window`.
    fn cycle_display(&mut self) {
        let mode = self.opts.live_changes.cycle_window();
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
        self.pacing.repaint();
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
        let (mode, size) = (self.opts.live_changes.window().names().0, self.opts.live_changes.window_size());
        let _ = placer.send(Placement { mode, size, dashboard, refocus });
    }

    /// The loopback's delay while it runs: DELAY's live value.
    fn delay_ms(&self) -> Option<u32> {
        self.loopback()
    }

    /// u: whether the feed may end for the setup wizard now. Live changes
    /// not saved with w are lost to it (it starts from the saved settings),
    /// so with some, the first u says so and a second u within
    /// `SETUP_CONFIRM` goes ahead.
    fn setup_asked(&mut self) -> bool {
        let now = Instant::now();
        let changes = self.opts.live_changes.changes(self.delay_ms());
        if changes.is_empty() || self.setup_armed.is_some_and(|at| now.duration_since(at) < SETUP_CONFIRM) {
            return true;
        }
        self.setup_armed = Some(now);
        let text = format!("Not saved: {}. w saves them; u again opens the setup without them", changes.join(" "));
        self.overlay.message(text, SETUP_CONFIRM, now);
        false
    }

    /// w: what changed since the start (or the last save), as `view.sh
    /// --save` writes it.
    fn save(&mut self) {
        let now = Instant::now();
        let delay_ms = self.delay_ms();
        let result = match &self.opts.settings {
            Some(s) => self.opts.live_changes.save(s, delay_ms),
            None => Err("no settings to save to".to_string()),
        };
        match result {
            Ok(changed) if changed.is_empty() => {
                self.overlay.message("Nothing changed since the feed started", Duration::from_secs(2), now);
            }
            Ok(changed) => self.overlay.message(format!("Saved: {}", changed.join(" ")), Duration::from_secs(3), now),
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
                label: None,
            });
        }
        // Read every frame, so it follows c, a resize and frame generation
        // turning on or off.
        let label = overlay::enhancement_label(
            self.framegen.as_ref().map(|f| f.schedule.mode()),
            self.upscaling_running(),
            self.opts.live_changes.scaling(),
            self.scaled(),
        );
        if !self.overlay.visible(now) {
            // The label alone: it stays on while the overlay is hidden.
            return label.map(|l| SceneText {
                page: None,
                kind: PageKind::Panel,
                key_bar: None,
                no_signal: None,
                message: None,
                label: Some(l),
            });
        }
        let loopback = self.loopback().is_some();
        let page = match self.overlay.page {
            None => None,
            Some(Page::Tips) => {
                let id = &self.overlay.tips_id;
                Some(overlay::tips_lines(id, sources::get(id).as_ref()))
            }
            Some(Page::Panel) => {
                let changes = self.opts.live_changes.changes(self.delay_ms());
                // Upscaling's line: what it does on this monitor, or a
                // suggestion (#85); nothing while the monitor is not known.
                let setting = self.opts.live_changes.upscaling();
                let hint = self.monitor_fit().and_then(|f| overlay::upscaling_hint(setting, self.upscaling_running(), &f));
                let id = &self.overlay.tips_id;
                let p = self.opts.live_changes.picture();
                Some(overlay::panel_lines(&overlay::Panel {
                    display: self.opts.live_changes.window().names().1,
                    levels: p.levels.as_str(),
                    scaling: self.opts.live_changes.scaling().as_str(),
                    upscaling: setting.name(),
                    upscaling_hint: hint.as_deref(),
                    framegen: cfg!(feature = "dev").then(|| self.framegen.as_ref().map_or("off", |f| f.schedule.mode().as_str())),
                    colour: [p.contrast, p.brightness, p.gamma, p.saturation],
                    audio: match &self.opts.audio {
                        AudioSetup::Off(v) => overlay::Audio::Other(v.clone()),
                        AudioSetup::Loopback(keys) => match keys.status() {
                            Status::Running { delay_ms, level } => overlay::Audio::Running { level, delay_ms },
                            Status::Stopped(why) => overlay::Audio::Failed(why),
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
            label,
        })
    }

    /// When the held card frame, if any, was dequeued.
    fn held(&self) -> Option<Duration> {
        self.slot.lock().expect("the slot's lock is never poisoned").tag.map(|t| t.dequeued)
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        // Runs the readbacks' callbacks (screenshot, pass times).
        gfx.device.poll(wgpu::PollType::Poll).map_err(|e| format!("device poll: {e}"))?;
        if let Err(e) = gfx.screenshot.poll(&self.notify) {
            eprintln!("{e}");
        }
        if let Some(timer) = gfx.timer.as_mut()
            && let Some(times) = timer.take()?
        {
            self.pacing.stats().gpu(times, now_monotonic());
        }
        let Some(feedback) = gfx.feedback.as_mut() else { return Ok(()) };
        for o in feedback.drain()? {
            self.pacing.feedback(o);
        }
        Ok(())
    }

    /// Uploads the newest frame and presents it, now; paced, only once the
    /// compositor has taken the previous one (pacing.rs). Meanwhile the
    /// slot keeps only the newest frame.
    fn render(&mut self) -> Result<(), String> {
        let paced = self.gfx.as_ref().and_then(|g| g.feedback.as_ref()).is_some_and(|f| f.paces());
        let hidden = self.gfx.as_ref().is_none_or(|g| g.hidden);
        if !self.pacing.may_present(now_monotonic(), paced, hidden) {
            return Ok(());
        }
        // Built before the frame is taken: it reads the app's state.
        let scene = self.scene_text(Instant::now());
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        let (mut tag, (superseded, superseded_duplicates)) = {
            let mut s = self.slot.lock().expect("the slot's lock is never poisoned");
            let superseded = (s.superseded, s.superseded_duplicates);
            let tag = match s.tag.take() {
                Some(tag) => {
                    std::mem::swap(&mut s.data, &mut gfx.upload);
                    tag
                }
                // Nothing new from the card, but the overlay changed: the
                // last frame (black before the first) drawn again.
                None if self.pacing.repaint_wanted() => {
                    let now = now_monotonic();
                    Tag { captured: now, dequeued: now, repaint: true, kind: Kind::Duplicate, generated: false, withheld: 0 }
                }
                None => return Ok(()),
            };
            (tag, superseded)
        };
        let new_frame = !tag.repaint;
        // Frame generation (framegen.rs): what this card frame shows. A
        // duplicate withheld in new-frames-only mode is not presented,
        // unless the overlay wants drawing anyway.
        // The cadence is the capture thread's: it saw the card frames this
        // window never took (#78).
        let (show, missed) = match self.framegen.as_mut() {
            Some(fg) if new_frame => {
                let late = fg.took(superseded, superseded_duplicates);
                let step = fg.schedule.next(tag.kind, self.opts.live.cadence(), late);
                let show = if step.show == Show::Withheld && self.pacing.repaint_wanted() { Show::Real } else { step.show };
                if show == Show::Withheld {
                    self.opts.live.framegen_step(show, step.missed, tag.kind);
                    fg.withheld += 1;
                    return Ok(());
                }
                (Some(show), step.missed)
            }
            _ => (None, false),
        };

        let acquire_start = now_monotonic();
        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.pacing.stats().not_acquired();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                self.pacing.stats().not_acquired();
                return Ok(());
            }
            other => return Err(format!("get_current_texture: {other:?}")),
        };
        self.pacing.stats().acquired(now_monotonic() - acquire_start);

        let (w, h) = (self.format.width, self.format.height);
        // With frame generation a duplicate is not uploaded: the shown
        // picture comes from the new frame it repeats.
        let upload = new_frame && (self.framegen.is_none() || tag.kind == Kind::New);
        if upload {
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
        let convert = upload || (self.reconvert && gfx.have_frame);
        // Frame generation converts into its newest new frame, then shows
        // a copy of it or a generated frame in `rgb`; its frames are made
        // on first use.
        let mut fg = self.framegen.as_mut();
        if let Some(fg) = fg.as_deref_mut() {
            let mode = fg.schedule.mode();
            let gpu = fg.gpu.get_or_insert_with(|| FramegenGpu::new(&gfx.device, (w, h), mode));
            if upload {
                gpu.newest = 1 - gpu.newest;
            }
        }

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gfx.device.create_command_encoder(&Default::default());
        // Measured only with a convert pass: with frame generation a
        // duplicate has none, and its timestamps would be stale.
        if let Some(t) = gfx.timer.as_mut()
            && new_frame
            && convert
        {
            t.arm();
        }
        // A function, not a closure: each call borrows its own view.
        fn target(view: &wgpu::TextureView) -> [Option<wgpu::RenderPassColorAttachment<'_>>; 1] {
            [Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })]
        }
        if convert {
            let into = fg.as_ref().and_then(|f| f.gpu.as_ref()).map_or(&gfx.rgb_view, |g| &g.views[g.newest]);
            let attachments = target(into);
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
        if let Some(Framegen { schedule, gpu: Some(gpu), held, withheld, .. }) = fg
            && (show.is_some() || convert)
        {
            let newest = gpu.newest;
            let mut generated = false;
            if show == Some(Show::Generated)
                && let Some(generator) = gpu.generator.as_mut()
            {
                let frames = Pair { prev: &gpu.views[1 - newest], next: &gpu.views[newest], out: &gfx.rgb_view };
                let times = gfx.timer.as_mut().and_then(|t| t.generate());
                generated = generator.generate(&gfx.device, &gfx.queue, &mut encoder, frames, times);
            }
            // What this card frame showed, counted once the generator has
            // said whether it cut (#78).
            let kind = tag.kind;
            let shown = match show {
                Some(Show::Generated) if generated => {
                    *held = Some(tag);
                    tag.generated = true;
                    show
                }
                // A scene cut: the new frame now, nothing held.
                Some(Show::Generated) => Some(schedule.cut()),
                // The held new frame; on a new frame (the window a refresh
                // behind), that new frame is held in its turn.
                Some(Show::Held) => {
                    if let Some(h) = held.take() {
                        let this = std::mem::replace(&mut tag, h);
                        if upload {
                            *held = Some(this);
                        }
                    }
                    show
                }
                _ => show,
            };
            if let Some(shown) = shown {
                self.opts.live.framegen_step(shown, missed, kind);
            }
            // A held frame shown on a new frame is the previous new frame.
            let from = if show == Some(Show::Held) && upload { 1 - newest } else { newest };
            if !generated {
                let copy = |texture| wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                };
                encoder.copy_texture_to_texture(
                    copy(&gpu.frames[from]),
                    copy(&gfx.rgb),
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
            }
            tag.withheld = std::mem::take(withheld);
        }
        gfx.screenshot.copy(&mut encoder, &gfx.rgb);
        {
            // An orthogonal filter: its first pass, across the rows; FSR 1:
            // EASU into its intermediate texture.
            let two = gfx.filters.down.is_some() || gfx.sharp.up.is_some();
            let timestamps = match (&gfx.timer, two) {
                (Some(t), true) => t.writes_split(1),
                (Some(t), false) => [None, t.writes(1)],
                (None, _) => [None, None],
            };
            let [mut first, last] = timestamps;
            if let Some((between, _)) = &gfx.sharp.up {
                let attachments = target(between);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("easu"),
                    color_attachments: &attachments,
                    depth_stencil_attachment: None,
                    timestamp_writes: first.take(),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&gfx.sharp.easu);
                pass.set_bind_group(0, &gfx.sharp.easu_bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
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
            if gfx.nis.is_some() {
                pass.set_pipeline(&gfx.nis_pass.pipeline);
                pass.set_bind_group(0, &gfx.nis_pass.bind_group, &[]);
            } else if let Some((_, bind)) = &gfx.sharp.up {
                pass.set_pipeline(&gfx.sharp.rcas);
                pass.set_bind_group(0, bind, &[]);
            } else if let Some((_, bind)) = &gfx.filters.down {
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
                label: text.label.as_deref(),
                viewport: gfx.viewport.map(|v| v as f32),
            };
            let size = (gfx.config.width, gfx.config.height);
            let drawing = Instant::now();
            extra = gfx.osd.draw(&gfx.device, &gfx.queue, &mut encoder, &view, size, &s);
            self.pacing.stats().overlay(drawing.elapsed());
        }
        if let Some(t) = &gfx.timer {
            t.resolve(&mut encoder);
        }
        gfx.queue.submit(extra.into_iter().chain([encoder.finish()]));
        gfx.screenshot.submitted();
        if let Some(t) = gfx.timer.as_mut()
            && t.submitted()
        {
            self.pacing.stats().gpu_submitted();
        }

        // `present` commits the surface; feedback asked for first attaches
        // to that commit.
        if let Some(feedback) = gfx.feedback.as_mut() {
            feedback.request(tag)?;
        }
        gfx.queue.present(frame);
        self.pacing.presented(now_monotonic(), new_frame);
        if convert {
            self.reconvert = false;
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
                self.pacing.repaint();
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
                self.pacing.arrived();
                Ok(())
            }
            What::Run(Wake::Signal(s)) => {
                self.signal(s);
                Ok(())
            }
            What::Notice => {
                self.notice();
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
                        self.pacing.repaint();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.opts.wizard
            && w.borrow_mut().tick(Instant::now())
        {
            self.pacing.repaint();
        }
        // Feedback may have freed the way for a frame that had to wait: one
        // that came on this wake, a fresh one, or one overdue (the card
        // stopped). Not any held frame on the compositor's frame callback:
        // the next card frame replaces a stale one. Sending every one there
        // kept each later frame a refresh behind (kernel -> presented 55 ms,
        // not 41; the wake comes here under WaitUntil, 2026-10-16).
        let send = self.pacing.wake(now_monotonic(), self.held());
        if let Err(e) = self.drain().and_then(|()| if send { self.render() } else { Ok(()) }) {
            self.fail(event_loop, e);
        }
        // The next change of the overlay or the wizard's page, as a delay
        // from now: pacing's clock is the capture's.
        let now = Instant::now();
        let mut page = self.overlay.next_change(now);
        if let Some(w) = self.opts.wizard.as_ref().and_then(|w| w.borrow().next_change(now)) {
            page = Some(page.map_or(w, |d| d.min(w)));
        }
        let waiting = Waiting {
            page: page.map(|d| d.saturating_duration_since(now)),
            held: self.held().is_some(),
            hidden: self.gfx.as_ref().is_some_and(|g| g.hidden),
            reading: self.gfx.as_ref().is_some_and(|g| g.screenshot.reading()),
        };
        let wake = self.pacing.next_wake(now_monotonic(), waiting);
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, |in_| ControlFlow::WaitUntil(now + in_)));
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        // The last feedback, then let go of winit's display while it lives.
        if let Err(e) = self.drain() {
            self.end.set_or_warn(Outcome::Failed(e));
        }
        // A screenshot still coming back is finished, not dropped.
        if let Some(gfx) = self.gfx.as_mut() {
            gfx.screenshot.finish(&gfx.device, &self.notify);
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
        let live_changes = LiveChanges::read(&s).unwrap();
        let format = Format { width: 1920, height: 1080, fps: 60.0, pixels: crate::capture::Pixels::Yuyv };
        let mut app = App {
            window_no: 0,
            frame_wake: Arc::default(),
            format,
            slot: Arc::new(Mutex::new(Slot { data: Vec::new(), tag: None, superseded: 0, superseded_duplicates: 0 })),
            gfx: None,
            overlay: Overlay::new("ps5", Instant::now()),
            notices: Arc::default(),
            notify: Arc::new(|_| {}),
            dashboard_opened: None,
            setup_armed: None,
            reconvert: false,
            placer: None,
            modifiers: ModifiersState::empty(),
            pacing: Pacing::new(Duration::ZERO, format.fps, Stats::new(Duration::ZERO, false, Arc::new(crate::dashboard::Live::new()))),
            end: End::new(Arc::new(|_| {})),
            framegen: None,
            gpu: gpustat::Monitor::new(&dir),
            gamemode: GameMode::with(crate::gamemode::absent, false),
            adapter: None,
            device_type: None,
            monitor_size: None,
            opts: Options {
                audio: AudioSetup::Off("none".into()),
                source: "ps5".into(),
                settings: Some(s),
                ..Options::new(live_changes, Arc::new(crate::dashboard::Live::new()))
            },
        };
        app.opts.live_changes.mark_baseline(None);
        (app, file)
    }

    #[test]
    fn g_switches_frame_generation_live_and_off_after_the_last() {
        // #83: g steps through the judged modes, then off. On main none is
        // judged (copy is a test generator), so a `--framegen copy` run's
        // g turns it off; the branches' modes come first in their builds.
        let (mut app, _) = app_with("SCALING=balanced\n");
        app.adapter = Some((0x1002, 1, "a test GPU".into()));
        app.device_type = Some(wgpu::DeviceType::DiscreteGpu);
        app.framegen = Some(Framegen::new(Mode::Copy));
        app.opts.live.framegen("copy");
        app.update_gpu();
        assert!(app.gamemode.wanted());
        let mut seen = Vec::new();
        for _ in 0..=framegen::judged().len() {
            app.cycle_framegen();
            let mode = app.framegen.as_ref().map(|f| f.schedule.mode());
            seen.push(mode);
            // Each step: the dashboard, the GPU section and GameMode follow,
            // the last frame is converted again, and the overlay says it.
            assert_eq!(app.opts.live.framegen_counts().map(|c| c.0), mode.map(Mode::as_str));
            assert_eq!(app.gamemode.wanted(), mode.is_some());
            assert!(app.reconvert);
            let name = mode.map_or("off", Mode::as_str);
            assert_eq!(app.overlay.current_message(Instant::now()), Some(format!("Frame generation: {name}").as_str()));
        }
        let want: Vec<Option<Mode>> = framegen::judged().into_iter().map(Some).chain([None]).collect();
        assert_eq!(seen, want, "judged modes in order, then off");
        // An integrated GPU is refused it: a warning, and it stays off.
        if let Some(&first) = framegen::judged().first() {
            app.device_type = Some(wgpu::DeviceType::IntegratedGpu);
            app.cycle_framegen();
            assert!(app.framegen.is_none(), "{first:?} refused on an integrated GPU");
            assert!(app.overlay.current_message(Instant::now()).unwrap().starts_with("Warning: "));
        }
    }

    #[test]
    fn the_fit_follows_the_monitor() {
        let (mut app, _) = app_with("SCALING=fast\n");
        // Monitor not known: no fit line.
        assert_eq!(app.monitor_fit(), None);
        // A 4K monitor: enlarged 2x.
        app.monitor_size = Some((3840, 2160));
        let f = app.monitor_fit().unwrap();
        assert!(f.enlarged && f.reason.contains("3840x2160"), "{f:?}");
        // Moved to a 1080p monitor: pixel for pixel.
        app.monitor_size = Some((1920, 1080));
        let f = app.monitor_fit().unwrap();
        assert!(f.one_to_one && !f.enlarged, "{f:?}");
    }

    #[test]
    fn gpu_use_is_sampled_and_shown_only_with_a_gpu_enhancement() {
        // A plain run: no sampler, no GPU section (#72).
        let (mut app, _) = app_with("SCALING=balanced\n");
        app.adapter = Some((0x10de, 1, "a test GPU".into()));
        app.update_gpu();
        assert!(!app.gpu.running());
        assert!(!app.opts.live.gpu_shown());
        assert!(!app.gamemode.wanted(), "a plain run never asks GameMode (#77)");
        // Frame generation on: sampled, shown (this GPU's whole-card
        // numbers are not read: the section says why).
        app.framegen = Some(Framegen::new(Mode::Copy));
        app.update_gpu();
        assert!(app.gpu.running());
        assert!(app.opts.live.gpu_shown());
        assert!(app.gamemode.wanted());
        // Off again mid-run: stopped, gone.
        app.framegen = None;
        app.update_gpu();
        assert!(!app.gpu.running());
        assert!(!app.opts.live.gpu_shown());
        assert!(!app.gamemode.wanted());
    }

    #[test]
    fn new_only_counts_a_duplicate_replaced_in_the_slot_as_withheld() {
        let mut fg = Framegen::new(Mode::NewOnly);
        // The duplicate after a new frame came while the last present was
        // in flight; the next new frame replaced it in the slot.
        assert!(fg.took(1, 1), "late: it replaced a frame the window never took");
        assert_eq!(fg.withheld, 1);
        assert!(!fg.took(1, 1));
        assert_eq!(fg.withheld, 1, "counted once");
        // A new frame replaced (the window a refresh behind) is a drop.
        assert!(fg.took(2, 1));
        assert_eq!(fg.withheld, 1);
        // Generating modes keep withheld at 0: they present every frame.
        let mut copy = Framegen::new(Mode::Copy);
        copy.took(3, 3);
        assert_eq!(copy.withheld, 0);
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

    /// The panel's lines, without a dev build's `g` row (#83), so a line's
    /// number is the same in every build; `framegen_row` reads that row.
    fn panel(app: &App) -> Vec<String> {
        app.scene_text(Instant::now())
            .and_then(|t| t.page)
            .expect("the panel is open")
            .into_iter()
            .filter(|l| !l.starts_with("g     Framegen"))
            .collect()
    }

    /// The panel's row whose label (after the key column) starts so.
    fn row(app: &App, label: &str) -> String {
        panel(app).into_iter().find(|l| l.get(6..).is_some_and(|r| r.starts_with(label))).unwrap_or_else(|| panic!("no {label} row"))
    }

    fn framegen_row(app: &App) -> Option<String> {
        app.scene_text(Instant::now())
            .and_then(|t| t.page)
            .expect("the panel is open")
            .into_iter()
            .find(|l| l.starts_with("g     Framegen"))
    }

    /// The app with a stand-in loopback started at 50 ms (#29), its
    /// overlay messages taken on `notice` as the window takes them.
    /// `config`: a different one per test, as each names its temp dir.
    fn app_with_loopback(config: &'static str) -> (App, crate::audio::Loopback, crate::audio::fake::Script, std::path::PathBuf) {
        let (mut app, file) = app_with(config);
        let (loopback, script) = crate::audio::fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let notices = app.notices.clone();
        keys.set_notify(Arc::new(move |m| notices.lock().unwrap().extend(m)));
        app.opts.audio = AudioSetup::Loopback(keys);
        (app, loopback, script, file)
    }

    fn keys(app: &App) -> LoopbackKeys {
        let AudioSetup::Loopback(keys) = &app.opts.audio else { panic!("no loopback") };
        keys.clone()
    }

    fn audio_line(app: &mut App) -> String {
        if app.overlay.page.is_none() {
            press(app, "Tab");
        }
        row(app, "Audio        ")
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

    /// u, as the window takes it: an action that ends the window.
    fn u(app: &mut App) -> bool {
        let action = keys::action(&Key::Character("u".into()), ModifiersState::empty(), false, app.key_mode()).expect("u in the overlay");
        app.apply(action)
    }

    #[test]
    fn u_ends_the_feed_for_the_setup_and_asks_first_over_unsaved_changes() {
        let (mut app, _file) = app_with("# setup key\n");
        assert!(press(&mut app, "Tab"));
        // Nothing unsaved: u ends the window, for the setup.
        assert!(u(&mut app));
        assert!(app.end.is_set());

        let (mut app, _file) = app_with("# setup key, unsaved\n");
        assert!(press(&mut app, "Tab"));
        assert!(press(&mut app, "l"));
        // Unsaved: the first u says so and the feed goes on.
        assert!(!u(&mut app));
        assert!(!app.end.is_set());
        assert_eq!(
            app.overlay.current_message(Instant::now()),
            Some("Not saved: LEVELS=limited. w saves them; u again opens the setup without them")
        );
        // The second, within SETUP_CONFIRM, goes ahead.
        assert!(u(&mut app));
        assert!(app.end.is_set());
    }

    #[test]
    fn the_audio_keys_and_the_panel_go_through_the_loopback() {
        // What happens to the loopback is audio.rs's (#42): here, that the
        // window passes the keys and the wake on, and shows its status.
        let (mut app, _loopback, script, file) = app_with_loopback("DELAY=0.05\n# audio keys\n");
        assert_eq!(app.loopback(), Some(50));
        crate::audio::fake::kill(&keys(&app));
        assert_eq!(app.loopback(), None, "stopped before its wake is handled");
        app.notice();
        assert_eq!(
            app.overlay.current_message(Instant::now()),
            Some("Audio stopped (output: Device disconnected): ctrl -/+, m, 9 or 0 start it again")
        );
        assert_eq!(audio_line(&mut app), "m 9 0 Audio        not running, see the log: output: Device disconnected");
        assert!(!app.apply(Action::Sync(10)));
        app.notice();
        assert_eq!(*script.starts.borrow(), [50, 60]);
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Audio sync 60 ms"));
        assert_eq!(row(&app, "Audio sync"), "^-/^+ Audio sync   60 ms   (alt -/+: 50 ms steps)");
        assert!(!app.apply(Action::Level(LevelChange::Mute)));
        assert_eq!(script.starts.borrow().len(), 2, "a running one is not started again");
        // AUDIO=none: the keys do nothing.
        app.opts.audio = AudioSetup::Off("none".into());
        assert!(!app.apply(Action::Sync(10)));
        assert_eq!(script.starts.borrow().len(), 2);
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn overlay_keys_and_w_save_through_the_settings() {
        let (mut app, file) = app_with("# mine\nLEVELS=auto\nCONTRAST=2\n");
        assert!(!press(&mut app, "l"), "l only with the overlay open");
        assert!(press(&mut app, "Tab"));
        // A dev build names frame generation under Upscaling (#83, #85); a
        // release build has no row.
        assert_eq!(framegen_row(&app), cfg!(feature = "dev").then(|| "g     Framegen     off".to_string()));
        assert_eq!(row(&app, "Upscaling"), "c     Upscaling    off");
        assert_eq!(row(&app, "Resizing"), "      Resizing     balanced");
        assert_eq!(row(&app, "Levels"), "l     Levels       auto");
        assert_eq!(row(&app, "Colour"), "1-8   Colour       contrast 2 · brightness 0 · gamma 0 · saturation 0");
        assert_eq!(row(&app, "Audio        "), "      Audio        none");
        assert_eq!(row(&app, "Setup tips"), "t     Setup tips   PlayStation 5");
        assert_eq!(row(&app, "Save"), "w     Save         nothing changed");

        assert!(press(&mut app, "l"));
        // c: Upscaling's next method (#85).
        assert!(press(&mut app, "c"));
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Upscaling: AMD FSR 1"));
        assert_eq!(row(&app, "Upscaling"), "c     Upscaling    AMD FSR 1");
        assert_eq!(row(&app, "Save"), "w     Save         UPSCALING=fsr LEVELS=limited", "settings.rows' order");
        assert!(press(&mut app, "w"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# mine\nLEVELS=limited\nCONTRAST=2\nUPSCALING=fsr\n");
        assert_eq!(app.overlay.current_message(Instant::now()), Some("Saved: UPSCALING=fsr LEVELS=limited"));
        assert_eq!(row(&app, "Save"), "w     Save         nothing changed", "the save is the new baseline");
        assert!(press(&mut app, "r"));
        assert_eq!(row(&app, "Save"), "w     Save         CONTRAST=0");
        assert!(press(&mut app, "t"));
        assert_eq!(app.overlay.page, Some(Page::Tips));
        assert!(press(&mut app, "Esc"));
        assert_eq!(app.overlay.page, None);
        assert!(app.pacing.repaint_wanted(), "every change draws again");
        app.pacing.drew();
        app.reconvert = false;
        assert!(press(&mut app, "2"));
        assert!(app.pacing.repaint_wanted() && app.reconvert, "a colour key redraws a frozen frame converted again");

        // f: the panel's Display line and w's WINDOW follow it.
        press(&mut app, "Tab");
        press(&mut app, "f");
        assert_eq!(row(&app, "Display"), "f     Display      Borderless");
        assert!(row(&app, "Save").contains("WINDOW=borderless"));
        press(&mut app, "f");
        assert_eq!(app.opts.live_changes.window(), WindowMode::Windowed { width: 1280, height: 720 });
        press(&mut app, "f");
        assert_eq!(app.opts.live_changes.window(), WindowMode::Fullscreen);
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
    fn letterbox_in_whole_pixels() {
        assert_eq!(letterbox((1920, 1200), (1920, 1080)), [0, 60, 1920, 1080], "1:1, bars above and below");
        assert_eq!(letterbox((2560, 1440), (1920, 1080)), [0, 0, 2560, 1440]);
        assert_eq!(letterbox((3440, 1440), (1920, 1080)), [440, 0, 2560, 1440]);
        assert_eq!(letterbox((950, 1127), (1920, 1080)), [0, 297, 950, 534]);
    }

    #[test]
    fn window_modes_from_the_settings() {
        assert_eq!(WindowMode::from_settings("fullscreen", "1280x720"), Ok(WindowMode::Fullscreen));
        assert_eq!(WindowMode::from_settings("windowed", "1280x720"), Ok(WindowMode::Windowed { width: 1280, height: 720 }));
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
