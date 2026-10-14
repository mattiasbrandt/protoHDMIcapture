//! #7: libmpv inside the Rust window, paced the PoC's way. mpv reads the card
//! and renders (its scalers, colour handling); we own the window, the EGL
//! context and the swap, and hand the compositor a frame only after its frame
//! callback for the previous one (the PoC's pacing, `display::App::render`).
//! libmpv's render API is OpenGL-only, hence EGL here instead of wgpu.
//!
//! Timed from outside like every other player (`hdmipoc meta` +
//! `tools/present-shim.so` + `tools/present-latency.py`).

use std::ffi::{CString, c_void};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use glutin::config::{Config, ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext};
use glutin::display::{Display, GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use libmpv2::Mpv;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType, mpv_render_update};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use crate::capture::{self, now_monotonic};
use crate::presentation::{Outcome, Presentation, Tag};

/// A frame callback this late counts as lost: pacing stops waiting for it.
const PACE_TIMEOUT: Duration = Duration::from_millis(100);

pub struct Options {
    pub seconds: Option<u64>,
    pub monitor: Option<String>,
    pub pace: bool,
}

enum UserEvent {
    /// mpv's update callback: something to render.
    Update,
    Quit,
}

struct Gl {
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    render: RenderContext<'static>,
    presentation: Presentation,
}

struct App {
    opts: Options,
    mpv: &'static Mpv,
    url: String,
    proxy: EventLoopProxy<UserEvent>,
    gl: Option<Gl>,
    /// mpv asked for a render that pacing held back.
    pending: bool,
    in_flight: u32,
    last_submit: Duration,
    swaps: u64,
    pace_timeouts: u64,
    error: Option<String>,
}

fn get_proc_address(display: &Display, name: &str) -> *mut c_void {
    let name = CString::new(name).expect("GL function names have no NUL");
    display.get_proc_address(&name) as *mut c_void
}

pub fn run(node: &std::path::Path, opts: Options) -> Result<(), String> {
    // Today's view.sh options that concern the picture path.
    let url = format!("av://v4l2:{}", node.display());
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "libmpv")?;
        init.set_option("profile", "low-latency")?;
        init.set_option("untimed", true)?;
        init.set_option("audio", "no")?;
        init.set_option("hwdec", "no")?;
        init.set_option(
            "demuxer-lavf-o",
            format!("video_size={}x{},framerate={},input_format=yuyv422", capture::WIDTH, capture::HEIGHT, capture::FPS)
                .as_str(),
        )?;
        init.set_option("terminal", true)?;
        init.set_option("msg-level", "all=warn")?;
        Ok(())
    })
    .map_err(|e| format!("mpv: {e}"))?;
    let mpv: &'static Mpv = Box::leak(Box::new(mpv));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| format!("event loop: {e}"))?;
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

    let mut app = App {
        opts,
        mpv,
        url,
        proxy,
        gl: None,
        pending: false,
        in_flight: 0,
        last_submit: now_monotonic(),
        swaps: 0,
        pace_timeouts: 0,
        error: None,
    };
    let loop_result = event_loop.run_app(&mut app);
    eprintln!("summary: swaps {}, pace timeouts {}", app.swaps, app.pace_timeouts);
    loop_result.map_err(|e| format!("event loop: {e}"))?;
    if let Some(e) = app.error {
        return Err(e);
    }
    Ok(())
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<Gl, String> {
        let mut attrs = Window::default_attributes().with_title("hdmipoc-mpv").with_name("hdmipoc", "hdmipoc");
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

        let (window, config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, ConfigTemplateBuilder::new(), |configs| {
                configs.reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a }).expect("an EGL config")
            })
            .map_err(|e| format!("EGL display: {e}"))?;
        let window = window.ok_or("no window")?;
        let config: Config = config;
        let display = config.display();

        let raw = window.window_handle().map_err(|e| e.to_string())?.as_raw();
        let context = unsafe { display.create_context(&config, &ContextAttributesBuilder::new().build(Some(raw))) }
            .map_err(|e| format!("GL context: {e}"))?;
        let surface_attrs = window.build_surface_attributes(Default::default()).map_err(|e| e.to_string())?;
        let surface = unsafe { display.create_window_surface(&config, &surface_attrs) }
            .map_err(|e| format!("GL surface: {e}"))?;
        let context = context.make_current(&surface).map_err(|e| format!("make current: {e}"))?;
        // Never block in swap: pacing decides when to swap.
        surface
            .set_swap_interval(&context, SwapInterval::DontWait)
            .map_err(|e| format!("swap interval: {e}"))?;

        let (RawDisplayHandle::Wayland(dh), RawWindowHandle::Wayland(wh)) =
            (window.display_handle().map_err(|e| e.to_string())?.as_raw(), raw)
        else {
            return Err("not on Wayland".into());
        };
        // Safety: winit's display and the window's surface outlive `Gl`,
        // which `exiting` drops first.
        let presentation = unsafe { Presentation::new(dh.display.as_ptr(), wh.surface.as_ptr())? };

        let mut render = self
            .mpv
            .create_render_context(vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams { get_proc_address, ctx: display.clone() }),
                RenderParam::WaylandDisplay(dh.display.as_ptr() as *const c_void),
            ])
            .map_err(|e| format!("mpv render context: {e}"))?;
        let proxy = self.proxy.clone();
        render.set_update_callback(move || {
            let _ = proxy.send_event(UserEvent::Update);
        });
        self.mpv.command("loadfile", &[&self.url]).map_err(|e| format!("mpv loadfile: {e}"))?;
        eprintln!("mpv: rendering through libmpv into an EGL surface, paced {}", self.opts.pace);
        Ok(Gl { window, surface, context, render, presentation })
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(gl) = self.gl.as_mut() else { return Ok(()) };
        for o in gl.presentation.drain()? {
            if let Outcome::FrameDone = o {
                self.in_flight = self.in_flight.saturating_sub(1);
            }
        }
        Ok(())
    }

    fn render(&mut self) -> Result<(), String> {
        if self.opts.pace && self.in_flight > 0 {
            if now_monotonic() - self.last_submit < PACE_TIMEOUT {
                self.pending = true;
                return Ok(());
            }
            self.pace_timeouts += 1;
            self.in_flight = 0;
        }
        let Some(gl) = self.gl.as_mut() else { return Ok(()) };
        self.pending = false;
        let flags = gl.render.update().map_err(|e| format!("mpv update: {e}"))?;
        if flags & mpv_render_update::Frame == 0 {
            return Ok(());
        }
        let size = gl.window.inner_size();
        gl.render
            .render::<Display>(0, size.width as i32, size.height as i32, true)
            .map_err(|e| format!("mpv render: {e}"))?;
        // Feedback and frame callback first: the swap commits.
        let now = now_monotonic();
        gl.presentation.request(Tag { captured: now, dequeued: now })?;
        gl.window.pre_present_notify();
        gl.surface.swap_buffers(&gl.context).map_err(|e| format!("swap: {e}"))?;
        gl.render.report_swap();
        self.in_flight += 1;
        self.last_submit = now_monotonic();
        self.swaps += 1;
        Ok(())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gl.is_some() {
            return;
        }
        match self.init(event_loop) {
            Ok(gl) => self.gl = Some(gl),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let result = match event {
            UserEvent::Update => self.render(),
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
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(&event.logical_key, Key::Named(NamedKey::Escape)) =>
            {
                event_loop.exit()
            }
            WindowEvent::Resized(size) => {
                if let (Some(gl), Some(w), Some(h)) =
                    (self.gl.as_ref(), NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    gl.surface.resize(&gl.context, w, h);
                    eprintln!("surface: resized to {}x{}", size.width, size.height);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let result = self.drain().and_then(|()| if self.pending { self.render() } else { Ok(()) });
        if let Err(e) = result {
            self.error = Some(e);
            event_loop.exit();
            return;
        }
        // mpv waits (up to 200 ms, vo_libmpv.c flip_page) for a frame held
        // back here, and nothing wakes winit for the frame callback when
        // Mesa's EGL read it off the socket: poll for it every 1 ms.
        event_loop.set_control_flow(if self.pending {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1))
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        // The render context goes before mpv quits and before winit's display.
        self.gl = None;
    }
}
