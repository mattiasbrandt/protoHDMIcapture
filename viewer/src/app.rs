//! The window: wgpu draws each frame the moment it arrives (not on a redraw
//! request, which Wayland throttles), YUYV -> RGB at the capture's own size
//! and then scaled into a letterboxed viewport (ADR 0001), handed to the
//! compositor only once it has taken the previous one (#5: a 60.000 Hz card
//! outruns a 59.95 Hz display, and every frame handed over early queues).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::Controls;
use crate::capture::{Event, Format, FrameSource, now_monotonic};
use crate::gputime::GpuTimer;
use crate::picture::{self, Colour, Picture};
use crate::present::{Feedback, Outcome, Presentation, Tag};
use crate::pw::LevelChange;
use crate::scale::{self, Filter, Scaling};
use crate::stats::Stats;

/// A frame callback this late counts as lost: pacing stops waiting for it.
const PACE_TIMEOUT: Duration = Duration::from_millis(100);

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
    /// The audio keys, when the loopback runs (AUDIO=loopback).
    pub audio: Option<Controls>,
}

enum UserEvent {
    Frame,
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
    rgb_view: wgpu::TextureView,
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

/// Runs the window until quit; the capture thread feeds it. Returns the
/// capture's error, if it failed.
pub fn run(mut source: Box<dyn FrameSource>, opts: Options) -> Result<(), String> {
    let format = source.format();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    let quit = Arc::new(AtomicBool::new(false));

    let wake = |proxy: EventLoopProxy<UserEvent>, quit: Arc<AtomicBool>| {
        move || {
            quit.store(true, Ordering::Relaxed);
            let _ = proxy.send_event(UserEvent::Quit);
        }
    };
    ctrlc::set_handler(wake(proxy.clone(), quit.clone())).map_err(|e| e.to_string())?;
    if let Some(s) = opts.seconds {
        let stop = wake(proxy.clone(), quit.clone());
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
        let stop = wake(proxy.clone(), quit.clone());
        thread::spawn(move || -> Result<(u64, u64), String> {
            let mut frames = 0u64;
            let mut gaps = 0u64;
            let result = source.run(&quit, &mut |e| match e {
                Event::Frame(f) => {
                    frames += 1;
                    {
                        let mut s = slot.lock().expect("the slot's lock is never poisoned");
                        if s.tag.is_some() {
                            s.superseded += 1;
                        }
                        s.data.copy_from_slice(f.data);
                        s.tag = Some(Tag { captured: f.captured, dequeued: f.dequeued });
                    }
                    let _ = proxy.send_event(UserEvent::Frame);
                }
                // A gap before the first good frame is the card's start-up.
                Event::Gap { .. } if frames == 0 => {}
                Event::Gap { from, to, lost } => {
                    gaps += 1;
                    eprintln!("capture gap: sequence {from} -> {to}, {lost} lost");
                }
                Event::NoSignal => eprintln!("no signal"),
                Event::SignalBack => eprintln!("signal back"),
            });
            stop();
            result.map(|()| (frames, gaps)).map_err(|e| format!("capture: {e}"))
        })
    };

    let mut app = App {
        format,
        stats: Stats::new(start, opts.stats),
        picture: opts.picture,
        scaling: opts.scaling,
        modifiers: ModifiersState::empty(),
        opts,
        slot: slot.clone(),
        gfx: None,
        in_flight: 0,
        last_submit: start,
        error: None,
    };
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
        let rgb = frame_texture("rgb", w, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT);
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
        queue.write_buffer(&picture_buffer, 0, &picture::bytes(&self.picture.uniform(w, h)));
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

        Ok(Gfx {
            window,
            surface,
            device,
            queue,
            config,
            convert_pipeline,
            convert_bind_group,
            picture_buffer,
            rgb_view,
            scale_pipeline,
            scale_bind_group,
            filter_pipeline,
            filter_bind_group,
            filter_buffer,
            filter: None,
            viewport: [0, 0, 1, 1],
            timer,
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
        let Some(audio) = &self.opts.audio else { return false };
        let Key::Character(c) = key else { return false };
        let m = self.modifiers;
        let step = if m.control_key() && !m.alt_key() {
            10
        } else if m.alt_key() && !m.control_key() {
            50
        } else {
            0
        };
        if m.super_key() {
            return false;
        }
        match (c.as_str(), step) {
            ("-", s) if s > 0 => eprintln!("audio sync {} ms", audio.nudge_delay(-s)),
            ("+" | "=", s) if s > 0 => eprintln!("audio sync {} ms", audio.nudge_delay(s)),
            ("m", 0) => audio.change_level(LevelChange::Mute),
            ("9", 0) => audio.change_level(LevelChange::Volume(-5)),
            ("0", 0) => audio.change_level(LevelChange::Volume(5)),
            _ => return false,
        }
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
        let p = &mut self.picture;
        match c.as_str() {
            "1" => p.adjust(Colour::Contrast, -1),
            "2" => p.adjust(Colour::Contrast, 1),
            "3" => p.adjust(Colour::Brightness, -1),
            "4" => p.adjust(Colour::Brightness, 1),
            "5" => p.adjust(Colour::Gamma, -1),
            "6" => p.adjust(Colour::Gamma, 1),
            "7" => p.adjust(Colour::Saturation, -1),
            "8" => p.adjust(Colour::Saturation, 1),
            // Overlay keys in view.sh; global until the overlay exists (#12).
            "r" => p.reset_colour(),
            "l" => p.cycle_levels(),
            // The overlay's c, global until the overlay exists (#12).
            "c" => {
                self.scaling = self.scaling.next();
                self.relayout();
                return;
            }
            _ => return,
        }
        if let Some(gfx) = &self.gfx {
            let u = self.picture.uniform(self.format.width, self.format.height);
            gfx.queue.write_buffer(&gfx.picture_buffer, 0, &picture::bytes(&u));
        }
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        if let Some(timer) = gfx.timer.as_mut() {
            gfx.device.poll(wgpu::PollType::Poll).map_err(|e| format!("device poll: {e}"))?;
            if let Some([convert, scale]) = timer.take()? {
                self.stats.gpu(convert, scale);
            }
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
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        if gfx.hidden {
            return Ok(());
        }
        let tag = {
            let mut s = self.slot.lock().expect("the slot's lock is never poisoned");
            let Some(tag) = s.tag.take() else { return Ok(()) };
            std::mem::swap(&mut s.data, &mut gfx.upload);
            tag
        };

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

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gfx.device.create_command_encoder(&Default::default());
        if let Some(t) = gfx.timer.as_mut() {
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
        {
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
        if let Some(t) = &gfx.timer {
            t.resolve(&mut encoder);
        }
        gfx.queue.submit([encoder.finish()]);
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
        self.stats.submitted();
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
            }
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let result = match event {
            UserEvent::Frame => self.render(),
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
                // q quits, as in view.sh.
                if matches!(&event.logical_key, Key::Character(c) if c == "q") {
                    event_loop.exit();
                } else if !self.audio_key(&event.logical_key) {
                    self.picture_key(&event.logical_key);
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
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Feedback may have freed the way for a frame that had to wait.
        if let Err(e) = self.drain().and_then(|()| self.render()) {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        // The last feedback, then let go of winit's display while it lives.
        if let Err(e) = self.drain() {
            self.error.get_or_insert(e);
        }
        self.gfx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
