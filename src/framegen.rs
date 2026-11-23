//! Frame generation's seam (#66, GLOSSARY "Frame generation"): what the
//! window shows for each card frame it takes, and the generators that make
//! the pictures between new frames.
//!
//! - **The schedule** is pure: each card frame's kind (duplicate.rs) and
//!   the source's cadence in, what to show out. A generated frame only
//!   ever takes the place of a duplicate frame, and only in a steady 2:2
//!   cadence (a 30 fps source in a 60 Hz signal): when a new frame comes,
//!   the picture halfway between the last new frame and it is shown, and
//!   the new frame itself on the duplicate that follows. Anything else (a
//!   source that matches the signal, a cadence break, the first frames, a
//!   scene cut the generator reports, the window a refresh behind) shows
//!   the real frame, as without frame generation. So it never adds more
//!   than one refresh of delay. The window still presents once per card
//!   frame (pacing.rs); only what it presents changes.
//! - **New-frames-only mode** shows each new frame once and presents no
//!   duplicate: an external Vulkan frame-generation layer then sees the
//!   source's own frame rate (#69).
//! - **A generator** writes the picture halfway between two new frames,
//!   the convert pass's RGB frames at the capture's size, before scaling.
//!   `Copy` is the one on `main`, for the wiring and its tests; the real
//!   ones come on their own branches (#67, #68).
//!
//! The user-facing setting (FRAMEGEN), its key and its overlay row come
//! with the generator that wins round one; until then the mode is a dev
//! build's `--framegen` (main.rs), and a default build picks none: the
//! seam is compiled and tested there, but nothing reaches it.
#![cfg_attr(not(feature = "dev"), allow(dead_code))]

use std::time::Duration;

use crate::duplicate::{Cadence, Kind, Tracker};
use crate::gputime::PassTimes;

/// How the window uses frame generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Generated frames from the `Copy` generator: the last new frame
    /// again, so the picture is today's, a refresh late.
    Copy,
    /// No generator: each new frame once, duplicates not presented.
    NewOnly,
}

impl Mode {
    /// Every mode, as `--framegen` names them.
    pub const ALL: [Mode; 2] = [Mode::Copy, Mode::NewOnly];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Copy => "copy",
            Mode::NewOnly => "new-only",
        }
    }

    pub fn from_flag(v: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == v)
            .ok_or_else(|| format!("--framegen {v}: use {}", Self::ALL.map(Mode::as_str).join(" or ")))
    }

    /// The mode runs a generator on the GPU.
    pub fn generates(self) -> bool {
        match self {
            Mode::Copy => true,
            Mode::NewOnly => false,
        }
    }
}

/// Why frame generation is refused on this GPU, if it is: it is meant for
/// dedicated gaming GPUs (decided on #63), so an integrated one shows real
/// frames, said once at the start. New-frames-only runs no generator and
/// is never refused.
pub fn refusal(mode: Mode, device: wgpu::DeviceType, name: &str) -> Option<String> {
    (mode.generates() && device == wgpu::DeviceType::IntegratedGpu)
        .then(|| format!("frame generation needs a dedicated GPU, and {name} is integrated: showing real frames"))
}

/// What the window shows for a card frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// This card frame, as without frame generation.
    Real,
    /// The generated frame between the last new frame and this one; this
    /// one is held for the next card frame.
    Generated,
    /// The new frame the last `Generated` held, shown on the duplicate
    /// that followed it.
    Held,
    /// Not presented (new-frames-only: a duplicate frame).
    Withheld,
}

/// The schedule's answer for one card frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub show: Show,
    /// A generated frame was due, but the window was a refresh behind
    /// (the new frame had been superseded in the slot): shown real.
    pub missed: bool,
}

/// What to show, card frame by card frame.
pub struct Schedule {
    mode: Mode,
    tracker: Tracker,
    /// A new frame came before this one: there is a pair to generate from.
    have_new: bool,
    /// The last step showed a generated frame and holds its new frame.
    holding: bool,
}

impl Schedule {
    pub fn new(mode: Mode) -> Self {
        Self { mode, tracker: Tracker::new(), have_new: false, holding: false }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// A card frame the window took: its kind against the last frame the
    /// window took (run.rs `Slot::put`), its capture time, and whether it
    /// replaced a frame the window never took (`late`: the window is a
    /// refresh behind, and a generated frame now would push the real one
    /// later still).
    pub fn next(&mut self, kind: Kind, captured: Duration, late: bool) -> Step {
        self.tracker.push(kind, captured);
        let real = Step { show: Show::Real, missed: false };
        match (self.mode, kind) {
            (Mode::NewOnly, Kind::New) => real,
            (Mode::NewOnly, Kind::Duplicate) => Step { show: Show::Withheld, missed: false },
            (Mode::Copy, Kind::New) => {
                let pair = std::mem::replace(&mut self.have_new, true);
                self.holding = false;
                if !pair || self.tracker.cadence() != Cadence::TwoTwo {
                    real
                } else if late {
                    Step { show: Show::Real, missed: true }
                } else {
                    self.holding = true;
                    Step { show: Show::Generated, missed: false }
                }
            }
            (Mode::Copy, Kind::Duplicate) if std::mem::take(&mut self.holding) => Step { show: Show::Held, missed: false },
            (Mode::Copy, Kind::Duplicate) => real,
        }
    }

    /// The generator would not interpolate the pair (a scene cut): the new
    /// frame shows now, real, and nothing is held for the duplicate.
    pub fn cut(&mut self) -> Show {
        self.holding = false;
        Show::Real
    }
}

/// The frames a generator works on, as texture views at the capture's
/// size, `Rgba8Unorm`.
pub struct Pair<'a> {
    /// The new frame before `next`.
    pub prev: &'a wgpu::TextureView,
    /// Read by the real generators (#67, #68); `Copy` shows `prev`.
    #[allow(dead_code)]
    pub next: &'a wgpu::TextureView,
    /// Where the generated frame goes: what the window scales and shows.
    pub out: &'a wgpu::TextureView,
}

/// A way to make the picture halfway between two new frames.
pub trait Generator {
    /// Encodes the frame halfway between `frames.prev` and `frames.next`
    /// into `frames.out`. False: the pair is not interpolated (a scene
    /// cut), and the window shows the real frame instead. With `times`,
    /// the generator's first pass writes the start and its last pass the
    /// end, on every call (the GPU time, gputime.rs).
    fn generate(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frames: Pair<'_>,
        times: Option<PassTimes<'_>>,
    ) -> bool;
}

/// The trivial generator: the previous new frame, unchanged. Shown in
/// place of the generated frame it makes the picture today's, one refresh
/// late, which proves the wiring and the pacing without judging a picture.
pub struct Copy {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

const COPY: &str = r#"
@group(0) @binding(0) var prev: texture_2d<f32>;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    return textureLoad(prev, vec2u(in.pos.xy), 0);
}
"#;

impl Copy {
    pub fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("framegen copy"),
            entries: &[crate::app::texture_entry(0, false)],
        });
        let pipeline = crate::app::pipeline(device, "framegen copy", COPY, &layout, wgpu::TextureFormat::Rgba8Unorm);
        Self { pipeline, layout }
    }
}

impl Generator for Copy {
    fn generate(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frames: Pair<'_>,
        times: Option<PassTimes<'_>>,
    ) -> bool {
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("framegen copy"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(frames.prev) }],
        });
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view: frames.out,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("framegen copy"),
            color_attachments: &attachments,
            depth_stencil_attachment: None,
            timestamp_writes: times.as_ref().and_then(|t| t.render(true, true)),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

/// The generator a mode runs, if any.
pub fn generator(mode: Mode, device: &wgpu::Device) -> Option<Box<dyn Generator>> {
    match mode {
        Mode::Copy => Some(Box::new(Copy::new(device))),
        Mode::NewOnly => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Kind::{Duplicate as D, New as N};

    /// Kinds in at 60 Hz, never late: what the schedule shows.
    fn run(mode: Mode, kinds: &[Kind]) -> Vec<Show> {
        let mut s = Schedule::new(mode);
        kinds.iter().enumerate().map(|(i, &k)| s.next(k, Duration::from_micros(i as u64 * 16_667), false).show).collect()
    }

    /// `n` new frames in 2:2.
    fn two_two(n: usize) -> Vec<Kind> {
        [N, D].repeat(n)
    }

    #[test]
    fn a_steady_2_2_alternates_generated_and_held_once_established() {
        let shows = run(Mode::Copy, &two_two(10));
        // 2:2 is established after four closed holds of 2 (duplicate.rs
        // ESTABLISH): until then, and for the first frames, real.
        let first = shows.iter().position(|s| *s == Show::Generated).expect("generated once 2:2 holds");
        assert!(shows[..first].iter().all(|s| *s == Show::Real), "{shows:?}");
        assert_eq!(first % 2, 0, "on a new frame");
        for pair in shows[first..].chunks(2) {
            assert_eq!(pair, [Show::Generated, Show::Held], "{shows:?}");
        }
    }

    #[test]
    fn the_first_two_frames_are_real() {
        assert_eq!(run(Mode::Copy, &[N, D]), [Show::Real, Show::Real]);
    }

    #[test]
    fn a_source_that_matches_the_signal_is_shown_real() {
        assert!(run(Mode::Copy, &[N; 30]).iter().all(|s| *s == Show::Real));
    }

    #[test]
    fn a_break_shows_real_frames_until_2_2_holds_again() {
        let mut kinds = two_two(8);
        kinds.extend([N, D, D]); // a hold of 3: base PS5 under 30 fps
        let before = kinds.len();
        kinds.extend(two_two(8));
        let shows = run(Mode::Copy, &kinds);
        // The third frame of the hold of 3: the break; real from there.
        assert_eq!(&shows[before - 3..before - 1], [Show::Generated, Show::Held]);
        assert_eq!(shows[before - 1], Show::Real);
        let back = shows[before..].iter().position(|s| *s == Show::Generated).expect("2:2 again");
        assert!(back >= 6, "re-established only after four holds of 2: {:?}", &shows[before..]);
        assert!(shows[before..before + back].iter().all(|s| *s == Show::Real));
    }

    #[test]
    fn a_cut_shows_the_new_frame_real_and_holds_nothing() {
        let mut s = Schedule::new(Mode::Copy);
        let mut t = 0u64;
        let mut next = |s: &mut Schedule, k| {
            t += 16_667;
            s.next(k, Duration::from_micros(t), false).show
        };
        for _ in 0..8 {
            next(&mut s, N);
            next(&mut s, D);
        }
        assert_eq!(next(&mut s, N), Show::Generated);
        assert_eq!(s.cut(), Show::Real);
        assert_eq!(next(&mut s, D), Show::Real, "nothing held after a cut");
        assert_eq!(next(&mut s, N), Show::Generated, "the next pair is generated again");
    }

    #[test]
    fn a_late_window_misses_the_generated_frame_and_shows_real() {
        let mut s = Schedule::new(Mode::Copy);
        for i in 0..16u64 {
            s.next(if i % 2 == 0 { N } else { D }, Duration::from_micros(i * 16_667), false);
        }
        let step = s.next(N, Duration::from_micros(16 * 16_667), true);
        assert_eq!(step, Step { show: Show::Real, missed: true });
        assert_eq!(s.next(D, Duration::from_micros(17 * 16_667), false).show, Show::Real, "nothing held");
    }

    #[test]
    fn new_frames_only_withholds_every_duplicate() {
        assert_eq!(
            run(Mode::NewOnly, &[N, D, N, D, D, N, N]),
            [Show::Real, Show::Withheld, Show::Real, Show::Withheld, Show::Withheld, Show::Real, Show::Real]
        );
    }

    #[test]
    fn integrated_gpus_are_refused_a_generator_and_nothing_else() {
        use wgpu::DeviceType::*;
        assert!(refusal(Mode::Copy, IntegratedGpu, "Intel UHD 620").is_some_and(|m| m.contains("Intel UHD 620")));
        for d in [DiscreteGpu, VirtualGpu, Cpu, Other] {
            assert_eq!(refusal(Mode::Copy, d, "x"), None, "{d:?}");
        }
        assert_eq!(refusal(Mode::NewOnly, IntegratedGpu, "x"), None, "no generator, nothing refused");
    }

    /// The `Copy` generator on the GPU: its output is the previous new
    /// frame, texel for texel, through the window's frame format:
    /// `cargo test -- --ignored gpu_copy`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_copy_outputs_the_previous_frame() {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = crate::app::BACKENDS;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("a GPU");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let (w, h) = (64u32, 36u32);
        let texture = |usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        let picture = |seed: u32| -> Vec<u8> {
            (0..w * h).flat_map(|i| [(i.wrapping_mul(seed) >> 3) as u8, (i % w) as u8 * 4, (i / w) as u8 * 7, 255]).collect()
        };
        let (prev_px, next_px) = (picture(2_654_435_761), picture(40_503));
        let source = |px: &[u8]| {
            let t = texture(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            queue.write_texture(
                t.as_image_copy(),
                px,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            t
        };
        let (prev, next) = (source(&prev_px), source(&next_px));
        let out = texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let views = [&prev, &next, &out].map(|t| t.create_view(&Default::default()));
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(w * h * 4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut generator = generator(Mode::Copy, &device).expect("copy generates");
        let mut encoder = device.create_command_encoder(&Default::default());
        let frames = Pair { prev: &views[0], next: &views[1], out: &views[2] };
        assert!(generator.generate(&device, &queue, &mut encoder, frames, None));
        encoder.copy_texture_to_buffer(
            out.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        readback.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let got = readback.get_mapped_range(..).unwrap().to_vec();
        assert!(got == prev_px, "the previous frame, texel for texel");
        assert!(got != next_px);
    }

    #[test]
    fn the_flag_names_every_mode() {
        for m in Mode::ALL {
            assert_eq!(Mode::from_flag(m.as_str()), Ok(m));
        }
        assert!(Mode::from_flag("blend").unwrap_err().contains("copy or new-only"));
    }
}
