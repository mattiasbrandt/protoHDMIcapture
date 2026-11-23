//! The `ffx` generator (#68), its flow so far: AMD FidelityFX Optical Flow (SDK v1.1.4,
//! MIT; the WGSL ports in `ffx/` keep AMD's notice) finds where each 8x8
//! block of the new frame was in the previous one, and an interpolation
//! pass after afmf-linux's (MIT) warps both frames halfway along that flow.
//!
//! Per generated frame, on the GPU, in the SDK's order:
//! - **Luma** of both frames at half resolution (afmf-linux's choice: a
//!   1080p frame's flow at 960x540, ~0.6 ms a frame at 3440x1440 on RADV
//!   by its own measurement), and a pyramid of up to 7 levels, each the
//!   2x2 mean of the one above (the SDK's single-pass downsampler computes
//!   the same means).
//! - **Scene change**: nine strata histograms of each frame and their
//!   divergence; over the SDK's 0.45 the pair is a cut, and every later
//!   pass sees it.
//! - **Flow**, coarsest level first: block search around the coarser
//!   level's vector (+-8 pixels), a 3x3 vector median, and the scale-up
//!   that seeds the next finer level.
//! - **Interpolation** into the output, a render pass at the frames' size.
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

use crate::gputime::PassTimes;

/// The SDK's pyramid depth (`advancedAlgorithmIterations`).
const MAX_LEVELS: u32 = 7;
/// Pixels a side of a flow block, at every level.
const BLOCK: u32 = 8;
/// Frame pixels per flow-luma pixel: half resolution.
const SCALE: u32 = 2;
/// The coarsest level is at least this many luma pixels on its short side
/// (the search reads 4-pixel words and 8x8 blocks).
const MIN_LEVEL_SIDE: u32 = 8;
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
fn params(luma: (u32, u32), of: (u32, u32), next_of: (u32, u32), src: (u32, u32), level: u32, levels: u32) -> Vec<u8> {
    let i = |v: u32| (v as i32).to_le_bytes();
    [i(luma.0), i(luma.1), i(of.0), i(of.1), i(next_of.0), i(next_of.1), i(src.0), i(src.1)]
        .into_iter()
        .chain([level, levels, SCALE, 0].map(u32::to_le_bytes))
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

struct Pipelines {
    luma: wgpu::ComputePipeline,
    down: wgpu::ComputePipeline,
    histogram: wgpu::ComputePipeline,
    divergence: wgpu::ComputePipeline,
    search: wgpu::ComputePipeline,
    filter: wgpu::ComputePipeline,
    scale: wgpu::ComputePipeline,
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
    /// Per level: the search's vectors (seeded by the coarser level).
    flow_searched: Vec<wgpu::Buffer>,
    /// Per level: the filtered vectors; level 0's is the flow.
    flow_filtered: Vec<wgpu::Buffer>,
    histogram: wgpu::Buffer,
    previous_histogram: wgpu::Buffer,
    filtered_histogram: wgpu::Buffer,
    scd: wgpu::Buffer,
    /// Level 0's parameters (the luma pass's).
    luma_params: wgpu::Buffer,
    /// Per frame, per level from 1: the 2x2 reduction into it.
    down: [Vec<wgpu::BindGroup>; 2],
    /// Per frame: its histograms.
    histograms: [wgpu::BindGroup; 2],
    divergence: wgpu::BindGroup,
    search: Vec<wgpu::BindGroup>,
    filter: Vec<wgpu::BindGroup>,
    /// Per level from 1: the scale-up into the level below.
    scale: Vec<wgpu::BindGroup>,
}

impl State {
    fn new(device: &wgpu::Device, p: &Pipelines, size: (u32, u32)) -> Self {
        let src = (size.0.div_ceil(SCALE), size.1.div_ceil(SCALE));
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
        let flows = |which: &str| -> Vec<wgpu::Buffer> {
            flow_sizes
                .iter()
                .enumerate()
                .map(|(l, (w, h))| storage(device, &format!("ffx flow {which} {l}"), u64::from(w * h) * 8))
                .collect()
        };
        let (flow_searched, flow_filtered) = (flows("searched"), flows("filtered"));
        let histogram = storage(device, "ffx histogram", HISTOGRAM_BINS * 4);
        let previous_histogram = storage(device, "ffx previous histogram", HISTOGRAM_BINS * 4);
        let filtered_histogram = storage(device, "ffx filtered histogram", HISTOGRAM_BINS * 4);
        let scd = storage(device, "ffx scene change", SCD_SLOTS * 4);
        let level_params: Vec<_> = (0..n as usize)
            .map(|l| {
                let next_of = if l > 0 { flow_sizes[l - 1] } else { (0, 0) };
                // The pyramid pass reads the finer level: `src` is it.
                let finer = if l > 0 { luma_sizes[l - 1] } else { src };
                let bytes = params(luma_sizes[l], flow_sizes[l], next_of, src, l as u32, n);
                let down = params(luma_sizes[l], flow_sizes[l], next_of, finer, l as u32, n);
                (uniform(device, &format!("ffx level {l}"), &bytes), uniform(device, &format!("ffx down {l}"), &down))
            })
            .collect();
        let luma_params = uniform(device, "ffx luma", &params(src, flow_sizes[0], (0, 0), src, 0, n));
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
        let search = (0..n as usize)
            .map(|l| {
                bind(
                    device,
                    &p.search,
                    "ffx search",
                    &[b(&level_params[l].0), b(&luma[0][l]), b(&luma[1][l]), b(&flow_searched[l]), b(&scd)],
                )
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
                    &[b(&level_params[l].0), b(&luma[0][l]), b(&luma[1][l]), b(&flow_filtered[l]), b(&flow_searched[l - 1]), b(&scd)],
                )
            })
            .collect();
        Self {
            size,
            src,
            luma_sizes,
            flow_sizes,
            luma,
            flow_searched,
            flow_filtered,
            histogram,
            previous_histogram,
            filtered_histogram,
            scd,
            luma_params,
            down,
            histograms,
            divergence,
            search,
            filter,
            scale,
        }
    }
}

/// The generator: its pipelines, and its buffers once it knows the size.
pub struct Ffx {
    pipelines: Pipelines,
    state: Option<State>,
}

impl Ffx {
    pub fn new(device: &wgpu::Device) -> Self {
        let pipelines = Pipelines {
            luma: compute(device, "ffx luma", include_str!("ffx/luma.wgsl")),
            down: compute(device, "ffx down", include_str!("ffx/down.wgsl")),
            histogram: compute(device, "ffx histogram", include_str!("ffx/histogram.wgsl")),
            divergence: compute(device, "ffx divergence", include_str!("ffx/divergence.wgsl")),
            search: compute(device, "ffx search", include_str!("ffx/search.wgsl")),
            filter: compute(device, "ffx filter", include_str!("ffx/filter.wgsl")),
            scale: compute(device, "ffx scale", include_str!("ffx/scale.wgsl")),
        };
        Self { pipelines, state: None }
    }

    /// The flow from `next` back to `prev` and the scene-change test, into
    /// the state's buffers (level 0's filtered flow, the scene-change
    /// slots).
    pub fn flow(
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
            self.state = Some(State::new(device, &self.pipelines, size));
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
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("ffx flow"), timestamp_writes: None });
        pass.set_pipeline(&p.histogram);
        pass.set_bind_group(0, &s.histograms[0], &[]);
        pass.dispatch_workgroups(histogram_groups.0.max(1), 16, 9);
        pass.set_pipeline(&p.divergence);
        pass.set_bind_group(0, &s.divergence, &[]);
        pass.dispatch_workgroups(9, 3, 1);
        for l in (0..n).rev() {
            pass.set_pipeline(&p.search);
            pass.set_bind_group(0, &s.search[l], &[]);
            let (gx, gy) = groups(s.luma_sizes[l], 16, 16);
            pass.dispatch_workgroups(gx, gy, 1);
            pass.set_pipeline(&p.filter);
            pass.set_bind_group(0, &s.filter[l], &[]);
            let (gx, gy) = groups(s.flow_sizes[l], 16, 4);
            pass.dispatch_workgroups(gx, gy, 1);
            if l > 0 {
                pass.set_pipeline(&p.scale);
                pass.set_bind_group(0, &s.scale[l - 1], &[]);
                let (gx, gy) = groups(s.flow_sizes[l - 1], 4, 4);
                pass.dispatch_workgroups(gx, gy, 1);
            }
        }
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
            ffx.flow(&self.device, &mut encoder, &views[0], &views[1], None);
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
            let bytes = self.read_buffer(&ffx.state.as_ref().expect("generated").flow_filtered[0]);
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
        let mut ffx = Ffx::new(&rig.device);
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

    /// A dark picture, then an unrelated bright one: a scene change.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_ffx_a_cut_shows_the_new_frame() {
        let rig = Rig::new(SIZE, wgpu::Features::empty());
        let mut ffx = Ffx::new(&rig.device);
        let (w, h) = SIZE;
        let next = picture(w, h, (0, 0), 99, 160, 96);
        rig.generate(&mut ffx, &picture(w, h, (0, 0), 3, 0, 90), &next);
        assert!(rig.scene_changed(&ffx), "dark to bright, unrelated: a cut");
        // And the same pair moved, not cut, right after: the detector
        // judges each pair on its own.
        rig.generate(&mut ffx, &picture(w, h, (0, 0), 7, 0, 256), &picture(w, h, (8, 4), 7, 0, 256));
        assert!(!rig.scene_changed(&ffx));
    }
}
