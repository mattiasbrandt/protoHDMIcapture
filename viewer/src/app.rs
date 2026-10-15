//! The window: wgpu draws each frame the moment it arrives (not on a redraw
//! request, which Wayland throttles), YUYV -> RGB at the capture's own size
//! and then scaled into a letterboxed viewport (ADR 0001), handed to the
//! compositor only once it has taken the previous one (#5: a 60.000 Hz card
//! outruns a 59.95 Hz display, and every frame handed over early queues).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::Controls;
use crate::capture::{Event, Format, FrameSource, now_monotonic};
use crate::desktop::Desktop;
use crate::gputime::GpuTimer;
use crate::osd::{Osd, Scene};
use crate::overlay::{self, Notify, Overlay, Page};
use crate::picture::{self, Colour, Picture};
use crate::present::{Feedback, Outcome, Presentation, Tag};
use crate::pw::LevelChange;
use crate::scale::{self, Filter, Scaling};
use crate::screenshot::Screenshot;
use crate::settings::Settings;
use crate::sources;
use crate::stats::Stats;

/// A frame callback this late counts as lost: pacing stops waiting for it.
const PACE_TIMEOUT: Duration = Duration::from_millis(100);
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
    /// Quit after this long (measurements).
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
}

/// The sound, as the audio keys and the panel see it.
pub enum AudioSetup {
    /// AUDIO is not loopback: its value.
    Off(String),
    /// The loopback did not start: why.
    Failed(String),
    Running(Controls),
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

#[derive(Debug, PartialEq)]
enum AudioAction {
    /// Move the sync by this many ms.
    Sync(i32),
    Level(LevelChange),
}

/// The audio keys as the mpv script binds them: ctrl -/+ (or =) 10 ms,
/// alt -/+ 50 ms, and m, 9, 0 plain (no ctrl, alt or super).
fn audio_action(key: &Key, m: ModifiersState) -> Option<AudioAction> {
    let Key::Character(c) = key else { return None };
    if m.super_key() {
        return None;
    }
    let step = match (m.control_key(), m.alt_key()) {
        (true, false) => 10,
        (false, true) => 50,
        (false, false) => 0,
        (true, true) => return None,
    };
    Some(match (c.as_str(), step) {
        ("-", s) if s > 0 => AudioAction::Sync(-s),
        ("+" | "=", s) if s > 0 => AudioAction::Sync(s),
        ("m", 0) => AudioAction::Level(LevelChange::Mute),
        ("9", 0) => AudioAction::Level(LevelChange::Volume(-5)),
        ("0", 0) => AudioAction::Level(LevelChange::Volume(5)),
        _ => return None,
    })
}

/// The overlay's text for one frame (osd.rs draws it).
struct SceneText {
    page: Option<Vec<String>>,
    key_bar: Option<String>,
    no_signal: Option<String>,
    message: Option<String>,
}

enum UserEvent {
    Frame,
    /// A thread has something to show (`Notify`).
    Notice,
    Quit,
}

/// The newest captured frame, waiting for the display; one replaced before
/// the display took it is skipped (not a capture gap).
struct Slot {
    data: Vec<u8>,
    tag: Option<Tag>,
    superseded: u64,
}

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
    filter_pipeline: wgpu::RenderPipeline,
    filter_bind_group: wgpu::BindGroup,
    filter_buffer: wgpu::Buffer,
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
    /// LEVELS or a colour value changed: convert the last frame again on a
    /// repaint, so a frozen feed shows it too.
    reconvert: bool,
    /// When the overlay next changes on its own.
    deadline: Option<Instant>,
    /// The window placement worker, once started.
    placer: Option<std::sync::mpsc::Sender<(&'static str, (u32, u32))>>,
    modifiers: ModifiersState,
    stats: Stats,
    in_flight: u32,
    last_submit: Duration,
    error: Option<String>,
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

/// Ctrl-C, SIGTERM and SIGHUP (ctrlc's termination feature), installed by
/// main before anything is switched, so a signal at any point ends in the
/// normal teardown (the display switched back, the run dir cleaned) rather
/// than leaving it to the next run's recovery.
type Wake = Box<dyn Fn() + Send>;

#[derive(Clone)]
pub struct Quit {
    flag: Arc<AtomicBool>,
    /// Wakes the window's loop, once it runs.
    wake: Arc<Mutex<Option<Wake>>>,
}

impl Quit {
    pub fn install() -> Result<Self, String> {
        let q = Self { flag: Arc::default(), wake: Arc::default() };
        let handler = q.clone();
        ctrlc::set_handler(move || handler.request()).map_err(|e| format!("signal handler: {e}"))?;
        Ok(q)
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::Relaxed);
        if let Some(wake) = self.wake.lock().expect("the quit lock is never poisoned").as_ref() {
            wake();
        }
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn on_request(&self, wake: Wake) {
        *self.wake.lock().expect("the quit lock is never poisoned") = Some(wake);
    }
}

/// Runs the window until quit; the capture thread feeds it. Returns the
/// capture's error, if it failed.
pub fn run(mut source: Box<dyn FrameSource>, opts: Options, quitter: Quit) -> Result<(), String> {
    let format = source.format();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    let quit = quitter.flag.clone();
    {
        let p = proxy.clone();
        quitter.on_request(Box::new(move || {
            let _ = p.send_event(UserEvent::Quit);
        }));
    }
    // A signal before the loop's wake was set.
    if quitter.requested() {
        let _ = proxy.send_event(UserEvent::Quit);
    }
    let stopper = || {
        let q = quitter.clone();
        move || q.request()
    };
    if let Some(s) = opts.seconds {
        let stop = stopper();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(s));
            stop();
        });
    }

    let slot = Arc::new(Mutex::new(Slot { data: vec![0; format.frame_size()], tag: None, superseded: 0 }));
    let start = now_monotonic();
    let capture_thread = {
        let slot = slot.clone();
        let quit = quit.clone();
        let stop = stopper();
        let live = crate::dashboard::live();
        thread::spawn(move || -> Result<(u64, u64), String> {
            let mut frames = 0u64;
            let mut gaps = 0u64;
            let result = source.run(&quit, &mut |e| match e {
                Event::Frame(f) => {
                    frames += 1;
                    live.frame();
                    {
                        let mut s = slot.lock().expect("the slot's lock is never poisoned");
                        if s.tag.is_some() {
                            s.superseded += 1;
                        }
                        s.data.copy_from_slice(f.data);
                        s.tag = Some(Tag { captured: f.captured, dequeued: f.dequeued, repaint: false });
                    }
                    let _ = proxy.send_event(UserEvent::Frame);
                }
                // A gap before the first good frame is the card's start-up.
                Event::Gap { .. } if frames == 0 => {}
                Event::Gap { from, to, lost } => {
                    gaps += 1;
                    live.gap(from, to, lost);
                    eprintln!("capture gap: sequence {from} -> {to}, {lost} lost");
                }
                Event::NoSignal => {
                    live.signal(false);
                    eprintln!("no signal");
                }
                Event::SignalBack => {
                    live.signal(true);
                    eprintln!("signal back");
                }
            });
            stop();
            result.map(|()| (frames, gaps)).map_err(|e| format!("capture: {e}"))
        })
    };

    let notices: Arc<Mutex<Vec<(String, Duration)>>> = Arc::default();
    let notify: Notify = {
        let (n, p) = (notices.clone(), event_loop.create_proxy());
        Arc::new(move |message| {
            if let Some(m) = message {
                n.lock().expect("the notices' lock is never poisoned").push(m);
            }
            let _ = p.send_event(UserEvent::Notice);
        })
    };
    if let AudioSetup::Running(c) = &opts.audio {
        c.set_notify(notify.clone());
    }
    let mut app = App {
        format,
        stats: Stats::new(start, opts.stats),
        picture: opts.picture,
        scaling: opts.scaling,
        overlay: Overlay::new(&opts.source, Instant::now()),
        baseline: overlay::Values::new(),
        notices,
        notify,
        repaint: false,
        reconvert: false,
        deadline: None,
        placer: None,
        modifiers: ModifiersState::empty(),
        opts,
        slot: slot.clone(),
        gfx: None,
        in_flight: 0,
        last_submit: start,
        error: None,
    };
    app.baseline = app.values();
    let loop_result = event_loop.run_app(&mut app);
    quit.store(true, Ordering::Relaxed);
    let captured = capture_thread.join().map_err(|_| "the capture thread panicked".to_string())?;

    if app.opts.stats {
        let superseded = slot.lock().expect("the slot's lock is never poisoned").superseded;
        app.stats.summary(superseded);
        if let Ok((frames, gaps)) = &captured {
            eprintln!("summary: captured {frames} frames, {gaps} capture gaps");
        }
    }
    loop_result.map_err(|e| format!("event loop: {e}"))?;
    if let Some(e) = app.error {
        return Err(e);
    }
    captured.map(|_| ())
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<Gfx, String> {
        let (w, h) = (self.format.width, self.format.height);
        let mut attrs = Window::default_attributes()
            .with_title("protoHDMIcapture")
            .with_name("protohdmicapture", "protohdmicapture");
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

        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
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
        if self.opts.stats {
            eprintln!(
                "surface: {}x{} {:?}, present mode {:?}, paced {}",
                config.width,
                config.height,
                config.format,
                config.present_mode,
                feedback.is_some()
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
        let filter_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("filter"),
            size: scale::UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let filter_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("filter"),
            layout: &filter_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&rgb_view) },
                wgpu::BindGroupEntry { binding: 1, resource: filter_buffer.as_entire_binding() },
            ],
        });
        let filter_pipeline = pipeline(&device, "filter", scale::WGSL, &filter_layout, config.format);

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
            filter_pipeline,
            filter_bind_group,
            filter_buffer,
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
        if let Some(f) = &gfx.filter {
            gfx.queue.write_buffer(&gfx.filter_buffer, 0, &f.bytes(frame));
        }
        if self.opts.stats {
            eprintln!(
                "scaling: {} -> {vw}x{vh}, {}",
                self.scaling.as_str(),
                gfx.filter.as_ref().map_or("bilinear sampler", |f| f.name)
            );
        }
    }

    /// The audio keys, as view.sh binds them with AUDIO=loopback: sync with
    /// ctrl -/+ (10 ms) and alt -/+ (50 ms), m mute, 9/0 volume -5/+5.
    /// True when the key was one of them.
    fn audio_key(&mut self, key: &Key) -> bool {
        let AudioSetup::Running(audio) = &self.opts.audio else { return false };
        let Some(action) = audio_action(key, self.modifiers) else { return false };
        let synced = match action {
            AudioAction::Sync(ms) => Some(audio.nudge_delay(ms)),
            // Their messages come from the controls' thread (Notify).
            AudioAction::Level(change) => {
                audio.change_level(change);
                None
            }
        };
        if let Some(ms) = synced {
            self.overlay.message(format!("Audio sync {ms} ms"), Duration::from_millis(1500), Instant::now());
        }
        self.repaint = true;
        true
    }

    /// The picture keys (mpv's 1..8; the overlay's r and l). The next frame
    /// shows the change.
    fn picture_key(&mut self, key: &Key) {
        let Key::Character(c) = key else { return };
        // A plain key only: ctrl/alt/super with a digit belong to the
        // desktop, or to other controls.
        if self.modifiers.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER) {
            return;
        }
        let (colour, by) = match c.as_str() {
            "1" => (Colour::Contrast, -1),
            "2" => (Colour::Contrast, 1),
            "3" => (Colour::Brightness, -1),
            "4" => (Colour::Brightness, 1),
            "5" => (Colour::Gamma, -1),
            "6" => (Colour::Gamma, 1),
            "7" => (Colour::Saturation, -1),
            "8" => (Colour::Saturation, 1),
            _ => return,
        };
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

    /// Tab, and the keys that only mean something while the overlay is
    /// open, as the mpv script binds them. True when the key was taken.
    fn overlay_key(&mut self, key: &Key) -> bool {
        let open = self.overlay.page.is_some();
        match key {
            Key::Named(NamedKey::Tab) => self.overlay.toggle(),
            _ if !open => return false,
            Key::Named(NamedKey::Escape) => self.overlay.page = None,
            Key::Named(NamedKey::ArrowLeft) if self.overlay.page == Some(Page::Tips) => self.overlay.cycle_source(-1),
            Key::Named(NamedKey::ArrowRight) if self.overlay.page == Some(Page::Tips) => self.overlay.cycle_source(1),
            Key::Character(c) => match c.as_str() {
                "l" => {
                    self.picture.cycle_levels();
                    self.picture_changed();
                }
                "c" => {
                    self.scaling = self.scaling.next();
                    self.relayout();
                }
                "r" => {
                    self.picture.reset_colour();
                    self.picture_changed();
                }
                "w" => self.save(),
                "t" => self.overlay.toggle_tips(),
                _ => return false,
            },
            _ => return false,
        }
        self.repaint = true;
        true
    }

    /// d: the dashboard in a terminal of its own (#16).
    fn open_dashboard(&mut self) {
        let now = Instant::now();
        match crate::dashboard::open(Some(self.notify.clone())) {
            Ok(()) => self.overlay.message("Opening the debug dashboard", Duration::from_millis(1500), now),
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
        if matches!(self.opts.desktop, Desktop::Generic) {
            return;
        }
        let placer = self.placer.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::channel::<(&'static str, (u32, u32))>();
            let desktop = self.opts.desktop.clone();
            thread::spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    match desktop.place_window(std::process::id(), job.0, job.1) {
                        Ok(m) => eprintln!("{m}"),
                        Err(m) => eprintln!("warning: {m}"),
                    }
                }
            });
            tx
        });
        let _ = placer.send((self.opts.window.names().0, self.opts.window_size));
    }

    /// The saveable settings' live values, spelled as the settings file
    /// spells them (DELAY in seconds, as the mpv script writes it).
    fn values(&self) -> overlay::Values {
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
        if let AudioSetup::Running(a) = &self.opts.audio {
            v.insert("DELAY", format!("{:.3}", f64::from(a.delay_ms()) / 1000.0));
        }
        v
    }

    fn saveable(&self) -> Vec<&'static str> {
        let Some(s) = &self.opts.settings else { return Vec::new() };
        let now = self.values();
        // Static names: the keys of `values`, in lib/settings.sh's order.
        s.rows.iter().filter(|r| r.has_flag("save")).filter_map(|r| now.keys().find(|k| **k == r.name).copied()).collect()
    }

    /// w: what changed since the start (or the last save), as `view.sh
    /// --save` writes it.
    fn save(&mut self) {
        let now = Instant::now();
        let values = self.values();
        let loopback = matches!(self.opts.audio, AudioSetup::Running(_));
        let changed = overlay::changes(&self.saveable(), &self.baseline, &values, loopback);
        if changed.is_empty() {
            self.overlay.message("Nothing changed since the feed started", Duration::from_secs(2), now);
            return;
        }
        // The wizard's test feed reads a preview config the wizard deletes.
        let result = if std::env::var_os("PROTOHDMI_SETUP_CHILD").is_some_and(|v| !v.is_empty()) {
            Err("this is the wizard's test feed; change settings in the wizard".to_string())
        } else if let Some(s) = &self.opts.settings {
            let pairs: Vec<(&str, &str)> = changed.iter().filter_map(|kv| kv.split_once('=')).collect();
            s.save(&pairs)
        } else {
            Err("no settings to save to".to_string())
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
        if !self.overlay.visible(now) {
            return None;
        }
        let loopback = matches!(self.opts.audio, AudioSetup::Running(_));
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
                        AudioSetup::Failed(why) => overlay::Audio::Failed(why.clone()),
                        AudioSetup::Running(a) => {
                            overlay::Audio::Running { level: a.current_level(), delay_ms: a.delay_ms() }
                        }
                    },
                    tips_name: sources::get(id).map_or_else(|| id.clone(), |s| s.name),
                    changes: &changes,
                }))
            }
        };
        Some(SceneText {
            page,
            key_bar: self.overlay.key_bar_showing(now).then(|| overlay::key_bar(loopback)),
            no_signal: self.overlay.no_signal(now),
            message: self.overlay.current_message(now).map(str::to_string),
        })
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
                Outcome::Presented { tag, at, refresh, flags } => self.stats.presented(tag, at, refresh, flags),
                Outcome::Discarded => self.stats.discarded(),
                Outcome::FrameDone => self.in_flight = self.in_flight.saturating_sub(1),
            }
        }
        Ok(())
    }

    /// Uploads the newest frame and presents it, now; paced, only once the
    /// compositor has taken the previous one (its frame callback). Meanwhile
    /// the slot keeps only the newest frame.
    fn render(&mut self) -> Result<(), String> {
        let paced = self.gfx.as_ref().is_some_and(|g| g.feedback.is_some());
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
            let attachments = target(&view);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scale"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: gfx.timer.as_ref().and_then(|t| t.writes(1)),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [x, y, vw, vh] = gfx.viewport.map(|v| v as f32);
            pass.set_viewport(x, y, vw, vh, 0.0, 1.0);
            if gfx.filter.is_some() {
                pass.set_pipeline(&gfx.filter_pipeline);
                pass.set_bind_group(0, &gfx.filter_bind_group, &[]);
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
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let result = match event {
            UserEvent::Frame => {
                self.overlay.signal.frame(Instant::now());
                self.render()
            }
            UserEvent::Notice => {
                let now = Instant::now();
                for (text, for_) in self.notices.lock().expect("the notices' lock is never poisoned").drain(..) {
                    self.overlay.message(text, for_, now);
                }
                self.repaint = true;
                self.render()
            }
            UserEvent::Quit => {
                event_loop.exit();
                Ok(())
            }
        };
        if let Err(e) = result {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let key = &event.logical_key;
                let plain = !self.modifiers.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER);
                let is = |k: &str| matches!(key, Key::Character(c) if c == k);
                // Held down, these act once (the mpv script binds Tab, f
                // and m without repeat; s is a shot per press); the rest
                // repeat, the overlay's keys included, as there.
                let once = matches!(key, Key::Named(NamedKey::Tab)) || ["f", "m", "s", "d"].into_iter().any(is);
                // q quits, as in view.sh.
                if is("q") {
                    event_loop.exit();
                } else if (event.repeat && once) || (plain && self.overlay_key(key)) {
                    // Ignored, or taken by the overlay.
                } else if plain && is("f") {
                    self.cycle_display();
                } else if plain && is("d") {
                    self.open_dashboard();
                } else if plain && is("s") {
                    if let Some(gfx) = self.gfx.as_mut()
                        && !gfx.screenshot.request()
                    {
                        eprintln!("screenshot: the last one is still being saved");
                    }
                    // Without frames from the card, a repaint takes the shot.
                    self.repaint = true;
                } else if !self.audio_key(key) {
                    self.picture_key(key);
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
        // Feedback may have freed the way for a frame that had to wait.
        if let Err(e) = self.drain().and_then(|()| self.render()) {
            self.error = Some(e);
            event_loop.exit();
        }
        // Wake for the next change, and while a repaint waits on pacing
        // (no frame from the card may come to wake us); not while the
        // window has no area, which nothing is drawn into.
        let now = Instant::now();
        self.deadline = self.overlay.next_change(now);
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
            self.error.get_or_insert(e);
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

    /// An app with no window, its settings in a temp file.
    fn app_with(config: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("phc-app-{}-{:p}", std::process::id(), config));
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
            reconvert: false,
            deadline: None,
            placer: None,
            modifiers: ModifiersState::empty(),
            stats: Stats::new(Duration::ZERO, false),
            in_flight: 0,
            last_submit: Duration::ZERO,
            error: None,
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
            },
        };
        app.baseline = app.values();
        (app, file)
    }

    fn press(app: &mut App, k: &str) -> bool {
        let key = match k {
            "Tab" => Key::Named(NamedKey::Tab),
            "Esc" => Key::Named(NamedKey::Escape),
            c => Key::Character(c.into()),
        };
        app.overlay_key(&key)
    }

    fn panel(app: &App) -> Vec<String> {
        app.scene_text(Instant::now()).and_then(|t| t.page).expect("the panel is open")
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
        // view.sh reads what w wrote as saved and valid.
        let out = std::process::Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../view.sh"))
            .arg("--settings")
            .env("CONFIG_FILE", &file)
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&out.stdout);
        let row = |k: &str| report.lines().find(|l| l.split_whitespace().next() == Some(k)).unwrap_or("").split_whitespace().collect::<Vec<_>>();
        assert_eq!(row("LEVELS"), ["LEVELS", "limited", "saved"], "{report}");
        assert_eq!(row("SCALING"), ["SCALING", "fast", "saved"], "{report}");
        assert!(out.status.success() && report.contains("all valid"), "{report}");

        assert!(press(&mut app, "r"));
        assert_eq!(panel(&app)[9], "w     Save         CONTRAST=0");
        assert!(press(&mut app, "t"));
        assert_eq!(app.overlay.page, Some(Page::Tips));
        assert!(press(&mut app, "Esc"));
        assert_eq!(app.overlay.page, None);
        assert!(app.repaint, "every change draws again");
        app.repaint = false;
        app.reconvert = false;
        app.picture_key(&Key::Character("2".into()));
        assert!(app.repaint && app.reconvert, "a colour key redraws a frozen frame converted again");

        // f: the panel's Display line and w's WINDOW follow it.
        press(&mut app, "Tab");
        app.cycle_display();
        assert_eq!(panel(&app)[2], "f     Display      Borderless");
        assert!(panel(&app)[9].contains("WINDOW=borderless"));
        app.cycle_display();
        assert_eq!(app.opts.window, WindowMode::Windowed { width: 1280, height: 720 });
        app.cycle_display();
        assert_eq!(app.opts.window, WindowMode::Fullscreen);
        press(&mut app, "Esc");
        app.cycle_display();
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
    fn audio_keys_as_the_mpv_script_binds_them() {
        let k = |c: &str| Key::Character(c.into());
        let (ctrl, alt, none) = (ModifiersState::CONTROL, ModifiersState::ALT, ModifiersState::empty());
        assert_eq!(audio_action(&k("-"), ctrl), Some(AudioAction::Sync(-10)));
        assert_eq!(audio_action(&k("="), ctrl), Some(AudioAction::Sync(10)));
        assert_eq!(audio_action(&k("+"), alt | ModifiersState::SHIFT), Some(AudioAction::Sync(50)));
        assert_eq!(audio_action(&k("m"), none), Some(AudioAction::Level(LevelChange::Mute)));
        assert_eq!(audio_action(&k("0"), none), Some(AudioAction::Level(LevelChange::Volume(5))));
        // Plain keys only, as the review found ctrl+alt+m muting.
        assert_eq!(audio_action(&k("m"), ctrl | alt), None);
        assert_eq!(audio_action(&k("9"), ctrl), None);
        assert_eq!(audio_action(&k("-"), none), None);
        assert_eq!(audio_action(&k("-"), ctrl | ModifiersState::SUPER), None);
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
}
