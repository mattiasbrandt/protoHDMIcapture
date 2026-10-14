//! Display: wgpu on Vulkan in a winit window, a frame drawn the moment it
//! arrives (not on a redraw request, which Wayland throttles to the
//! compositor's frame callback), YUYV -> RGB in the shader, and every
//! present timed by the compositor (`presentation`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::Audio;
use crate::capture::{self, Capture, Event, now_monotonic};
use crate::presentation::{Outcome, Presentation, Tag};
use crate::stats::Stats;

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

// Pass 1, at the capture's own 1920x1080: YUYV -> RGB. A texel is one
// macropixel, Y0 U Y1 V for two pixels; chroma is sited with the even pixel
// and interpolated for the odd one. BT.709 limited range (a PS5 over the
// MS2130 measured luma 16..235).
const CONVERT: &str = r#"
@group(0) @binding(0) var yuyv: texture_2d<f32>;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let x = u32(in.pos.x);
    let y = u32(in.pos.y);
    let t = textureLoad(yuyv, vec2u(x / 2u, y), 0);
    var luma = t.r;
    var c = t.ga;
    if ((x & 1u) == 1u) {
        luma = t.b;
        let next = textureLoad(yuyv, vec2u(min(x / 2u + 1u, 959u), y), 0);
        c = (t.ga + next.ga) * 0.5;
    }
    let yy = (luma - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3f(yy + 1.5748 * cr, yy - 0.1873 * cb - 0.4681 * cr, yy + 1.8556 * cb);
    return vec4f(clamp(rgb, vec3f(0.0), vec3f(1.0)), 1.0);
}
"#;

// Pass 2: the RGB frame scaled bilinearly into the letterboxed viewport
// (1:1 when the viewport is 1920x1080).
const SCALE: &str = r#"
@group(0) @binding(0) var rgb: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    return textureSample(rgb, smp, in.uv);
}
"#;

fn pipeline(
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

fn texture_entry(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
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

pub struct Options {
    pub windowed: bool,
    pub seconds: Option<u64>,
    pub mailbox: bool,
    /// Immediate present: a frame goes out at once and may tear, where the
    /// desktop allows tearing (#8).
    pub immediate: bool,
    /// Fullscreen on this output (e.g. DP-2) instead of the compositor's pick.
    pub monitor: Option<String>,
    pub delay_ms: u32,
    /// Hand a frame to the compositor only once it has taken the previous
    /// one (its frame callback; see `App::render`).
    pub pace: bool,
}

/// A frame callback this late counts as lost: pacing stops waiting for it.
const PACE_TIMEOUT: Duration = Duration::from_millis(100);

/// `[` and `]` change the audio delay by this much.
const DELAY_STEP_MS: u32 = 10;

enum UserEvent {
    Frame,
    Quit,
}

/// The newest captured frame, waiting for the display. A frame replaced
/// before the display took it was skipped by the viewer (not a capture gap).
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
    rgb_view: wgpu::TextureView,
    scale_pipeline: wgpu::RenderPipeline,
    scale_bind_group: wgpu::BindGroup,
    texture: wgpu::Texture,
    upload: Vec<u8>,
    // Borrows winit's wl_display: dropped in `exiting`, while it lives.
    presentation: Presentation,
}

struct App {
    opts: Options,
    slot: Arc<Mutex<Slot>>,
    gfx: Option<Gfx>,
    stats: Stats,
    audio: Audio,
    /// Presents whose frame callback has not arrived yet, and when the
    /// last one was submitted.
    in_flight: u32,
    last_submit: Duration,
    error: Option<String>,
}

pub fn run(node: &std::path::Path, opts: Options) -> Result<(), String> {
    let mut cap = Capture::open(node).map_err(|e| format!("{}: {e}", node.display()))?;
    eprintln!(
        "capture: {} YUYV {}x{} @ {}",
        node.display(),
        capture::WIDTH,
        capture::HEIGHT,
        capture::FPS
    );

    let usb = capture::usb_id(node).ok_or("the card is not on USB")?;
    let audio = Audio::start(&usb, opts.delay_ms)?;
    eprintln!("{}", audio.describe());

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

    let slot = Arc::new(Mutex::new(Slot {
        data: vec![0; cap.frame_size],
        tag: None,
        superseded: 0,
    }));
    let start = now_monotonic();
    let capture_thread = {
        let slot = slot.clone();
        let quit = quit.clone();
        let stop = wake(proxy.clone(), quit.clone());
        thread::spawn(move || -> Result<(u64, u64), String> {
            let mut frames = 0u64;
            let mut gaps = 0u64;
            let result = cap.run(&quit, |e| {
                let t = (now_monotonic() - start).as_secs_f64();
                match e {
                    Event::Frame(f) => {
                        frames += 1;
                        {
                            let mut s = slot.lock().unwrap();
                            if s.tag.is_some() {
                                s.superseded += 1;
                            }
                            s.data.copy_from_slice(f.data);
                            s.tag = Some(Tag { captured: f.captured, dequeued: f.dequeued });
                        }
                        let _ = proxy.send_event(UserEvent::Frame);
                    }
                    Event::Gap { from, to, lost } if frames == 0 => {
                        eprintln!("{t:6.1} s  start-up gap: sequence {from} -> {to}, {lost} lost");
                    }
                    Event::Gap { from, to, lost } => {
                        gaps += 1;
                        eprintln!("{t:6.1} s  gap: sequence {from} -> {to}, {lost} lost");
                    }
                    Event::NoFrames => eprintln!("{t:6.1} s  no frames"),
                    Event::FramesBack => eprintln!("{t:6.1} s  frames again"),
                }
            });
            stop();
            result.map(|()| (frames, gaps)).map_err(|e| format!("capture: {e}"))
        })
    };

    let mut app = App { opts, slot: slot.clone(), gfx: None, stats: Stats::new(start), audio, in_flight: 0, last_submit: start, error: None };
    let loop_result = event_loop.run_app(&mut app);
    quit.store(true, Ordering::Relaxed);
    let captured = capture_thread.join().map_err(|_| "capture thread panicked".to_string())?;

    let superseded = slot.lock().unwrap().superseded;
    app.stats.summary(superseded);
    eprintln!("summary: audio {}", app.audio.granted());
    eprintln!("summary: audio {}", app.audio.status());
    match &captured {
        Ok((frames, gaps)) => eprintln!("summary: captured {frames} frames, {gaps} capture gaps"),
        Err(e) => eprintln!("summary: {e}"),
    }
    loop_result.map_err(|e| format!("event loop: {e}"))?;
    if let Some(e) = app.error {
        return Err(e);
    }
    captured.map(|_| ())
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<Gfx, String> {
        let mut attrs = Window::default_attributes()
            .with_title("hdmipoc")
            .with_inner_size(PhysicalSize::new(capture::WIDTH, capture::HEIGHT));
        if !self.opts.windowed {
            let monitor = match &self.opts.monitor {
                None => None,
                Some(name) => Some(
                    event_loop
                        .available_monitors()
                        .find(|m| m.name().as_deref() == Some(name.as_str()))
                        .ok_or_else(|| format!("no monitor named {name}"))?,
                ),
            };
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(monitor)));
        }
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| format!("window: {e}"))?);

        let (RawDisplayHandle::Wayland(dh), RawWindowHandle::Wayland(wh)) = (
            window.display_handle().map_err(|e| e.to_string())?.as_raw(),
            window.window_handle().map_err(|e| e.to_string())?.as_raw(),
        ) else {
            return Err("not on Wayland: presentation timing needs wp_presentation".into());
        };
        // Safety: winit's display and the window's surface live until
        // `exiting`, where `Gfx` (and this) is dropped first.
        let presentation = unsafe { Presentation::new(dh.display.as_ptr(), wh.surface.as_ptr())? };
        eprintln!("wp_presentation clock id {:?} (CLOCK_MONOTONIC is 1)", presentation.clock_id());
        if presentation.clock_id() != Some(libc::CLOCK_MONOTONIC as u32) {
            return Err("presentation clock is not CLOCK_MONOTONIC; latencies would be meaningless".into());
        }

        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone()).map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("adapter: {e}"))?;
        let info = adapter.get_info();
        eprintln!("gpu: {} ({:?}, {})", info.name, info.backend, info.driver_info);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| format!("device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        eprintln!("present modes offered: {:?}", caps.present_modes);
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("surface not supported by the adapter")?;
        // Not sRGB: the shader's output is already gamma-encoded video.
        config.format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
            .ok_or("no non-sRGB surface format")?;
        let wanted = if self.opts.immediate {
            wgpu::PresentMode::Immediate
        } else if self.opts.mailbox {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        config.present_mode =
            if caps.present_modes.contains(&wanted) { wanted } else { wgpu::PresentMode::Fifo };
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        eprintln!(
            "surface: {}x{} {:?}, present mode {:?} ({:?} requested), frame latency {}, paced {}",
            config.width,
            config.height,
            config.format,
            config.present_mode,
            wanted,
            config.desired_maximum_frame_latency,
            self.opts.pace
        );

        let frame_texture = |label, width, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height: capture::HEIGHT, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let texture = frame_texture(
            "yuyv",
            capture::WIDTH / 2,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        let rgb = frame_texture(
            "rgb",
            capture::WIDTH,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let rgb_view = rgb.create_view(&Default::default());

        let convert_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("convert"),
            entries: &[texture_entry(0, false)],
        });
        let convert_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("convert"),
            layout: &convert_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture.create_view(&Default::default())),
            }],
        });
        let convert_pipeline =
            pipeline(&device, "convert", CONVERT, &convert_layout, wgpu::TextureFormat::Rgba8Unorm);

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

        Ok(Gfx {
            window,
            surface,
            device,
            queue,
            config,
            convert_pipeline,
            convert_bind_group,
            rgb_view,
            scale_pipeline,
            scale_bind_group,
            texture,
            upload: vec![0; (capture::WIDTH * capture::HEIGHT * 2) as usize],
            presentation,
        })
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        for o in gfx.presentation.drain()? {
            match o {
                Outcome::Presented { tag, at, refresh, flags } => self.stats.presented(tag, at, refresh, flags),
                Outcome::Discarded => self.stats.discarded(),
                Outcome::FrameDone => self.in_flight = self.in_flight.saturating_sub(1),
            }
        }
        Ok(())
    }

    /// Uploads the newest frame and presents it, now.
    ///
    /// Paced, it waits until the compositor has taken the previous frame
    /// (frame callback): the card (60.000 Hz) outruns a 59.95 Hz display,
    /// and every frame handed over early queues behind the last (Fifo grew
    /// to ~4 frames, 72 ms median, over 10 minutes). Meanwhile the slot
    /// keeps only the newest frame. Waiting for "presented" instead halved
    /// the frame rate: it comes a refresh or more after the hand-over.
    fn render(&mut self) -> Result<(), String> {
        if self.opts.pace && self.in_flight > 0 {
            if now_monotonic() - self.last_submit < PACE_TIMEOUT {
                return Ok(());
            }
            self.stats.pace_timeout();
            self.in_flight = 0;
        }
        let Some(gfx) = self.gfx.as_mut() else { return Ok(()) };
        let tag = {
            let mut s = self.slot.lock().unwrap();
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

        gfx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &gfx.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &gfx.upload,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(capture::WIDTH * 2),
                rows_per_image: Some(capture::HEIGHT),
            },
            wgpu::Extent3d { width: capture::WIDTH / 2, height: capture::HEIGHT, depth_or_array_layers: 1 },
        );

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gfx.device.create_command_encoder(&Default::default());
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
                timestamp_writes: None,
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
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Letterbox 16:9 into whatever the surface is.
            let (sw, sh) = (gfx.config.width as f32, gfx.config.height as f32);
            let scale = (sw / capture::WIDTH as f32).min(sh / capture::HEIGHT as f32);
            let (w, h) = (capture::WIDTH as f32 * scale, capture::HEIGHT as f32 * scale);
            // Whole-pixel origin, so 1920x1080 lands 1:1 on the surface.
            pass.set_viewport(((sw - w) / 2.0).round(), ((sh - h) / 2.0).round(), w, h, 0.0, 1.0);
            pass.set_pipeline(&gfx.scale_pipeline);
            pass.set_bind_group(0, &gfx.scale_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gfx.queue.submit([encoder.finish()]);

        // `present` commits the wl_surface; the feedback request must come
        // before it to attach to that commit.
        gfx.presentation.request(tag)?;
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
            Ok(gfx) => self.gfx = Some(gfx),
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
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character(c) if c == "q" => event_loop.exit(),
                    Key::Character(c) if c == "[" || c == "]" => {
                        let a = &self.audio;
                        let ms = if c == "]" {
                            a.delay_ms() + DELAY_STEP_MS
                        } else {
                            a.delay_ms().saturating_sub(DELAY_STEP_MS)
                        };
                        eprintln!("audio delay {} -> {ms} ms", a.delay_ms());
                        a.set_delay_ms(ms);
                    }
                    _ => {}
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(gfx) = self.gfx.as_mut() {
                    gfx.config.width = size.width.max(1);
                    gfx.config.height = size.height.max(1);
                    gfx.surface.configure(&gfx.device, &gfx.config);
                    let monitor = gfx.window.current_monitor();
                    eprintln!(
                        "surface: resized to {}x{} on {} at {:.3} Hz, fullscreen {}",
                        gfx.config.width,
                        gfx.config.height,
                        monitor.as_ref().and_then(|m| m.name()).unwrap_or_else(|| "?".into()),
                        monitor.and_then(|m| m.refresh_rate_millihertz()).unwrap_or(0) as f64 / 1000.0,
                        gfx.window.fullscreen().is_some()
                    );
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
        // Feedback for the last presents, then let go of winit's display
        // while it is still alive.
        if let Err(e) = self.drain() {
            self.error.get_or_insert(e);
        }
        self.gfx = None;
    }
}
