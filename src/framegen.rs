//! Frame generation's seam (#66, GLOSSARY "Frame generation"): what the
//! window shows for each card frame it takes, and the generators that make
//! the pictures between new frames.
//!
//! - **The schedule** is pure: each card frame's kind (duplicate.rs) and
//!   the source's cadence in, what to show out. The cadence is the capture
//!   thread's (run.rs), which sees every card frame, including those the
//!   window never takes, so a late window does not read as a cadence break
//!   (#78). A generated frame only ever takes the place of a duplicate
//!   frame, and only in a steady 2:2
//!   cadence (a 30 fps source in a 60 Hz signal): when a new frame comes,
//!   the picture halfway between the last new frame and it is shown, and
//!   the new frame itself on the duplicate that follows. Anything else (a
//!   source that matches the signal, a cadence break, the first frames, a
//!   scene cut the generator reports) shows the real frame, as without
//!   frame generation. The window a refresh behind misses one generated
//!   frame: if a generated frame was holding its new frame, that new frame
//!   shows now and the late one on the next duplicate, so every new frame
//!   is shown. So it never adds more than one refresh of delay. The
//!   window still presents once per card frame (pacing.rs); only what it
//!   presents changes.
//! - **New-frames-only mode** shows each new frame once and presents no
//!   duplicate: an external Vulkan frame-generation layer then sees the
//!   source's own frame rate (#69).
//! - **A generator** writes the picture halfway between two new frames,
//!   the convert pass's RGB frames at the capture's size, before scaling.
//!   Copy (the last new frame again) is for the wiring and its tests;
//!   blend (#67, half and half) is round one's baseline; ffx-base,
//!   FidelityFX optical flow (#68, framegen/ffx.rs), is ffx's base.
//! - **What ships** (#84): FRAMEGEN=off|ffx|rife, `g` and the overlay's
//!   Frame generation row: ffx (#73, the FidelityFX flow at full
//!   resolution) and rife (#74, RIFE v4.6). The other generators are a
//!   dev build's, by `--framegen` (main.rs).

mod ffx;

use crate::duplicate::{Cadence, Kind};
use crate::gputime::PassTimes;

mod rife;

/// How the window uses frame generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Generated frames from the copy generator: the last new frame
    /// again, so the picture is today's, a refresh late.
    Copy,
    /// Generated frames from the `Blend` generator: the two new frames
    /// mixed half and half (#67, round one's baseline: ghosts on motion).
    Blend,
    /// No generator: each new frame once, duplicates not presented.
    NewOnly,
    /// AMD FidelityFX Optical Flow and an interpolation along it (#68,
    /// framegen/ffx.rs `Quality::FAST`): ffx's base, a dev build's.
    FfxBase,
    /// The same, spending more GPU time on the picture (#73,
    /// framegen/ffx.rs `Quality::BEST`): FRAMEGEN=ffx.
    Ffx,
    /// RIFE v4.6's machine-learned interpolation (#74, rife.rs).
    Rife,
}

impl Mode {
    /// Every mode, as `--framegen` names them.
    pub const ALL: [Mode; 6] = [Mode::Copy, Mode::Blend, Mode::NewOnly, Mode::FfxBase, Mode::Ffx, Mode::Rife];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Copy => "copy",
            Mode::Blend => "blend",
            Mode::NewOnly => "new-only",
            Mode::FfxBase => "ffx-base",
            Mode::Ffx => "ffx",
            Mode::Rife => "rife",
        }
    }

    /// A dev build's `--framegen MODE` (main.rs, score.rs): every mode.
    #[cfg_attr(not(feature = "dev"), allow(dead_code))]
    pub fn from_flag(v: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == v)
            .ok_or_else(|| format!("--framegen {v}: use {}", Self::ALL.map(Mode::as_str).join(" or ")))
    }

    /// The mode runs a generator on the GPU.
    pub fn generates(self) -> bool {
        match self {
            Mode::Copy | Mode::Blend | Mode::FfxBase | Mode::Ffx | Mode::Rife => true,
            Mode::NewOnly => false,
        }
    }

    /// The mode ships (#84): FRAMEGEN names it and `g` cycles through it.
    /// The rest are a dev build's `--framegen`: `Copy` (a test generator),
    /// `NewOnly` (for external layers), and blend and ffx-base, judged and
    /// not shipped (#63).
    pub fn shipped(self) -> bool {
        match self {
            Mode::Ffx | Mode::Rife => true,
            Mode::Copy | Mode::Blend | Mode::NewOnly | Mode::FfxBase => false,
        }
    }

    /// FRAMEGEN's value (#84): off, or a shipped mode by its name.
    pub fn from_setting(v: &str) -> Result<Option<Self>, String> {
        if v == "off" {
            return Ok(None);
        }
        shipped().into_iter().find(|m| m.as_str() == v).map(Some).ok_or_else(|| {
            let names: Vec<&str> = ["off"].into_iter().chain(shipped().into_iter().map(Mode::as_str)).collect();
            format!("FRAMEGEN={v}: use {}", names.join(", "))
        })
    }

    /// FRAMEGEN as the settings file spells it: off, or a shipped mode's
    /// name. A dev build's other modes are not saved: off.
    pub fn setting(mode: Option<Self>) -> &'static str {
        mode.filter(|m| m.shipped()).map_or("off", Mode::as_str)
    }
}

/// `g`'s next step from `current` (None: off) through `modes`, in their
/// order, then off again (#83). A current mode not in `modes` (a
/// `--framegen copy` run) steps to the first.
pub fn cycle(current: Option<Mode>, modes: &[Mode]) -> Option<Mode> {
    match current.and_then(|c| modes.iter().position(|&m| m == c)) {
        Some(i) => modes.get(i + 1).copied(),
        None => modes.first().copied(),
    }
}

/// The shipped modes, in `Mode::ALL`'s order: what `g` cycles through.
pub fn shipped() -> Vec<Mode> {
    Mode::ALL.into_iter().filter(|m| m.shipped()).collect()
}

/// Why frame generation is refused on this GPU, if it is: it is meant for
/// dedicated gaming GPUs (decided on #63), so an integrated one shows real
/// frames, said once at the start. rife is refused, too, where a `size`
/// frame needs a bigger buffer than the device's `limits` bind (#74: a 4K
/// capture), which wgpu would otherwise end in a panic. New-frames-only
/// runs no generator and is never refused.
pub fn refusal(mode: Mode, device: wgpu::DeviceType, name: &str, size: (u32, u32), limits: &wgpu::Limits) -> Option<String> {
    if mode.generates() && device == wgpu::DeviceType::IntegratedGpu {
        return Some(format!("frame generation needs a dedicated GPU, and {name} is integrated: showing real frames"));
    }
    let room = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
    (mode == Mode::Rife && rife::largest_binding(size, rife::scale()) > room).then(|| {
        format!(
            "rife cannot run on a {}x{} picture on {name} (too large for this method): showing real frames; ffx works at this size",
            size.0, size.1
        )
    })
}

/// What the window shows for a card frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// This card frame, as without frame generation.
    Real,
    /// The generated frame between the last new frame and this one; this
    /// one is held for the next card frame.
    Generated,
    /// The new frame the last `Generated` held, shown on the card frame
    /// after it: the duplicate that follows, or, the window a refresh
    /// behind, a new frame, which is then held in its turn.
    Held,
    /// Not presented (new-frames-only: a duplicate frame).
    Withheld,
}

/// The schedule's answer for one card frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub show: Show,
    /// A generated frame was due, but the window was a refresh behind (a
    /// card frame had been superseded in the slot): not generated.
    pub missed: bool,
}

/// What to show, card frame by card frame.
pub struct Schedule {
    mode: Mode,
    /// A new frame came before this one: there is a pair to generate from.
    have_new: bool,
    /// The last step showed a generated frame and holds its new frame.
    holding: bool,
}

impl Schedule {
    pub fn new(mode: Mode) -> Self {
        Self { mode, have_new: false, holding: false }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// A card frame the window took: its kind against the last frame the
    /// window took (run.rs `Slot::put`), the source's cadence (the capture
    /// thread's, which saw every card frame), and whether it replaced a
    /// frame the window never took (`late`: the window is a refresh
    /// behind, and a generated frame now would push the real one later
    /// still).
    pub fn next(&mut self, kind: Kind, cadence: Cadence, late: bool) -> Step {
        let real = Step { show: Show::Real, missed: false };
        // Every generating mode follows the same rules; only its picture
        // differs (the generator).
        match (self.mode.generates(), kind) {
            (false, Kind::New) => real,
            (false, Kind::Duplicate) => Step { show: Show::Withheld, missed: false },
            (true, Kind::New) => {
                let pair = std::mem::replace(&mut self.have_new, true);
                let holding = std::mem::take(&mut self.holding);
                if !pair || cadence != Cadence::TwoTwo {
                    real
                } else if late && holding {
                    // The held new frame was never shown (its duplicate was
                    // superseded): it shows now, this one on the next card
                    // frame.
                    self.holding = true;
                    Step { show: Show::Held, missed: true }
                } else if late {
                    Step { show: Show::Real, missed: true }
                } else {
                    self.holding = true;
                    Step { show: Show::Generated, missed: false }
                }
            }
            (true, Kind::Duplicate) if std::mem::take(&mut self.holding) => Step { show: Show::Held, missed: false },
            (true, Kind::Duplicate) => real,
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
    /// The new frame just taken.
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

/// A generator that is one fragment pass over the frames: copy and
/// blend. Its shader reads `prev` at binding 0 and, when it uses it,
/// `next` at binding 1, texel for texel (the frames and `out` share a
/// size).
pub struct OnePass {
    label: &'static str,
    /// The shader reads `next` too.
    reads_next: bool,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

/// The trivial generator: the previous new frame, unchanged. Shown in
/// place of the generated frame it makes the picture today's, one refresh
/// late, which proves the wiring and the pacing without judging a picture.
const COPY: &str = r#"
@group(0) @binding(0) var prev: texture_2d<f32>;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    return textureLoad(prev, vec2u(in.pos.xy), 0);
}
"#;

/// Round one's baseline (#67): the two new frames mixed half and half.
/// What moves between them shows twice, faintly (ghosting); what stands
/// still is unchanged.
const BLEND: &str = r#"
@group(0) @binding(0) var prev: texture_2d<f32>;
@group(0) @binding(1) var next: texture_2d<f32>;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let at = vec2u(in.pos.xy);
    return mix(textureLoad(prev, at, 0), textureLoad(next, at, 0), 0.5);
}
"#;

impl OnePass {
    fn new(device: &wgpu::Device, label: &'static str, shader: &str, reads_next: bool) -> Self {
        let entries = [crate::app::texture_entry(0, false), crate::app::texture_entry(1, false)];
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &entries[..if reads_next { 2 } else { 1 }],
        });
        let pipeline = crate::app::pipeline(device, label, shader, &layout, wgpu::TextureFormat::Rgba8Unorm);
        Self { label, reads_next, pipeline, layout }
    }

    pub fn copy(device: &wgpu::Device) -> Self {
        Self::new(device, "framegen copy", COPY, false)
    }

    pub fn blend(device: &wgpu::Device) -> Self {
        Self::new(device, "framegen blend", BLEND, true)
    }
}

impl Generator for OnePass {
    fn generate(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frames: Pair<'_>,
        times: Option<PassTimes<'_>>,
    ) -> bool {
        let entries = [
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(frames.prev) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(frames.next) },
        ];
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(self.label),
            layout: &self.layout,
            entries: &entries[..if self.reads_next { 2 } else { 1 }],
        });
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view: frames.out,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(self.label),
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
        Mode::Copy => Some(Box::new(OnePass::copy(device))),
        Mode::Blend => Some(Box::new(OnePass::blend(device))),
        Mode::NewOnly => None,
        Mode::FfxBase => Some(Box::new(ffx::Ffx::new(device, ffx::Quality::FAST))),
        Mode::Ffx => Some(Box::new(ffx::Ffx::new(device, ffx::Quality::BEST))),
        // The weights are embedded and checked by a test (rife.rs): a
        // failure here is a broken build, not a run-time condition.
        Mode::Rife => Some(Box::new(rife::Rife::new(device, rife::scale()).expect("the embedded RIFE weights load"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::duplicate::Tracker;
    use Kind::{Duplicate as D, New as N};

    /// The capture thread's cadence (run.rs): every card frame, at 60 Hz.
    struct Capture(Tracker, u64);

    impl Capture {
        fn new() -> Self {
            Self(Tracker::new(), 0)
        }
        fn push(&mut self, kind: Kind) -> Cadence {
            self.1 += 16_667;
            self.0.push(kind, Duration::from_micros(self.1));
            self.0.cadence()
        }
    }

    /// Kinds in at 60 Hz, never late: what the schedule shows.
    fn run(mode: Mode, kinds: &[Kind]) -> Vec<Show> {
        let (mut s, mut c) = (Schedule::new(mode), Capture::new());
        kinds.iter().map(|&k| s.next(k, c.push(k), false).show).collect()
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
    fn g_cycles_off_then_each_mode_in_order_then_off() {
        // Over a list passed in (Mode::ALL's order).
        let modes = [Mode::Copy, Mode::NewOnly];
        assert_eq!(cycle(None, &modes), Some(Mode::Copy));
        assert_eq!(cycle(Some(Mode::Copy), &modes), Some(Mode::NewOnly));
        assert_eq!(cycle(Some(Mode::NewOnly), &modes), None, "after the last: off");
        // A mode outside the list (a dev build's `--framegen blend`) steps
        // to the list's first.
        assert_eq!(cycle(Some(Mode::NewOnly), &[Mode::Copy]), Some(Mode::Copy));
        // Nothing listed: g stays off.
        assert_eq!(cycle(None, &[]), None);
        assert_eq!(cycle(Some(Mode::Copy), &[]), None);
        // What ships (#84): off, ffx, rife, off.
        let s = shipped();
        assert_eq!(cycle(None, &s), Some(Mode::Ffx));
        assert_eq!(cycle(Some(Mode::Ffx), &s), Some(Mode::Rife));
        assert_eq!(cycle(Some(Mode::Rife), &s), None);
        assert_eq!(cycle(Some(Mode::Blend), &s), Some(Mode::Ffx));
    }

    #[test]
    fn what_ships_is_generators_in_mode_alls_order() {
        // Every shipped mode runs a generator; copy (a test generator) and
        // new-only (for external layers) never ship.
        for m in Mode::ALL {
            assert!(!m.shipped() || m.generates(), "{m:?}: a shipped mode runs a generator");
        }
        let s = shipped();
        assert!(!s.contains(&Mode::Copy) && !s.contains(&Mode::NewOnly), "{s:?}");
        let order: Vec<usize> = s.iter().map(|m| Mode::ALL.iter().position(|a| a == m).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{s:?}");
    }

    #[test]
    fn every_generating_mode_follows_the_same_schedule() {
        let mut kinds = two_two(8);
        kinds.extend([N, D, D, N, N]);
        kinds.extend(two_two(8));
        for m in Mode::ALL.into_iter().filter(|m| m.generates()) {
            assert_eq!(run(m, &kinds), run(Mode::Copy, &kinds), "{m:?}");
            assert!(run(m, &kinds).contains(&Show::Generated), "{m:?}");
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
        let mut c = Capture::new();
        let mut next = |s: &mut Schedule, k| s.next(k, c.push(k), false).show;
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
        let (mut s, mut c) = (Schedule::new(Mode::Copy), Capture::new());
        for k in two_two(8) {
            s.next(k, c.push(k), false);
        }
        let step = s.next(N, c.push(N), true);
        assert_eq!(step, Step { show: Show::Real, missed: true });
        assert_eq!(s.next(D, c.push(D), false).show, Show::Real, "nothing held");
    }

    /// Card frames in, the capture thread seeing every one, the window
    /// taking all but the one at `skip` (the slot superseded it: run.rs
    /// `Slot::put`, which keeps a superseded new frame's kind on the frame
    /// that replaced it): what the window shows for each frame it took.
    fn window(mode: Mode, kinds: &[Kind], skip: Option<usize>) -> Vec<Step> {
        let (mut s, mut c) = (Schedule::new(mode), Capture::new());
        let mut waiting: Option<Kind> = None;
        let mut steps = Vec::new();
        for (i, &k) in kinds.iter().enumerate() {
            let cadence = c.push(k);
            if Some(i) == skip {
                waiting = Some(k);
                continue;
            }
            let taken = if waiting == Some(N) { N } else { k };
            let late = waiting.take().is_some();
            steps.push(s.next(taken, cadence, late));
        }
        steps
    }

    fn generated_and_missed(steps: &[Step]) -> usize {
        steps.iter().filter(|s| s.show == Show::Generated || s.missed).count()
    }

    /// Every mode that runs a generator: the schedule's rules are theirs.
    fn generating() -> impl Iterator<Item = Mode> {
        Mode::ALL.into_iter().filter(|m| m.generates())
    }

    #[test]
    fn a_superseded_duplicate_costs_one_generated_frame_and_is_counted() {
        for mode in generating() {
            let kinds = two_two(60);
            let all = generated_and_missed(&window(mode, &kinds, None));
            // A duplicate superseded mid-run (#78): the window takes the next
            // new frame a refresh late.
            let steps = window(mode, &kinds, Some(61));
            assert_eq!(generated_and_missed(&steps), all, "{mode:?}: {steps:?}");
            assert_eq!(steps.iter().filter(|s| s.missed).count(), 1, "{mode:?}: {steps:?}");
            // The new frame its generated frame held still shows, on the late
            // new frame's refresh, which is held in its turn. Card frames 58 to
            // 64, 61 superseded: Generated, Held, Generated, (61), Held (60's
            // new frame), Held (62's), Generated: 2:2 as before.
            let shows: Vec<Show> = steps[58..64].iter().map(|s| s.show).collect();
            assert_eq!(
                shows,
                [Show::Generated, Show::Held, Show::Generated, Show::Held, Show::Held, Show::Generated],
                "{mode:?}: {steps:?}"
            );
            assert!(steps[61].missed, "{mode:?}: the late new frame's generated frame is the one missed");
        }
    }

    #[test]
    fn a_superseded_new_frame_costs_one_generated_frame_and_is_counted() {
        for mode in generating() {
            let kinds = two_two(60);
            let all = generated_and_missed(&window(mode, &kinds, None));
            let steps = window(mode, &kinds, Some(60));
            assert_eq!(generated_and_missed(&steps), all, "{mode:?}: {steps:?}");
            assert_eq!(steps.iter().filter(|s| s.missed).count(), 1, "{mode:?}: {steps:?}");
        }
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
        let (hd, limits) = ((1920, 1080), wgpu::Limits::default());
        assert!(refusal(Mode::Copy, IntegratedGpu, "Intel UHD 620", hd, &limits).is_some_and(|m| m.contains("Intel UHD 620")));
        for d in [DiscreteGpu, VirtualGpu, Cpu, Other] {
            assert_eq!(refusal(Mode::Copy, d, "x", hd, &limits), None, "{d:?}");
        }
        assert_eq!(refusal(Mode::NewOnly, IntegratedGpu, "x", hd, &limits), None, "no generator, nothing refused");
    }

    #[test]
    fn rife_is_refused_a_picture_too_large_for_the_devices_buffers() {
        // wgpu's default limits: 128 MiB a storage binding (#74's finding).
        let limits = wgpu::Limits::default();
        let at = |size| refusal(Mode::Rife, wgpu::DeviceType::DiscreteGpu, "RX 9070 XT", size, &limits);
        assert_eq!(at((1920, 1080)), None, "1080p fits");
        assert_eq!(at((2560, 1440)), None, "1440p fits");
        let why = at((3840, 2160)).expect("4K needs 267 MB, over 128 MiB");
        assert!(why.contains("3840x2160") && why.contains("ffx works"), "{why}");
        // The same picture with ffx is not refused; nor rife where the
        // device binds enough.
        assert_eq!(refusal(Mode::Ffx, wgpu::DeviceType::DiscreteGpu, "x", (3840, 2160), &limits), None);
        let big = wgpu::Limits { max_storage_buffer_binding_size: 1 << 30, max_buffer_size: 1 << 30, ..limits };
        assert_eq!(refusal(Mode::Rife, wgpu::DeviceType::DiscreteGpu, "x", (3840, 2160), &big), None);
    }

    /// Two pictures with something different in every texel.
    fn pictures(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let picture = |seed: u32| -> Vec<u8> {
            (0..w * h).flat_map(|i| [(i.wrapping_mul(seed) >> 3) as u8, (i % w) as u8 * 4, (i / w) as u8 * 7, 255]).collect()
        };
        (picture(2_654_435_761), picture(40_503))
    }

    /// `mode`'s generator on the GPU over `prev` and `next` (`w`×`h`,
    /// `Rgba8Unorm`, the window's frame format): its output.
    fn generate_on_gpu(mode: Mode, (w, h): (u32, u32), prev_px: &[u8], next_px: &[u8]) -> Vec<u8> {
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
        let (prev, next) = (source(prev_px), source(next_px));
        let out = texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let views = [&prev, &next, &out].map(|t| t.create_view(&Default::default()));
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(w * h * 4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut generator = generator(mode, &device).expect("the mode generates");
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
        readback.get_mapped_range(..).unwrap().to_vec()
    }

    /// The copy generator on the GPU: its output is the previous new
    /// frame, texel for texel: `cargo test -- --ignored gpu_`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_copy_outputs_the_previous_frame() {
        let (prev, next) = pictures(64, 36);
        let got = generate_on_gpu(Mode::Copy, (64, 36), &prev, &next);
        assert!(got == prev, "the previous frame, texel for texel");
        assert!(got != next);
    }

    /// The blend generator on the GPU (#67): every texel the mean of the
    /// two frames', within one step of 1/255 (the rounding of a unorm
    /// write); a texel the same in both stays as it was.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_blend_outputs_the_mean_of_the_two_frames() {
        let (w, h) = (64, 36);
        let (prev, mut next) = pictures(w, h);
        // A still corner: the same in both frames.
        next[..16].copy_from_slice(&prev[..16]);
        let got = generate_on_gpu(Mode::Blend, (w, h), &prev, &next);
        assert_eq!(got.len(), prev.len());
        let mut moved = 0;
        for (i, ((&g, &a), &b)) in got.iter().zip(&prev).zip(&next).enumerate() {
            let mean = (f64::from(a) + f64::from(b)) / 2.0;
            assert!((f64::from(g) - mean).abs() <= 1.0, "texel byte {i}: {g}, the mean of {a} and {b} is {mean}");
            moved += usize::from(a != b);
        }
        // The pictures differ in their red byte, a quarter of all bytes.
        assert!(moved > prev.len() / 8, "the pictures differ, so the test can see a blend: {moved}");
        assert_eq!(got[..16], prev[..16], "what stands still is unchanged");
    }

    #[test]
    fn the_flag_names_every_mode() {
        for m in Mode::ALL {
            assert_eq!(Mode::from_flag(m.as_str()), Ok(m));
        }
        assert!(Mode::from_flag("warp").unwrap_err().contains("copy or blend or new-only or ffx-base or ffx or rife"));
    }

    #[test]
    fn framegen_names_off_and_the_shipped_modes_only() {
        assert_eq!(shipped(), [Mode::Ffx, Mode::Rife]);
        assert_eq!(Mode::from_setting("off"), Ok(None));
        assert_eq!(Mode::from_setting("ffx"), Ok(Some(Mode::Ffx)));
        assert_eq!(Mode::from_setting("rife"), Ok(Some(Mode::Rife)));
        for dev in ["blend", "ffx-base", "copy", "new-only", "ffx-quality", ""] {
            assert_eq!(Mode::from_setting(dev), Err(format!("FRAMEGEN={dev}: use off, ffx, rife")), "{dev:?}");
        }
        for m in [None, Some(Mode::Ffx), Some(Mode::Rife)] {
            assert_eq!(Mode::from_setting(Mode::setting(m)), Ok(m), "{m:?}");
        }
        // A dev build's --framegen blend is not a FRAMEGEN value: w saves off.
        assert_eq!(Mode::setting(Some(Mode::Blend)), "off");
    }
}
