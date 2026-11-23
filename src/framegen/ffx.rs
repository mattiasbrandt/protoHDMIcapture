//! The `ffx` generator (#68): AMD FidelityFX Optical Flow (SDK v1.1.4,
//! MIT; the WGSL ports in `ffx/` keep AMD's notice) finds where each 8x8
//! block of the new frame was in the previous one, and an interpolation
//! pass after afmf-linux's (MIT) warps both frames halfway along that flow.
//!
//! Per generated frame, on the GPU, in the SDK's order:
//! - **Luma** of both frames at half resolution for `ffx` (afmf-linux's
//!   choice: a 1080p frame's flow at 960x540, ~0.6 ms a frame at 3440x1440
//!   on RADV by its own measurement), at full resolution for `ffx-quality`
//!   (`Quality`), and a pyramid of up to 7 levels, each the
//!   2x2 mean of the one above (the SDK's single-pass downsampler computes
//!   the same means).
//! - **Scene change**: nine strata histograms of each frame and their
//!   divergence; over the SDK's 0.45 the pair is a cut, and every later
//!   pass sees it.
//! - **Flow**, coarsest level first: block search around the coarser
//!   level's vector (+-8 pixels; `ffx-quality` +-16 above level 0), a 3x3
//!   vector median, and the scale-up that seeds the next finer level;
//!   `ffx-quality` then refines level 0's 8x8 vectors to 4x4 blocks and
//!   takes their median again.
//! - **Interpolation** into the output, a render pass at the frames' size;
//!   `ffx-quality` searches the flow both ways (the previous frame's
//!   blocks into the new frame too) and interpolates knowing which frame
//!   sees a region (interpolate_two.wgsl), and keeps blocks of still HUD
//!   (hud_mask.wgsl, after openfg's static-UI mask) blended unmoved.
//!
//! What differs from the SDK, and why:
//! - **No wave (subgroup) operations.** The SDK reduces with wave sums and
//!   minimums and leaves the last reduction steps to wave lockstep; here
//!   workgroup atomics and barriers do it, so the port runs on any GPU wgpu
//!   drives with no device feature to ask for and nothing to refuse.
//! - **Both frames every time.** The SDK keeps the previous frame's luma
//!   and histograms from its own last call. The seam calls a generator only
//!   for a pair it shows generated (not on cadence breaks), so the last
//!   call's frame need not be this pair's previous one: both frames' luma
//!   and histograms are made from the pair itself (two cheap passes more).
//!   For the same reason the scene change is this pair's alone (history
//!   bit 0), where the SDK holds a cut for four frames.
//! - **A cut is applied on the GPU.** The new frame is converted in the
//!   same command encoder the generator records into, unsubmitted, so the
//!   cut cannot be known when `generate` returns. The interpolation pass
//!   then writes the new frame itself, as the seam's real frame would show
//!   it: the same picture at the same time (the held duplicate after it is
//!   that new frame again). So `generate` returns true; only the dashboard
//!   counts the pair as generated.
//! - **Buffers instead of images**, and out-of-range reads return zero as
//!   the SDK's image loads do; the divergence pass writes this frame's
//!   histograms to their own buffer (copied after the pass) instead of
//!   overwriting what other workgroups of the same dispatch still read.

use crate::framegen::{Generator, Pair};
use crate::gputime::PassTimes;

/// The SDK's pyramid depth (`advancedAlgorithmIterations`).
const MAX_LEVELS: u32 = 7;
/// Pixels a side of a flow block, at every level.
const BLOCK: u32 = 8;
/// How hard the generator works: `ffx` (#68) as it was measured, and
/// `ffx-quality` (#73), which spends more of a gaming GPU's headroom on
/// the picture, each step measured (`gpu_ffx_time_at_1080p`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    /// Frame pixels per flow-luma pixel: 2 is afmf-linux's half
    /// resolution, 1 the flow at the frame's own resolution (#73 step 1:
    /// a block is 8 frame pixels, not 16, so motion edges are twice as
    /// fine).
    pub scale: u32,
    /// The block search's window, +-pixels, at every level above 0: 8 is
    /// the SDK's (search.wgsl); 16 (search_wide.wgsl, #73 step 2) catches
    /// a block whose motion the coarser level's vector, or the top level's
    /// zero, misses by up to 16. Level 0 keeps the SDK's 8: it starts from
    /// level 1's vector, rarely more than a pixel or two out, and holds
    /// most of the blocks (16 there was 3.3 ms of ffx-quality's 3.6 at
    /// 1080p, against 0.75 ms at the levels above).
    pub radius: i32,
    /// Level 0's 8x8 vectors refined to 4x4 blocks (refine.wgsl, #73 step
    /// 2): a block that straddles a motion edge no longer drags one side
    /// along with the other.
    pub refine: bool,
    /// The flow both ways, and an interpolation that knows which frame
    /// sees a region (interpolate_two.wgsl, #73 step 3): where an object
    /// uncovers or covers the background, the frame that shows it is
    /// used alone, instead of a blend of the background with the object
    /// (the halo around moving edges).
    pub two_way: bool,
    /// The static-HUD mask (hud_mask.wgsl, after openfg's, #73 step 4):
    /// blocks of still, high-contrast detail (HUD text, a minimap) are
    /// blended unmoved, not warped with the background moving behind
    /// them. With `two_way` only (its interpolation reads the mask).
    pub hud_mask: bool,
}

impl Quality {
    /// `ffx`: #68's generator, unchanged.
    pub const FAST: Self = Self { scale: 2, radius: 8, refine: false, two_way: false, hud_mask: false };
    /// `ffx-quality`.
    pub const BEST: Self = Self { scale: 1, radius: 16, refine: true, two_way: true, hud_mask: true };
}

/// New-frame pairs a block must stay static HUD in a row before it is
/// masked: a background that stops for a moment is not a HUD.
const HUD_FRAMES: u32 = 3;
/// The coarsest level is at least this many luma pixels on its short side
/// (the search reads 4-pixel words and 8x8 blocks).
const MIN_LEVEL_SIDE: u32 = 8;
/// Motion beyond this (frame pixels between the two frames) is not
/// believed and blends the frames unmoved: afmf-linux's value, half of
/// what its search reaches at half resolution (`ffx`).
const MAX_MOTION: f32 = 128.0;
/// `ffx-quality`'s: its search, at full resolution with a 16-pixel window
/// above level 0, reaches about 1000 frame pixels, and 128 threw away
/// the motions of a fast camera turn it had found and both flows agreed
/// on (#73, the driving clip).
const MAX_MOTION_TWO_WAY: f32 = 256.0;
/// A pixel equal in both frames within this on a moving block is a HUD and
/// is kept (afmf-linux's default, one 8-bit step).
const HUD: f32 = 1.0 / 255.0;
/// The scene-change buffer's slots (common.wgsl).
const SCD_SLOTS: u64 = 8;
/// Nine histograms of 256 bins.
const HISTOGRAM_BINS: u64 = 9 * 256;

const COMMON: &str = include_str!("ffx/common.wgsl");

/// The pyramid's level count for a flow-luma size.
fn levels(src: (u32, u32)) -> u32 {
    let side = src.0.min(src.1);
    (1..=MAX_LEVELS).rev().find(|&n| side >> (n - 1) >= MIN_LEVEL_SIDE).unwrap_or(1)
}

/// A level's flow size, as the SDK's `GetOpticalFlowTextureSize` and halving.
fn flow_sizes(src: (u32, u32), levels: u32) -> Vec<(u32, u32)> {
    let mut sizes = vec![(src.0.div_ceil(BLOCK), src.1.div_ceil(BLOCK))];
    for _ in 1..levels {
        let (w, h) = *sizes.last().expect("level 0");
        sizes.push((w.div_ceil(2), h.div_ceil(2)));
    }
    sizes
}

/// `Params` (common.wgsl) as bytes.
fn params(luma: (u32, u32), of: (u32, u32), next_of: (u32, u32), src: (u32, u32), level: u32, levels: u32, scale: u32) -> Vec<u8> {
    let i = |v: u32| (v as i32).to_le_bytes();
    [i(luma.0), i(luma.1), i(of.0), i(of.1), i(next_of.0), i(next_of.1), i(src.0), i(src.1)]
        .into_iter()
        .chain([level, levels, scale, 0].map(u32::to_le_bytes))
        .flatten()
        .collect()
}

fn uniform(device: &wgpu::Device, label: &str, bytes: &[u8]) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: true,
    });
    buffer.slice(..).get_mapped_range_mut().expect("mapped at creation").copy_from_slice(bytes);
    buffer.unmap();
    buffer
}

fn storage(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn compute(device: &wgpu::Device, label: &str, source: &str) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(format!("{COMMON}{source}").into()),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    })
}

fn bind(device: &wgpu::Device, pipeline: &wgpu::ComputePipeline, label: &str, resources: &[wgpu::BindingResource<'_>]) -> wgpu::BindGroup {
    let entries: Vec<_> =
        resources.iter().enumerate().map(|(i, r)| wgpu::BindGroupEntry { binding: i as u32, resource: r.clone() }).collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &entries,
    })
}

impl Pipelines {
    /// The search a level runs: the wide one above level 0, where there is
    /// one.
    fn search_at(&self, level: usize) -> &wgpu::ComputePipeline {
        match &self.search_wide {
            Some(wide) if level > 0 => wide,
            _ => &self.search,
        }
    }
}

struct Pipelines {
    luma: wgpu::ComputePipeline,
    down: wgpu::ComputePipeline,
    histogram: wgpu::ComputePipeline,
    divergence: wgpu::ComputePipeline,
    search: wgpu::ComputePipeline,
    /// `Quality::radius` above 8: the search at the levels above 0.
    search_wide: Option<wgpu::ComputePipeline>,
    filter: wgpu::ComputePipeline,
    scale: wgpu::ComputePipeline,
    refine: wgpu::ComputePipeline,
    /// `Quality::hud_mask`: the detector and its dilation.
    hud: Option<(wgpu::ComputePipeline, wgpu::ComputePipeline)>,
    interpolate: wgpu::RenderPipeline,
    interpolate_layout: wgpu::BindGroupLayout,
    /// `Quality::two_way`: interpolate_two.wgsl, and its layout (the
    /// backward flow at binding 6).
    interpolate_two: Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
    sampler: wgpu::Sampler,
}

/// The buffers for one frame size, and the bind groups that do not change
/// from call to call.
struct State {
    /// The frames' size.
    size: (u32, u32),
    /// Level 0's luma size.
    src: (u32, u32),
    luma_sizes: Vec<(u32, u32)>,
    flow_sizes: Vec<(u32, u32)>,
    /// Per level: [0] the new frame's luma, [1] the previous frame's.
    luma: [Vec<wgpu::Buffer>; 2],
    histogram: wgpu::Buffer,
    previous_histogram: wgpu::Buffer,
    filtered_histogram: wgpu::Buffer,
    scd: wgpu::Buffer,
    /// Level 0's parameters (the luma pass's).
    luma_params: wgpu::Buffer,
    interpolate_params: wgpu::Buffer,
    /// Per frame, per level from 1: the 2x2 reduction into it.
    down: [Vec<wgpu::BindGroup>; 2],
    /// Per frame: its histograms.
    histograms: [wgpu::BindGroup; 2],
    divergence: wgpu::BindGroup,
    /// The flow from the new frame back to the previous one, and with
    /// `Quality::two_way` from the previous frame on to the new one.
    dirs: Vec<Direction>,
    /// `Quality::hud_mask`'s buffers, kept from call to call (the counts
    /// are the mask's history).
    hud: Option<HudState>,
    /// Bound as the mask when there is none (two-way without hud_mask).
    no_mask: wgpu::Buffer,
}

struct HudState {
    blocks: (u32, u32),
    params: wgpu::Buffer,
    counts: wgpu::Buffer,
    mask: wgpu::Buffer,
    dilated: wgpu::Buffer,
    dilate: wgpu::BindGroup,
}

/// One direction's flow: the search from one frame's luma into the
/// other's, level by level. Forward (the SDK's): the new frame's blocks,
/// previous = new + v. Backward (#73 step 3): the previous frame's
/// blocks, new = previous + w.
struct Direction {
    /// Per level: the filtered vectors; level 0's is the flow.
    flow_filtered: Vec<wgpu::Buffer>,
    search: Vec<wgpu::BindGroup>,
    filter: Vec<wgpu::BindGroup>,
    /// Per level from 1: the scale-up into the level below.
    scale: Vec<wgpu::BindGroup>,
    /// With `Quality::refine`: the 4x4 flow's size, its refinement and
    /// median, and the buffer the interpolation reads.
    fine: Option<Fine>,
    /// The previous call's level-0 flow, the search's temporal candidate.
    history: wgpu::Buffer,
}

impl Direction {
    /// The flow the interpolation reads: the 4x4 one when refined
    /// (`interpolate_params` says its size and block).
    fn output(&self) -> &wgpu::Buffer {
        self.fine.as_ref().map_or(&self.flow_filtered[0], |f| &f.flow)
    }
}

/// Level 0's flow at 4x4 blocks (`Quality::refine`).
struct Fine {
    size: (u32, u32),
    refine: wgpu::BindGroup,
    filter: wgpu::BindGroup,
    flow: wgpu::Buffer,
}

impl State {
    fn new(device: &wgpu::Device, p: &Pipelines, size: (u32, u32), q: Quality) -> Self {
        let luma_scale = q.scale;
        let src = (size.0.div_ceil(luma_scale), size.1.div_ceil(luma_scale));
        let n = levels(src);
        let luma_sizes: Vec<_> = (0..n).map(|l| ((src.0 >> l).max(1), (src.1 >> l).max(1))).collect();
        let flow_sizes = flow_sizes(src, n);
        let luma_buffers = |which: &str| -> Vec<wgpu::Buffer> {
            luma_sizes
                .iter()
                .enumerate()
                .map(|(l, (w, h))| storage(device, &format!("ffx luma {which} {l}"), u64::from(w * h) * 4))
                .collect()
        };
        let luma = [luma_buffers("new"), luma_buffers("previous")];
        let histogram = storage(device, "ffx histogram", HISTOGRAM_BINS * 4);
        let previous_histogram = storage(device, "ffx previous histogram", HISTOGRAM_BINS * 4);
        let filtered_histogram = storage(device, "ffx filtered histogram", HISTOGRAM_BINS * 4);
        let scd = storage(device, "ffx scene change", SCD_SLOTS * 4);
        let level_params: Vec<_> = (0..n as usize)
            .map(|l| {
                let next_of = if l > 0 { flow_sizes[l - 1] } else { (0, 0) };
                // The pyramid pass reads the finer level: `src` is it.
                let finer = if l > 0 { luma_sizes[l - 1] } else { src };
                let bytes = params(luma_sizes[l], flow_sizes[l], next_of, src, l as u32, n, luma_scale);
                let down = params(luma_sizes[l], flow_sizes[l], next_of, finer, l as u32, n, luma_scale);
                (uniform(device, &format!("ffx level {l}"), &bytes), uniform(device, &format!("ffx down {l}"), &down))
            })
            .collect();
        let luma_params = uniform(device, "ffx luma", &params(src, flow_sizes[0], (0, 0), src, 0, n, luma_scale));
        let (out_size, out_block) = match q.refine {
            true => ((src.0.div_ceil(4), src.1.div_ceil(4)), 4 * luma_scale),
            false => (flow_sizes[0], BLOCK * luma_scale),
        };
        let interpolate_params = {
            let f = |v: f32| v.to_le_bytes();
            let i = |v: u32| (v as i32).to_le_bytes();
            let bytes: Vec<u8> = [
                f(size.0 as f32),
                f(size.1 as f32),
                f(out_block as f32),
                f(luma_scale as f32),
                i(out_size.0),
                i(out_size.1),
                f(if q.two_way { MAX_MOTION_TWO_WAY } else { MAX_MOTION }),
                f(HUD),
                i(size.0.div_ceil(8)),
                i(size.1.div_ceil(8)),
                (q.hud_mask as u32).to_le_bytes(),
                0u32.to_le_bytes(),
            ]
            .concat();
            uniform(device, "ffx interpolate", &bytes)
        };
        fn b(buffer: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
            buffer.as_entire_binding()
        }
        let down = [0, 1].map(|f| {
            (1..n as usize)
                .map(|l| bind(device, &p.down, "ffx down", &[b(&level_params[l].1), b(&luma[f][l - 1]), b(&luma[f][l])]))
                .collect()
        });
        let histograms =
            [0, 1].map(|f| bind(device, &p.histogram, "ffx histogram", &[b(&level_params[0].0), b(&luma[f][0]), b(&histogram)]));
        let divergence =
            bind(device, &p.divergence, "ffx divergence", &[b(&histogram), b(&previous_histogram), b(&filtered_histogram), b(&scd)]);
        // Forward searches the new frame's luma (0) in the previous one's
        // (1); backward the other way round.
        let dirs = [(0, 1, "forward"), (1, 0, "backward")][..if q.two_way { 2 } else { 1 }]
            .iter()
            .map(|&(cur, prev, which)| {
                let flows = |what: &str| -> Vec<wgpu::Buffer> {
                    flow_sizes
                        .iter()
                        .enumerate()
                        .map(|(l, (w, h))| storage(device, &format!("ffx flow {which} {what} {l}"), u64::from(w * h) * 8))
                        .collect()
                };
                // The search's vectors per level (seeded by the coarser
                // level): held by the bind groups below, read by nothing
                // else.
                let (flow_searched, flow_filtered) = (flows("searched"), flows("filtered"));
                // The previous pair's level-0 flow (search.wgsl's
                // TEMPORAL), copied from this one's after each call.
                let history = storage(device, &format!("ffx flow {which} history"), u64::from(flow_sizes[0].0 * flow_sizes[0].1) * 8);
                let search = (0..n as usize)
                    .map(|l| {
                        let mut resources = vec![b(&level_params[l].0), b(&luma[cur][l]), b(&luma[prev][l]), b(&flow_searched[l]), b(&scd)];
                        // search.wgsl reads the history; search_wide.wgsl
                        // has no such binding.
                        if l == 0 || p.search_wide.is_none() {
                            resources.push(b(&history));
                        }
                        bind(device, p.search_at(l), "ffx search", &resources)
                    })
                    .collect();
                let filter = (0..n as usize)
                    .map(|l| bind(device, &p.filter, "ffx filter", &[b(&level_params[l].0), b(&flow_searched[l]), b(&flow_filtered[l])]))
                    .collect();
                let scale = (1..n as usize)
                    .map(|l| {
                        bind(
                            device,
                            &p.scale,
                            "ffx scale",
                            &[
                                b(&level_params[l].0),
                                b(&luma[cur][l]),
                                b(&luma[prev][l]),
                                b(&flow_filtered[l]),
                                b(&flow_searched[l - 1]),
                                b(&scd),
                            ],
                        )
                    })
                    .collect();
                let fine = q.refine.then(|| {
                    let fine_size = (src.0.div_ceil(4), src.1.div_ceil(4));
                    let raw = storage(device, &format!("ffx fine {which} searched"), u64::from(fine_size.0 * fine_size.1) * 8);
                    let flow = storage(device, &format!("ffx fine {which}"), u64::from(fine_size.0 * fine_size.1) * 8);
                    // Refinement reads level 0's luma and 8x8 flow, writes
                    // next_of.
                    let refine_params = uniform(device, "ffx refine", &params(src, flow_sizes[0], fine_size, src, 0, n, luma_scale));
                    // The median runs on the 4x4 flow as on a level's.
                    let filter_params = uniform(device, "ffx fine filter", &params(src, fine_size, (0, 0), src, 0, n, luma_scale));
                    Fine {
                        size: fine_size,
                        refine: bind(
                            device,
                            &p.refine,
                            "ffx refine",
                            &[b(&refine_params), b(&luma[cur][0]), b(&luma[prev][0]), b(&flow_filtered[0]), b(&raw)],
                        ),
                        filter: bind(device, &p.filter, "ffx fine filter", &[b(&filter_params), b(&raw), b(&flow)]),
                        flow,
                    }
                });
                Direction { flow_filtered, search, filter, scale, fine, history }
            })
            .collect();
        let hud = q.hud_mask.then(|| {
            let blocks = (size.0.div_ceil(8), size.1.div_ceil(8));
            let i = |v: u32| (v as i32).to_le_bytes();
            let bytes: Vec<u8> =
                [i(size.0), i(size.1), i(blocks.0), i(blocks.1), HUD_FRAMES.to_le_bytes(), [0; 4], [0; 4], [0; 4]].concat();
            let params = uniform(device, "ffx hud", &bytes);
            let cells = u64::from(blocks.0 * blocks.1) * 4;
            let (counts, mask, dilated) = (
                storage(device, "ffx hud counts", cells),
                storage(device, "ffx hud mask", cells),
                storage(device, "ffx hud dilated", cells),
            );
            let (_, dilate_pipeline) = p.hud.as_ref().expect("made with hud_mask");
            let dilate = bind(device, dilate_pipeline, "ffx hud dilate", &[b(&params), b(&mask), b(&dilated)]);
            HudState { blocks, params, counts, mask, dilated, dilate }
        });
        let no_mask = storage(device, "ffx no hud mask", 4);
        Self {
            size,
            src,
            luma_sizes,
            flow_sizes,
            luma,
            histogram,
            previous_histogram,
            filtered_histogram,
            scd,
            luma_params,
            interpolate_params,
            down,
            histograms,
            divergence,
            dirs,
            hud,
            no_mask,
        }
    }
}

/// The generator: its pipelines, and its buffers once it knows the size.
pub struct Ffx {
    quality: Quality,
    pipelines: Pipelines,
    state: Option<State>,
}

impl Ffx {
    pub fn new(device: &wgpu::Device, quality: Quality) -> Self {
        let interpolate_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ffx interpolate"),
            entries: &[
                crate::app::uniform_entry(0),
                crate::app::texture_entry(1, true),
                crate::app::texture_entry(2, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                read_only_storage(4),
                read_only_storage(5),
            ],
        });
        let interpolate = crate::app::pipeline(
            device,
            "ffx interpolate",
            &format!("{COMMON}{}", include_str!("ffx/interpolate.wgsl")),
            &interpolate_layout,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let interpolate_two = quality.two_way.then(|| {
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ffx interpolate two-way"),
                entries: &[
                    crate::app::uniform_entry(0),
                    crate::app::texture_entry(1, true),
                    crate::app::texture_entry(2, true),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    read_only_storage(4),
                    read_only_storage(5),
                    read_only_storage(6),
                    read_only_storage(7),
                ],
            });
            let pipeline = crate::app::pipeline(
                device,
                "ffx interpolate two-way",
                &format!("{COMMON}{}", include_str!("ffx/interpolate_two.wgsl")),
                &layout,
                wgpu::TextureFormat::Rgba8Unorm,
            );
            (pipeline, layout)
        });
        let pipelines = Pipelines {
            luma: compute(device, "ffx luma", include_str!("ffx/luma.wgsl")),
            down: compute(device, "ffx down", include_str!("ffx/down.wgsl")),
            histogram: compute(device, "ffx histogram", include_str!("ffx/histogram.wgsl")),
            divergence: compute(device, "ffx divergence", include_str!("ffx/divergence.wgsl")),
            // ffx-quality: level 0 also tries the previous pair's vector
            // (search.wgsl's TEMPORAL).
            search: compute(
                device,
                "ffx search",
                &format!("const TEMPORAL: bool = {};\n{}", quality.two_way, include_str!("ffx/search.wgsl")),
            ),
            search_wide: (quality.radius != 8).then(|| {
                compute(
                    device,
                    "ffx search wide",
                    &format!("const RADIUS: i32 = {};\n{}", quality.radius, include_str!("ffx/search_wide.wgsl")),
                )
            }),
            filter: compute(device, "ffx filter", include_str!("ffx/filter.wgsl")),
            scale: compute(device, "ffx scale", include_str!("ffx/scale.wgsl")),
            refine: compute(device, "ffx refine", include_str!("ffx/refine.wgsl")),
            hud: quality.hud_mask.then(|| {
                (
                    compute(device, "ffx hud", include_str!("ffx/hud_mask.wgsl")),
                    compute(device, "ffx hud dilate", include_str!("ffx/hud_dilate.wgsl")),
                )
            }),
            interpolate,
            interpolate_layout,
            interpolate_two,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ffx"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
        };
        Self { quality, pipelines, state: None }
    }

    /// The flow from `next` back to `prev` and the scene-change test, into
    /// the state's buffers (level 0's filtered flow, the scene-change
    /// slots).
    fn flow(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        prev: &wgpu::TextureView,
        next: &wgpu::TextureView,
        times: Option<&PassTimes<'_>>,
    ) {
        let size = next.texture().size();
        let size = (size.width, size.height);
        if self.state.as_ref().is_none_or(|s| s.size != size) {
            self.state = Some(State::new(device, &self.pipelines, size, self.quality));
        }
        let (p, s) = (&self.pipelines, self.state.as_ref().expect("made above"));
        let n = s.luma_sizes.len();
        let groups = |(w, h): (u32, u32), gx: u32, gy: u32| (w.div_ceil(gx), h.div_ceil(gy));
        // This pair's scene change alone; the histograms start empty.
        encoder.clear_buffer(&s.scd, 0, None);
        encoder.clear_buffer(&s.histogram, 0, None);
        // The frames change from call to call: their bind groups too.
        let luma_groups = [(next, 0), (prev, 1)].map(|(frame, f)| {
            bind(
                device,
                &p.luma,
                "ffx luma",
                &[s.luma_params.as_entire_binding(), wgpu::BindingResource::TextureView(frame), s.luma[f][0].as_entire_binding()],
            )
        });
        let histogram_groups = groups((s.src.0 / 4 / 3, 1), 32, 1);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ffx pyramid"),
                timestamp_writes: times.and_then(|t| t.compute(true, false)),
            });
            for f in 0..2 {
                pass.set_pipeline(&p.luma);
                pass.set_bind_group(0, &luma_groups[f], &[]);
                let (gx, gy) = groups(s.src, 8, 8);
                pass.dispatch_workgroups(gx, gy, 1);
                pass.set_pipeline(&p.down);
                for l in 1..n {
                    pass.set_bind_group(0, &s.down[f][l - 1], &[]);
                    let (gx, gy) = groups(s.luma_sizes[l], 8, 8);
                    pass.dispatch_workgroups(gx, gy, 1);
                }
            }
            // The previous frame's histograms first: they become the ones
            // the new frame's are measured against.
            pass.set_pipeline(&p.histogram);
            pass.set_bind_group(0, &s.histograms[1], &[]);
            pass.dispatch_workgroups(histogram_groups.0.max(1), 16, 9);
            pass.set_pipeline(&p.divergence);
            pass.set_bind_group(0, &s.divergence, &[]);
            pass.dispatch_workgroups(9, 3, 1);
        }
        encoder.copy_buffer_to_buffer(&s.filtered_histogram, 0, &s.previous_histogram, 0, HISTOGRAM_BINS * 4);
        encoder.clear_buffer(&s.histogram, 0, None);
        // The HUD detector reads this pair's frames: bound per call.
        let hud = s.hud.as_ref().map(|h| {
            let (detect, _) = p.hud.as_ref().expect("made with hud_mask");
            let group = bind(
                device,
                detect,
                "ffx hud",
                &[
                    h.params.as_entire_binding(),
                    wgpu::BindingResource::TextureView(prev),
                    wgpu::BindingResource::TextureView(next),
                    h.counts.as_entire_binding(),
                    h.mask.as_entire_binding(),
                ],
            );
            (h, group)
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("ffx flow"), timestamp_writes: None });
        pass.set_pipeline(&p.histogram);
        pass.set_bind_group(0, &s.histograms[0], &[]);
        pass.dispatch_workgroups(histogram_groups.0.max(1), 16, 9);
        pass.set_pipeline(&p.divergence);
        pass.set_bind_group(0, &s.divergence, &[]);
        pass.dispatch_workgroups(9, 3, 1);
        for d in &s.dirs {
            for l in (0..n).rev() {
                pass.set_pipeline(p.search_at(l));
                pass.set_bind_group(0, &d.search[l], &[]);
                let (gx, gy) = groups(s.luma_sizes[l], 16, 16);
                pass.dispatch_workgroups(gx, gy, 1);
                pass.set_pipeline(&p.filter);
                pass.set_bind_group(0, &d.filter[l], &[]);
                let (gx, gy) = groups(s.flow_sizes[l], 16, 4);
                pass.dispatch_workgroups(gx, gy, 1);
                if l > 0 {
                    pass.set_pipeline(&p.scale);
                    pass.set_bind_group(0, &d.scale[l - 1], &[]);
                    let (gx, gy) = groups(s.flow_sizes[l - 1], 4, 4);
                    pass.dispatch_workgroups(gx, gy, 1);
                }
            }
            if let Some(f) = &d.fine {
                pass.set_pipeline(&p.refine);
                pass.set_bind_group(0, &f.refine, &[]);
                let (gx, gy) = groups(f.size, 8, 8);
                pass.dispatch_workgroups(gx, gy, 1);
                pass.set_pipeline(&p.filter);
                pass.set_bind_group(0, &f.filter, &[]);
                let (gx, gy) = groups(f.size, 16, 4);
                pass.dispatch_workgroups(gx, gy, 1);
            }
        }
        if let (Some((h, group)), Some((detect, dilate))) = (&hud, &p.hud) {
            let (gx, gy) = groups(h.blocks, 8, 8);
            pass.set_pipeline(detect);
            pass.set_bind_group(0, group, &[]);
            pass.dispatch_workgroups(gx, gy, 1);
            pass.set_pipeline(dilate);
            pass.set_bind_group(0, &h.dilate, &[]);
            pass.dispatch_workgroups(gx, gy, 1);
        }
        drop(pass);
        // This pair's level-0 flow is the next call's temporal candidate.
        for d in &s.dirs {
            encoder.copy_buffer_to_buffer(&d.flow_filtered[0], 0, &d.history, 0, d.history.size());
        }
    }
}

fn read_only_storage(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl Generator for Ffx {
    fn generate(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frames: Pair<'_>,
        times: Option<PassTimes<'_>>,
    ) -> bool {
        self.flow(device, encoder, frames.prev, frames.next, times.as_ref());
        let (p, s) = (&self.pipelines, self.state.as_ref().expect("made by flow"));
        let mut entries = vec![
            wgpu::BindGroupEntry { binding: 0, resource: s.interpolate_params.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(frames.prev) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(frames.next) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&p.sampler) },
            wgpu::BindGroupEntry { binding: 4, resource: s.dirs[0].output().as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: s.scd.as_entire_binding() },
        ];
        // Two-way: the backward flow too, and its interpolation.
        let (pipeline, layout) = match (&p.interpolate_two, s.dirs.get(1)) {
            (Some((pipeline, layout)), Some(backward)) => {
                entries.push(wgpu::BindGroupEntry { binding: 6, resource: backward.output().as_entire_binding() });
                let mask = s.hud.as_ref().map_or(&s.no_mask, |h| &h.dilated);
                entries.push(wgpu::BindGroupEntry { binding: 7, resource: mask.as_entire_binding() });
                (pipeline, layout)
            }
            _ => (&p.interpolate, &p.interpolate_layout),
        };
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("ffx interpolate"), layout, entries: &entries });
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view: frames.out,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ffx interpolate"),
            color_attachments: &attachments,
            depth_stencil_attachment: None,
            timestamp_writes: times.as_ref().and_then(|t| t.render(false, true)),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

/// On the GPU (`cargo test -- --ignored ffx`): a picture moved by a known
/// offset, and two unrelated pictures.
#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(features: wgpu::Features) -> (wgpu::Device, wgpu::Queue) {
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
        let features = features & adapter.features();
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features: features, ..Default::default() })).unwrap()
    }

    /// A hash of an integer pair: the colour of one 8x8 block.
    fn hash(x: i32, y: i32, seed: u32) -> u32 {
        let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841) ^ seed.wrapping_mul(0xcb1a_b31f);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1_e995);
        h ^ (h >> 15)
    }

    /// RGBA pixels of a w x h picture of 8x8 blocks, its content moved by
    /// `by` pixels: pixel (x, y) shows the plane's (x - by.0, y - by.1).
    /// Each channel in [lo, lo + span).
    fn picture(w: u32, h: u32, by: (i32, i32), seed: u32, lo: u32, span: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (u, v) = ((x - by.0).div_euclid(8), (y - by.1).div_euclid(8));
                let c = hash(u, v, seed);
                for k in 0..3 {
                    px.push((lo + ((c >> (k * 8)) & 0xff) % span) as u8);
                }
                px.push(255);
            }
        }
        px
    }

    /// RGBA pixels of a w x h picture of per-pixel noise, its content moved
    /// by `by` pixels; `keep` (a column range) shows the unmoved plane
    /// instead (a region that does not move).
    fn noise(w: u32, h: u32, by: (i32, i32), seed: u32, keep: std::ops::Range<i32>) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (u, v) = if keep.contains(&x) { (x, y) } else { (x - by.0, y - by.1) };
                let c = hash(u, v, seed);
                px.extend_from_slice(&[(c & 0xff) as u8, ((c >> 8) & 0xff) as u8, ((c >> 16) & 0xff) as u8, 255]);
            }
        }
        px
    }

    /// A textured object (48x48, its own noise moving with it) with its
    /// top-left at `at`, over a static textured background.
    fn scene(w: u32, h: u32, at: (i32, i32)) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (ox, oy) = (x - at.0, y - at.1);
                let c = if (0..48).contains(&ox) && (0..48).contains(&oy) { hash(ox, oy, 31) } else { hash(x, y, 17) };
                px.extend_from_slice(&[(c & 0xff) as u8, ((c >> 8) & 0xff) as u8, ((c >> 16) & 0xff) as u8, 255]);
            }
        }
        px
    }

    /// The halo metric: the mean absolute error, 8-bit levels averaged over
    /// the colour channels, against the true halfway picture, over the
    /// band of pixels within 8 of the halfway object's outline (both
    /// sides), where a blend of object and background shows.
    fn halo(out: &[u8], truth: &[u8], w: u32, at: (i32, i32)) -> f64 {
        let (mut sum, mut n) = (0u64, 0u64);
        let (x0, y0, x1, y1) = (at.0, at.1, at.0 + 48, at.1 + 48);
        for y in y0 - 8..y1 + 8 {
            for x in x0 - 8..x1 + 8 {
                let inside_core = x >= x0 + 8 && x < x1 - 8 && y >= y0 + 8 && y < y1 - 8;
                if inside_core {
                    continue;
                }
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                for k in 0..3 {
                    sum += u64::from(out[i + k].abs_diff(truth[i + k]));
                }
                n += 3;
            }
        }
        sum as f64 / n as f64
    }

    /// The HUD overlay's rectangle: (x0, y0, x1, y1).
    const OVERLAY: (i32, i32, i32, i32) = (64, 64, 176, 96);

    /// Noise moved by `by`, under a static HUD overlay: sparse glyph cores
    /// (opaque white, about one pixel in 16) with anti-aliased edges (50 %
    /// white over what is behind), as HUD text over a moving scene.
    fn hud_scene(w: u32, h: u32, by: (i32, i32)) -> (Vec<u8>, Vec<bool>) {
        let core = |x: i32, y: i32| {
            let (x0, y0, x1, y1) = OVERLAY;
            (x0..x1).contains(&x) && (y0..y1).contains(&y) && hash(x, y, 77).is_multiple_of(16)
        };
        let mut px = noise(w, h, by, 13, 0..0);
        let mut cores = vec![false; (w * h) as usize];
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let i = (y as u32 * w + x as u32) as usize;
                if core(x, y) {
                    px[i * 4..i * 4 + 3].copy_from_slice(&[255, 255, 255]);
                    cores[i] = true;
                } else if core(x - 1, y) || core(x + 1, y) || core(x, y - 1) || core(x, y + 1) {
                    for k in 0..3 {
                        px[i * 4 + k] = ((u16::from(px[i * 4 + k]) + 255) / 2) as u8;
                    }
                }
            }
        }
        (px, cores)
    }

    struct Rig {
        device: wgpu::Device,
        queue: wgpu::Queue,
        size: (u32, u32),
    }

    impl Rig {
        fn new(size: (u32, u32), features: wgpu::Features) -> Self {
            let (device, queue) = gpu(features);
            Self { device, queue, size }
        }

        fn texture(&self, usage: wgpu::TextureUsages) -> wgpu::Texture {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: self.size.0, height: self.size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        }

        /// A frame as the window keeps them (framegen's FramegenGpu).
        fn frame(&self, px: &[u8]) -> wgpu::Texture {
            let t =
                self.texture(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST);
            self.queue.write_texture(
                t.as_image_copy(),
                px,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.size.0 * 4), rows_per_image: Some(self.size.1) },
                wgpu::Extent3d { width: self.size.0, height: self.size.1, depth_or_array_layers: 1 },
            );
            t
        }

        fn read_buffer(&self, buffer: &wgpu::Buffer) -> Vec<u8> {
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: buffer.size(),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut encoder = self.device.create_command_encoder(&Default::default());
            encoder.copy_buffer_to_buffer(buffer, 0, &readback, 0, buffer.size());
            self.queue.submit([encoder.finish()]);
            readback.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            self.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            readback.get_mapped_range(..).unwrap().to_vec()
        }

        /// `generate` on prev -> next; the output's pixels.
        fn generate(&self, ffx: &mut Ffx, prev: &[u8], next: &[u8]) -> Vec<u8> {
            let (prev, next) = (self.frame(prev), self.frame(next));
            let out = self.texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
            let views = [&prev, &next, &out].map(|t| t.create_view(&Default::default()));
            let mut encoder = self.device.create_command_encoder(&Default::default());
            let frames = Pair { prev: &views[0], next: &views[1], out: &views[2] };
            assert!(ffx.generate(&self.device, &self.queue, &mut encoder, frames, None));
            let (w, h) = self.size;
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(w * h * 4),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                out.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            self.queue.submit([encoder.finish()]);
            readback.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            self.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            readback.get_mapped_range(..).unwrap().to_vec()
        }

        /// Level 0's flow, one vector per block.
        fn flow(&self, ffx: &Ffx) -> Vec<(i32, i32)> {
            let bytes = self.read_buffer(&ffx.state.as_ref().expect("generated").dirs[0].flow_filtered[0]);
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| (i32::from_le_bytes([c[0], c[1], c[2], c[3]]), i32::from_le_bytes([c[4], c[5], c[6], c[7]])))
                .collect()
        }

        /// The 4x4 flow (`Quality::refine`), one vector per 4x4 block.
        fn fine_flow(&self, ffx: &Ffx) -> Vec<(i32, i32)> {
            let fine = ffx.state.as_ref().expect("generated").dirs[0].fine.as_ref().expect("refined");
            let bytes = self.read_buffer(&fine.flow);
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| (i32::from_le_bytes([c[0], c[1], c[2], c[3]]), i32::from_le_bytes([c[4], c[5], c[6], c[7]])))
                .collect()
        }

        fn scene_changed(&self, ffx: &Ffx) -> bool {
            let bytes = self.read_buffer(&ffx.state.as_ref().expect("generated").scd);
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()) & 1 != 0
        }
    }

    const SIZE: (u32, u32) = (256, 192);

    #[test]
    fn the_pyramid_fits_the_flow_and_stops_at_8_pixels() {
        assert_eq!(levels((960, 540)), 7, "1080p's flow, the SDK's 7 levels");
        assert_eq!(levels((128, 96)), 4, "96 >> 3 = 12, >> 4 = 6");
        assert_eq!(levels((8, 8)), 1);
        assert_eq!(flow_sizes((960, 540), 3), [(120, 68), (60, 34), (30, 17)]);
    }

    /// The flow of a picture moved by (8, 4): at half resolution (4, 2),
    /// and the vector points back to where the content was.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_flow_finds_a_known_motion() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::FAST);
        let (w, h) = SIZE;
        rig.generate(&mut ffx, &picture(w, h, (0, 0), 7, 0, 256), &picture(w, h, (8, 4), 7, 0, 256));
        assert!(!rig.scene_changed(&ffx), "one picture, moved: no cut");
        let (fw, fh) = flow_sizes((w / 2, h / 2), 1)[0];
        let flow = rig.flow(&ffx);
        // The blocks at least two from the edge: content there was inside
        // the previous frame.
        let inner: Vec<_> = (2..fh - 2).flat_map(|y| (2..fw - 2).map(move |x| (x, y))).map(|(x, y)| flow[(y * fw + x) as usize]).collect();
        let right = inner.iter().filter(|v| **v == (-4, -2)).count();
        assert!(right * 100 >= inner.len() * 95, "{right} of {} blocks at (-4, -2): {:?}", inner.len(), &inner[..16]);
    }

    /// `ffx-quality`'s flow is at the frame's resolution: the same motion
    /// reads (-8, -4), one vector per 8x8 block of frame pixels.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_flow_is_at_full_resolution() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::BEST);
        let (w, h) = SIZE;
        rig.generate(&mut ffx, &picture(w, h, (0, 0), 7, 0, 256), &picture(w, h, (8, 4), 7, 0, 256));
        let (fw, fh) = flow_sizes((w, h), 1)[0];
        let flow = rig.flow(&ffx);
        assert_eq!(flow.len(), (fw * fh) as usize, "a vector per 8x8 block of the frame");
        let inner: Vec<_> = (2..fh - 2).flat_map(|y| (2..fw - 2).map(move |x| (x, y))).map(|(x, y)| flow[(y * fw + x) as usize]).collect();
        let right = inner.iter().filter(|v| **v == (-8, -4)).count();
        assert!(right * 100 >= inner.len() * 95, "{right} of {} blocks at (-8, -4): {:?}", inner.len(), &inner[..16]);
    }

    /// The wider window: a picture with two pyramid levels, moved by 30.
    /// The top level starts from zero and sees 15: the SDK's window ends
    /// at 7, so level 0, starting from twice a wrong vector with its own
    /// +-8, cannot reach 30; the wide top level finds 15, and level 0
    /// 30 (#73 step 2).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_search_reaches_beyond_8() {
        // 24 rows: level 1 has two block rows, so its 3x3 median is not
        // outvoted by the zeros it reads outside the level.
        let size = (256, 24);
        assert_eq!(levels(size), 2, "two levels");
        let rig = Rig::new(size, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::BEST);
        rig.generate(&mut ffx, &noise(256, 24, (0, 0), 5, 0..0), &noise(256, 24, (30, 0), 5, 0..0));
        let flow = rig.flow(&ffx);
        // Level 0's middle block row, away from the sides.
        let row: Vec<_> = flow[32 + 5..32 + 27].to_vec();
        assert!(row.iter().all(|v| *v == (-30, 0)), "the middle block row: {row:?}");
    }

    /// The 4x4 refinement: the part left of x = 100 moves right by 8, the
    /// rest stands still. The 8x8 block over 96..104 straddles the edge;
    /// its 4x4 quarters take each side's own vector (#73 step 2).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_refines_a_motion_edge_to_4x4() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::BEST);
        let (w, h) = SIZE;
        rig.generate(&mut ffx, &noise(w, h, (0, 0), 9, 0..0), &noise(w, h, (8, 0), 9, 100..w as i32));
        let fine = rig.fine_flow(&ffx);
        let fw = w.div_ceil(4) as usize;
        let rows = 4..(h / 4 - 4) as usize;
        let at = |x: usize| rows.clone().map(|y| fine[y * fw + x]).collect::<Vec<_>>();
        assert!(at(24).iter().all(|v| *v == (-8, 0)), "96..100 moves: {:?}", at(24));
        assert!(at(25).iter().all(|v| *v == (0, 0)), "100..104 stands: {:?}", at(25));
    }

    /// ffx-quality's generated frame of a picture moved by (8, 4) is the
    /// picture moved by (4, 2), as ffx's is.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_generates_the_halfway_picture() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::BEST);
        let (w, h) = SIZE;
        let out = rig.generate(&mut ffx, &noise(w, h, (0, 0), 7, 0..0), &noise(w, h, (8, 4), 7, 0..0));
        let want = noise(w, h, (4, 2), 7, 0..0);
        let border = 32;
        let (mut exact, mut all) = (0, 0);
        for y in border..h - border {
            for x in border..w - border {
                let i = ((y * w + x) * 4) as usize;
                all += 1;
                exact += usize::from(out[i..i + 4] == want[i..i + 4]);
            }
        }
        assert!(exact * 100 >= all * 99, "{exact} of {all} pixels exact");
    }

    /// The halo: an object moves 16 to the right over a still background.
    /// ffx-quality's halfway frame around the object's outline is much
    /// closer to the truth than ffx's (#73 step 3). Measured on the RX
    /// 9070 XT: ffx 41.1 levels, ffx-quality 14.2 (2.9 times smaller).
    /// What is left sits in the two strips only one frame sees (the
    /// background the object uncovers and the one it covers), about half
    /// of each: there the flow of the seen side is unreliable, and which
    /// layer is in front is not known.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_halo_is_smaller_than_ffx() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let (w, h) = SIZE;
        let (prev, next, truth) = (scene(w, h, (80, 72)), scene(w, h, (96, 72)), scene(w, h, (88, 72)));
        let fast = halo(&rig.generate(&mut Ffx::new(&rig.device, Quality::FAST), &prev, &next), &truth, w, (88, 72));
        let best = halo(&rig.generate(&mut Ffx::new(&rig.device, Quality::BEST), &prev, &next), &truth, w, (88, 72));
        eprintln!("halo (mean abs error, 8-bit levels): ffx {fast:.2}, ffx-quality {best:.2}");
        assert!(best * 2.5 <= fast, "ffx-quality {best:.2} against ffx {fast:.2}: not 2.5 times smaller");
    }

    /// The static-HUD mask: HUD text over a textured background moving
    /// (8, 4) a frame, so the flow under the text follows the background.
    /// Without the mask the warp drags the white glyph pixels to where the
    /// true halfway picture has none: ghost text. After three pairs the
    /// overlay is masked: no ghost pixels, and every opaque core pixel
    /// exact (#73 step 4).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_quality_keeps_a_static_hud() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let (w, h) = SIZE;
        let frame = |k: i32| hud_scene(w, h, (8 * k, 4 * k)).0;
        let (truth, cores) = hud_scene(w, h, (28, 14));
        // Ghost: glyph white (every channel at least 240) where the truth
        // has no glyph pixel (noise has all three that high about once in
        // 4000 pixels).
        let white = |px: &[u8], i: usize| px[i * 4..i * 4 + 3].iter().all(|&c| c >= 240);
        let ghosts_and_cores = |quality: Quality| {
            let mut ffx = Ffx::new(&rig.device, quality);
            let mut out = Vec::new();
            for k in 0..4 {
                out = rig.generate(&mut ffx, &frame(k), &frame(k + 1));
            }
            let (x0, y0, x1, y1) = OVERLAY;
            let (mut ghosts, mut exact, mut all) = (0, 0, 0);
            for y in y0 - 8..y1 + 8 {
                for x in x0 - 8..x1 + 8 {
                    let i = (y as u32 * w + x as u32) as usize;
                    if cores[i] {
                        all += 1;
                        exact += usize::from(out[i * 4..i * 4 + 3] == truth[i * 4..i * 4 + 3]);
                    } else if white(&out, i) && !white(&truth, i) {
                        ghosts += 1;
                    }
                }
            }
            (ghosts, exact, all)
        };
        let (ghosts, exact, all) = ghosts_and_cores(Quality::BEST);
        let (unmasked, _, _) = ghosts_and_cores(Quality { hud_mask: false, ..Quality::BEST });
        eprintln!("HUD ghost pixels: masked {ghosts}, unmasked {unmasked}; cores exact {exact} of {all}");
        assert_eq!(exact, all, "every opaque core pixel exact");
        assert!(unmasked >= 20, "the scene drags the text without the mask: {unmasked} ghost pixels");
        assert!(ghosts * 10 <= unmasked, "masked {ghosts} ghost pixels against {unmasked}");
    }

    /// A dark picture, then an unrelated bright one: a scene change, and the
    /// generated frame is the new frame itself (the real frame).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_a_cut_shows_the_new_frame() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::FAST);
        let (w, h) = SIZE;
        let next = picture(w, h, (0, 0), 99, 160, 96);
        let out = rig.generate(&mut ffx, &picture(w, h, (0, 0), 3, 0, 90), &next);
        assert!(rig.scene_changed(&ffx), "dark to bright, unrelated: a cut");
        assert!(out == next, "the new frame, texel for texel");
        // And the same pair moved, not cut, right after: the detector
        // judges each pair on its own.
        rig.generate(&mut ffx, &picture(w, h, (0, 0), 7, 0, 256), &picture(w, h, (8, 4), 7, 0, 256));
        assert!(!rig.scene_changed(&ffx));
    }

    /// The generated frame of a picture moved by (8, 4) is the picture
    /// moved by (4, 2), texel for texel away from the edges; a plain blend
    /// of the two frames is not.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_generates_the_halfway_picture() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device, Quality::FAST);
        let (w, h) = SIZE;
        let (prev, next) = (picture(w, h, (0, 0), 7, 0, 256), picture(w, h, (8, 4), 7, 0, 256));
        let out = rig.generate(&mut ffx, &prev, &next);
        let halfway = picture(w, h, (4, 2), 7, 0, 256);
        let border = 32;
        let inner = |px: &[u8]| -> Vec<[u8; 4]> {
            (border..h - border)
                .flat_map(|y| (border..w - border).map(move |x| ((y * w + x) * 4) as usize))
                .map(|i| [px[i], px[i + 1], px[i + 2], px[i + 3]])
                .collect()
        };
        let (got, want) = (inner(&out), inner(&halfway));
        let exact = got.iter().zip(&want).filter(|(a, b)| a == b).count();
        assert!(exact * 100 >= got.len() * 99, "{exact} of {} pixels exact", got.len());
        let blend: Vec<_> = inner(&prev)
            .iter()
            .zip(inner(&next))
            .map(|(a, b)| [0, 1, 2, 3].map(|k| (u16::from(a[k]) + u16::from(b[k])).div_ceil(2) as u8))
            .collect();
        let blend_exact = blend.iter().zip(&want).filter(|(a, b)| a == b).count();
        assert!(blend_exact * 2 < got.len(), "the test tells motion from a blend: {blend_exact} of {}", got.len());
    }

    /// The generator's GPU time at 1080p, as `--stats` measures it
    /// (`cargo test --release -- --ignored gpu_ffx_time --nocapture`).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_time_at_1080p() {
        let rig = Rig::new((1920, 1080), wgpu::Features::TIMESTAMP_QUERY);
        for (name, quality) in [("ffx", Quality::FAST), ("ffx-quality", Quality::BEST)] {
            time(&rig, name, quality);
        }
    }

    fn time(rig: &Rig, name: &str, quality: Quality) {
        let Some(mut timer) = crate::gputime::GpuTimer::new(&rig.device, &rig.queue) else {
            eprintln!("this GPU cannot time passes");
            return;
        };
        let mut ffx = Ffx::new(&rig.device, quality);
        let (w, h) = rig.size;
        let (prev, next) = (rig.frame(&picture(w, h, (0, 0), 7, 0, 256)), rig.frame(&picture(w, h, (8, 4), 7, 0, 256)));
        let out = rig.texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let views = [&prev, &next, &out].map(|t| t.create_view(&Default::default()));
        // Back to back (full clocks), then paced at 60 Hz as the window runs
        // it (the GPU's power level follows the load, #75).
        for paced in [false, true] {
            let mut times = Vec::new();
            for i in 0..if paced { 180 } else { 60 } {
                let started = std::time::Instant::now();
                timer.arm();
                let mut encoder = rig.device.create_command_encoder(&Default::default());
                let frames = Pair { prev: &views[0], next: &views[1], out: &views[2] };
                assert!(ffx.generate(&rig.device, &rig.queue, &mut encoder, frames, timer.generate()));
                timer.resolve(&mut encoder);
                rig.queue.submit([encoder.finish()]);
                timer.submitted();
                rig.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                if let Some(t) = timer.take().unwrap()
                && let Some(g) = t.generate
                // Paced: the last 2 s, once the clocks have settled.
                && (!paced || i >= 60)
                {
                    times.push(g);
                }
                if paced {
                    std::thread::sleep(std::time::Duration::from_micros(16_667).saturating_sub(started.elapsed()));
                }
            }
            times.sort();
            assert!(!times.is_empty(), "timed at least once");
            eprintln!(
                "{name} at 1920x1080, {}: median {:?}, p90 {:?}, p99 {:?}, {} frames",
                if paced { "paced at 60 Hz" } else { "back to back" },
                times[times.len() / 2],
                times[times.len() * 9 / 10],
                times[times.len() * 99 / 100],
                times.len()
            );
        }
    }
}
