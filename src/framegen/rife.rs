//! RIFE v4.6 frame interpolation (#74): the machine-learned interpolator the
//! Windows tools run (SmoovPlay: RIFE on TensorRT), here as wgpu compute so
//! one code path serves every GPU. The network is IFNet v4.6 from
//! Practical-RIFE (hzwer, MIT) in rife-ncnn-vulkan's conversion (nihui,
//! MIT): `rife/flownet-v4.6.bin` is that repository's
//! `models/rife-v4.6/flownet.bin`, unchanged (10.6 MB, fp16 weights), and
//! `rife/rife.wgsl` its layers with ncnn's semantics, so the output matches
//! ncnn's own (the golden test). Notices: `rife/NOTICE.md`.
//!
//! IFNet refines a flow both ways and a blend mask in four blocks, at 1/8,
//! 1/4, 1/2 and 1/1 of the frame; each warps both frames by the flow so far
//! and adds its correction. RIFE's `scale` divides those (0.5: 1/16 to
//! 1/2), which is how it runs large frames faster; the last warp and blend
//! stay at full resolution either way. The output is
//! `warp(prev, flow) * mask + warp(next, flow') * (1 - mask)`.
//!
//! A scene cut: RIFE has no detector, so the two frames' luma histograms
//! are compared on the GPU (#68's idea) and on a cut the output is the new
//! frame: the same picture at the same time as the seam's real-frame path.

use std::borrow::Cow;

use wgpu::util::{BufferInitDescriptor, DeviceExt};

use super::{Generator, Pair};
use crate::gputime::PassTimes;

/// The weights, as rife-ncnn-vulkan ships them.
const WEIGHTS: &[u8] = include_bytes!("rife/flownet-v4.6.bin");
const SHADER: &str = include_str!("rife/rife.wgsl");

/// The blocks: (channels, the scale they work at with RIFE's scale 1).
const BLOCKS: [(u32, u32); 4] = [(192, 8), (128, 4), (96, 2), (64, 1)];
/// Residual convolutions in a block.
const RESBLOCKS: usize = 8;
/// A block's output: 4 flow channels, the mask, and one unused, PixelShuffled
/// from 24.
const DECONV_OUT: u32 = 24;
/// The input channels the deconvolution reads at a time (rife.wgsl's `DCI`).
const DECONV_CHANNELS: u32 = 8;
/// rife.wgsl's conv flags (conv1 is stride 1, conv2 stride 2).
const LEAKY: u32 = 1;
const RESIDUAL: u32 = 2;

const PARTIAL: u32 = 8;

#[cfg(test)]
#[test]
fn small_convolutions_split_their_input_channels() {
    // RIFE scale 1 at 1080p, block 0's residual convs: 34x60, 192 channels.
    assert_eq!(conv_groups(60, 34, 192), (4, 3, 12));
    let (slices, per) = split((4, 3, 12), 192, 8);
    assert!(slices * 144 >= ENOUGH && per % 8 == 0 && slices * per >= 192 && (slices - 1) * per < 192, "{slices} x {per}");
    // Block 3's: already enough workgroups, no split.
    assert_eq!(split(conv_groups(480, 272, 64), 64, 8), (1, 64));
    // Never more slices than channel steps.
    assert_eq!(split((1, 1, 1), 7, 2), (4, 2));
}

/// A convolution's workgroups: a 16x16 output tile of 16 channels each.
fn conv_groups(w: u32, h: u32, c: u32) -> (u32, u32, u32) {
    (w.div_ceil(16), h.div_ceil(16), c.div_ceil(16))
}

/// Workgroups a convolution should have to fill the GPU (64 compute units
/// on the RX 9070 XT, several workgroups each in flight).
const ENOUGH: u32 = 768;

/// Split-K for a convolution too small to fill the GPU: (slices, input
/// channels a slice, a multiple of `step`, the channels a workgroup reads
/// at a time). One slice: no split.
fn split((x, y, z): (u32, u32, u32), cin: u32, step: u32) -> (u32, u32) {
    let groups = x * y * z;
    let want = ENOUGH.div_ceil(groups).clamp(1, cin.div_ceil(step));
    let per = cin.div_ceil(want).div_ceil(step) * step;
    (cin.div_ceil(per), per)
}

/// The size `plan` pads a `w` x `h` frame to at `scale`: the smallest
/// block works at 1/(8/scale) and halves twice more.
fn padded((w, h): (u32, u32), scale: f32) -> (u32, u32) {
    let pad = (32.0 / scale) as u32;
    (w.div_ceil(pad) * pad, h.div_ceil(pad) * pad)
}

/// The largest buffer `plan` makes for a `size` frame at `scale`, in bytes
/// (#74): each is bound whole, so the device's storage-binding limit must
/// allow it. A 3840x2160 capture needs 267 MB, over wgpu's default 128 MiB.
/// `plan` checks its own buffers against this, so the two cannot drift.
pub fn largest_binding(size: (u32, u32), scale: f32) -> u64 {
    let (pw, ph) = padded(size, scale);
    let plane = u64::from(pw) * u64::from(ph);
    // frames, flow, warped and result: 7, 5, 8 and 3 planes.
    let mut floats = 8 * plane;
    for (i, &(c, base)) in BLOCKS.iter().enumerate() {
        let s = (base as f32 / scale) as u32;
        let (bw, bh) = (pw / s, ph / s);
        let cin = if i == 0 { 7 } else { 12 };
        let (w1, h1) = (bw / 2, bh / 2);
        let (w2, h2) = (bw / 4, bh / 4);
        let area = |c: u32, w: u32, h: u32| u64::from(c) * u64::from(w) * u64::from(h);
        // A convolution split over its input channels sums into slices.
        let partial = |d_c: u32, d_w: u32, d_h: u32, s_c: u32, step: u32| {
            let (slices, _) = split(conv_groups(d_w, d_h, d_c), s_c, step);
            if slices == 1 { 0 } else { u64::from(slices) * area(d_c, d_w, d_h) }
        };
        let (deconv_slices, _) = split((w2.div_ceil(8), h2.div_ceil(8), 1), c, DECONV_CHANNELS);
        let deconv_partial = if deconv_slices == 1 { 0 } else { u64::from(deconv_slices) * area(DECONV_OUT, w1, h1) };
        floats = [
            floats,
            area(cin, bw, bh),
            area(c / 2, w1, h1),
            area(c, w2, h2),
            area(DECONV_OUT, w1, h1),
            partial(c / 2, w1, h1, cin, 2),
            partial(c, w2, h2, c / 2, 2),
            partial(c, w2, h2, c, 8),
            deconv_partial,
        ]
        .into_iter()
        .max()
        .expect("not empty");
    }
    floats * 4
}

/// The timestep the generated frame sits at: halfway.
const TIMESTEP: f32 = 0.5;

/// The scale `--framegen rife` runs at. Paced at 60 Hz, as the window runs
/// it, at 1080p on an RX 9070 XT with fp16 (#74's pacing fix,
/// `gpu_rife_paced_time_at_1080p`): 0.25 takes 5.87 ms (p99 5.92) with
/// GameMode and 7.33 ms (p99 7.45) without; 0.5 took 10.59 / 13.49 ms,
/// too little of a refresh left for the window's other work (106 of 893
/// in-between frames missed on the driving clip). 0.25 costs 0.94 dB on
/// #81's skip-frame score (25.01 against 25.95 dB) and 25 % more tear
/// blocks. 1.0, the model's full resolution, took 23.9 ms back to back.
pub const SCALE: f32 = 0.25;

/// The scale to run at: `SCALE`, or `PROTOHDMICAPTURE_RIFE_SCALE` (a dev
/// knob for comparing settings with `--framegen-score`, #81: 1, 0.5 or
/// 0.25). A value it cannot use is said on stderr and `SCALE` runs.
pub fn scale() -> f32 {
    let v = std::env::var("PROTOHDMICAPTURE_RIFE_SCALE").ok();
    scale_from(v.as_deref()).unwrap_or_else(|v| {
        eprintln!("warning: PROTOHDMICAPTURE_RIFE_SCALE={v} is not 1, 0.5 or 0.25; rife runs at {SCALE}");
        SCALE
    })
}

/// The knob's value: unset is `SCALE`; one it cannot use is an error.
fn scale_from(v: Option<&str>) -> Result<f32, String> {
    match v {
        None => Ok(SCALE),
        Some(v) => v.parse::<f32>().ok().filter(|x| [1.0, 0.5, 0.25].contains(x)).ok_or_else(|| v.to_string()),
    }
}

#[cfg(test)]
#[test]
fn the_largest_buffer_follows_the_padded_picture() {
    // At SCALE the frame pads to 128: the warped planes (8 floats a
    // pixel) are the largest buffer (#74's 4K finding: 267 MB).
    assert_eq!(largest_binding((1920, 1080), SCALE), 8 * 1920 * 1152 * 4);
    assert_eq!(largest_binding((2560, 1440), SCALE), 8 * 2560 * 1536 * 4);
    assert_eq!(largest_binding((3840, 2160), SCALE), 267_386_880);
    let default = wgpu::Limits::default().max_storage_buffer_binding_size;
    assert!(largest_binding((2560, 1440), SCALE) <= default && largest_binding((3840, 2160), SCALE) > default);
}

#[cfg(test)]
#[test]
fn the_scale_knob_takes_the_scales_the_network_runs_at() {
    assert_eq!(scale_from(None), Ok(SCALE));
    assert_eq!(scale_from(Some("0.5")), Ok(0.5));
    assert_eq!(scale_from(Some("1")), Ok(1.0));
    assert_eq!(scale_from(Some("0.3")), Err("0.3".into()));
    assert_eq!(scale_from(Some("fast")), Err("fast".into()));
}

/// fp16 (IEEE binary16) bits to f32.
fn f16(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = (bits >> 10) & 0x1f;
    let man = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * man * 2f32.powi(-24),
        0x1f if man == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        e => sign * (1.0 + man / 1024.0) * 2f32.powi(i32::from(e) - 15),
    }
}

/// One convolution's or deconvolution's weights and bias, as f32.
pub struct Layer {
    pub out: u32,
    pub inp: u32,
    pub weight: Vec<f32>,
    pub bias: Vec<f32>,
}

/// The model's layers in file order: per block, two stride-2 convolutions,
/// eight residual ones, and the deconvolution.
pub fn layers(bytes: &[u8]) -> Result<Vec<Layer>, String> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let s = bytes.get(at..at + n).ok_or_else(|| format!("RIFE weights end at byte {at}, {n} more expected"))?;
        at += n;
        Ok(s)
    };
    let mut layers = Vec::new();
    for (i, &(c, _)) in BLOCKS.iter().enumerate() {
        let first_in = if i == 0 { 7 } else { 12 };
        let mut shapes = vec![(c / 2, first_in, 3), (c, c / 2, 3)];
        shapes.extend(std::iter::repeat_n((c, c, 3), RESBLOCKS));
        shapes.push((DECONV_OUT, c, 4));
        for (out, inp, kernel) in shapes {
            let n = (out * inp * kernel * kernel) as usize;
            // ncnn ModelBin: a flag word, then the weights (0x01306B47:
            // fp16), then the bias as plain f32.
            let flag = u32::from_le_bytes(take(4)?.try_into().expect("4 bytes"));
            if flag != 0x0130_6B47 {
                return Err(format!("RIFE weights: layer {} is stored as {flag:#x}, not fp16", layers.len()));
            }
            let weight = take(n * 2)?.as_chunks::<2>().0.iter().map(|b| f16(u16::from_le_bytes(*b))).collect();
            let bias = take(out as usize * 4)?.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            layers.push(Layer { out, inp, weight, bias });
        }
    }
    if at != bytes.len() {
        return Err(format!("RIFE weights: {} bytes left over after {} layers", bytes.len() - at, layers.len()));
    }
    Ok(layers)
}

/// The uniform every kernel reads (rife.wgsl's `P`).
#[derive(Default, Clone, Copy)]
struct Params {
    s_off: u32,
    s_c: u32,
    s_w: u32,
    s_h: u32,
    d_off: u32,
    d_c: u32,
    d_w: u32,
    d_h: u32,
    f_off: u32,
    flags: u32,
    mul: f32,
    scale: f32,
}

impl Params {
    fn bytes(&self) -> Vec<u8> {
        [self.s_off, self.s_c, self.s_w, self.s_h, self.d_off, self.d_c, self.d_w, self.d_h, self.f_off, self.flags]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .chain(self.mul.to_le_bytes())
            .chain(self.scale.to_le_bytes())
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kernel {
    Load,
    Copy,
    Interp,
    Warp,
    Conv1,
    Conv2,
    Reduce,
    Deconv,
    BlockOut,
    Merge,
    Histogram,
    Cut,
}

const KERNELS: [(Kernel, &str); 12] = [
    (Kernel::Load, "load"),
    (Kernel::Copy, "copy"),
    (Kernel::Interp, "interp"),
    (Kernel::Warp, "warp"),
    (Kernel::Conv1, "conv1"),
    (Kernel::Conv2, "conv2"),
    (Kernel::Reduce, "reduce"),
    (Kernel::Deconv, "deconv"),
    (Kernel::BlockOut, "block_out"),
    (Kernel::Merge, "merge"),
    (Kernel::Histogram, "histogram"),
    (Kernel::Cut, "cut"),
];

/// One dispatch of the network for a frame size.
struct Op {
    kernel: Kernel,
    bind: wgpu::BindGroup,
    groups: (u32, u32, u32),
}

/// The buffers and dispatches for one frame size.
struct Plan {
    size: (u32, u32),
    padded: (u32, u32),
    load_uniform: wgpu::Buffer,
    frames: wgpu::Buffer,
    hist: wgpu::Buffer,
    /// The merged frame, 3 channels at the padded size: what the output
    /// pass writes and the golden test reads.
    #[cfg_attr(not(test), allow(dead_code))]
    result: wgpu::Buffer,
    ops: Vec<Op>,
    out_bind: wgpu::BindGroup,
}

/// `--framegen rife`.
pub struct Rife {
    scale: f32,
    /// The heavy kernels in f16 (rife.wgsl's alias `A`).
    #[cfg_attr(not(test), allow(dead_code))]
    fp16: bool,
    pipelines: Vec<(Kernel, wgpu::ComputePipeline)>,
    weights: Vec<(wgpu::Buffer, wgpu::Buffer, u32, u32)>,
    out_pipeline: wgpu::RenderPipeline,
    out_layout: wgpu::BindGroupLayout,
    plan: Option<Plan>,
}

const OUT: &str = r#"
struct O { w: u32, pw: u32, plane: u32, pad: u32 };
@group(0) @binding(0) var<uniform> o: O;
@group(0) @binding(1) var<storage, read> result: array<f32>;
@group(0) @binding(2) var<storage, read> frames: array<f32>;
@group(0) @binding(3) var<storage, read> cut_flag: array<u32, 1>;

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let x = u32(in.pos.x);
    let y = u32(in.pos.y);
    let i = y * o.pw + x;
    // A cut: the new frame (frames' channels 3..6), as the seam shows it.
    let b = select(0u, 1u, cut_flag[0] != 0u);
    let src = select(0u, 3u * o.plane, b == 1u);
    var c: vec3f;
    if b == 1u {
        c = vec3f(frames[src + i], frames[src + o.plane + i], frames[src + 2u * o.plane + i]);
    } else {
        c = vec3f(result[i], result[o.plane + i], result[2u * o.plane + i]);
    }
    return vec4f(clamp(c, vec3f(0.0), vec3f(1.0)), 1.0);
}
"#;

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
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

impl Rife {
    /// fp16 where the device has it (`SHADER_F16`, which the window asks
    /// for when a generator may run), as rife-ncnn-vulkan does on such GPUs.
    pub fn new(device: &wgpu::Device, scale: f32) -> Result<Self, String> {
        Self::with_precision(device, scale, device.features().contains(wgpu::Features::SHADER_F16))
    }

    fn with_precision(device: &wgpu::Device, scale: f32, fp16: bool) -> Result<Self, String> {
        let bytes = WEIGHTS;
        if ![1.0, 0.5, 0.25].contains(&scale) {
            return Err(format!("RIFE scale {scale}: 1, 0.5 or 0.25"));
        }
        let source = if fp16 {
            Cow::Owned(format!("enable f16;\n{}", SHADER.replacen("alias A = f32;", "alias A = f16;", 1)))
        } else {
            Cow::Borrowed(SHADER)
        };
        let desc = wgpu::ShaderModuleDescriptor { label: Some("rife"), source: wgpu::ShaderSource::Wgsl(source) };
        let module = device.create_shader_module(desc);
        let pipelines = KERNELS
            .iter()
            .map(|&(k, entry)| {
                let p = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: None,
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                });
                (k, p)
            })
            .collect();
        let buffer = |label: &str, data: &[f32]| {
            let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
            device.create_buffer_init(&BufferInitDescriptor { label: Some(label), contents: &bytes, usage: wgpu::BufferUsages::STORAGE })
        };
        let weights = layers(bytes)?
            .iter()
            .enumerate()
            .map(|(i, l)| (buffer(&format!("rife weight {i}"), &l.weight), buffer(&format!("rife bias {i}"), &l.bias), l.inp, l.out))
            .collect();
        let out_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rife out"),
            entries: &[crate::app::uniform_entry(0), storage_entry(1), storage_entry(2), storage_entry(3)],
        });
        let out_pipeline = crate::app::pipeline(device, "rife out", OUT, &out_layout, wgpu::TextureFormat::Rgba8Unorm);
        Ok(Self { scale, fp16, pipelines, weights, out_pipeline, out_layout, plan: None })
    }

    fn pipeline(&self, k: Kernel) -> &wgpu::ComputePipeline {
        &self.pipelines.iter().find(|(p, _)| *p == k).expect("every kernel has a pipeline").1
    }

    /// The buffers and dispatches for a `w` x `h` frame.
    fn plan(&self, device: &wgpu::Device, (w, h): (u32, u32)) -> Plan {
        let (pw, ph) = padded((w, h), self.scale);
        let plane = u64::from(pw) * u64::from(ph);
        // The largest buffer made, against `largest_binding` (#74).
        let largest = std::cell::Cell::new(0u64);
        let storage = |label: &str, floats: u64| {
            largest.set(largest.get().max(floats * 4));
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: floats * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let uniform = |p: Params| {
            device.create_buffer_init(&BufferInitDescriptor {
                label: Some("rife params"),
                contents: &p.bytes(),
                usage: wgpu::BufferUsages::UNIFORM,
            })
        };
        let frames = storage("rife frames", 7 * plane);
        let flow = storage("rife flow", 5 * plane);
        let warped = storage("rife warped", 8 * plane);
        let result = storage("rife result", 3 * plane);
        let hist = storage("rife histogram", 128);
        let cut = storage("rife cut", 4);

        let mut ops = Vec::new();
        let mut op = |kernel: Kernel, params: Params, bufs: &[(u32, &wgpu::Buffer)], groups: (u32, u32, u32)| {
            // Every kernel reads the parameters but `cut`.
            let u = (kernel != Kernel::Cut).then(|| uniform(params));
            let mut entries: Vec<_> = u.iter().map(|u| wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() }).collect();
            entries.extend(bufs.iter().map(|&(binding, b)| wgpu::BindGroupEntry { binding, resource: b.as_entire_binding() }));
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rife op"),
                layout: &self.pipeline(kernel).get_bind_group_layout(0),
                entries: &entries,
            });
            ops.push(Op { kernel, bind, groups });
        };
        let g8 = |w: u32, h: u32| (w.div_ceil(8), h.div_ceil(8), 1);
        // A convolution: straight, or split over input channels into a
        // partial buffer and reduced.
        macro_rules! conv {
            ($kernel:expr, $step:expr, $params:expr, $input:expr, $out:expr, $layer:expr) => {{
                let params: Params = $params;
                let (wt, b, _, _) = &self.weights[$layer];
                let groups = conv_groups(params.d_w, params.d_h, params.d_c);
                let (slices, per) = split(groups, params.s_c, $step);
                if slices == 1 {
                    op($kernel, params, &[(1, $input), (2, $out), (4, wt), (5, b), (6, $input)], groups);
                } else {
                    let part = storage("rife partial sums", u64::from(slices * params.d_c * params.d_w * params.d_h));
                    let split = Params { flags: params.flags | PARTIAL, f_off: per, ..params };
                    op($kernel, split, &[(1, $input), (2, &part), (4, wt), (5, b), (6, $input)], (groups.0, groups.1, groups.2 * slices));
                    let reduce = Params { s_c: slices, ..params };
                    op(
                        Kernel::Reduce,
                        reduce,
                        &[(1, &part), (2, $out), (5, b), (6, $input)],
                        (params.d_w.div_ceil(8), params.d_h.div_ceil(8), params.d_c),
                    );
                }
            }};
        }

        let mut layer = 0usize;
        for (i, &(c, base)) in BLOCKS.iter().enumerate() {
            let s = (base as f32 / self.scale) as u32;
            let (bw, bh) = (pw / s, ph / s);
            let cin = if i == 0 { 7 } else { 12 };
            let input = storage("rife block input", u64::from(cin) * u64::from(bw) * u64::from(bh));
            if i == 0 {
                // [img0, img1, timestep] resized to the block.
                op(
                    Kernel::Interp,
                    Params { s_c: 7, s_w: pw, s_h: ph, d_c: 7, d_w: bw, d_h: bh, mul: 1.0, ..Default::default() },
                    &[(1, &frames), (2, &input)],
                    g8(bw, bh),
                );
            } else {
                // [img0 and img1 warped by the flow so far, timestep, mask],
                // resized to the block, then the flow resized and divided by
                // the scale. At scale 1 straight into the block's input.
                let into = if s == 1 { &input } else { &warped };
                let full = Params { d_c: 8, d_w: pw, d_h: ph, mul: 1.0, ..Default::default() };
                op(Kernel::Warp, Params { s_off: 0, f_off: 0, d_off: 0, ..full }, &[(1, &frames), (2, into), (3, &flow)], g8(pw, ph));
                op(Kernel::Warp, Params { s_off: 3, f_off: 2, d_off: 3, ..full }, &[(1, &frames), (2, into), (3, &flow)], g8(pw, ph));
                op(Kernel::Copy, Params { s_off: 6, s_c: 1, d_off: 6, ..full }, &[(1, &frames), (2, into)], g8(pw, ph));
                op(Kernel::Copy, Params { s_off: 4, s_c: 1, d_off: 7, ..full }, &[(1, &flow), (2, into)], g8(pw, ph));
                if s == 1 {
                    op(
                        Kernel::Copy,
                        Params { s_c: 4, d_off: 8, d_c: 12, d_w: pw, d_h: ph, mul: 1.0, ..Default::default() },
                        &[(1, &flow), (2, &input)],
                        g8(pw, ph),
                    );
                } else {
                    op(
                        Kernel::Interp,
                        Params { s_c: 8, s_w: pw, s_h: ph, d_c: 12, d_w: bw, d_h: bh, mul: 1.0, ..Default::default() },
                        &[(1, &warped), (2, &input)],
                        g8(bw, bh),
                    );
                    op(
                        Kernel::Interp,
                        Params { s_c: 4, s_w: pw, s_h: ph, d_off: 8, d_c: 12, d_w: bw, d_h: bh, mul: 1.0 / s as f32, ..Default::default() },
                        &[(1, &flow), (2, &input)],
                        g8(bw, bh),
                    );
                }
            }
            // conv0, conv1: stride 2, leaky ReLU.
            let (w1, h1) = (bw / 2, bh / 2);
            let (w2, h2) = (bw / 4, bh / 4);
            let c1 = storage("rife conv0", u64::from(c / 2) * u64::from(w1) * u64::from(h1));
            let mut x = storage("rife features", u64::from(c) * u64::from(w2) * u64::from(h2));
            let mut y = storage("rife features", u64::from(c) * u64::from(w2) * u64::from(h2));
            conv!(
                Kernel::Conv2,
                2,
                Params { s_c: cin, s_w: bw, s_h: bh, d_c: c / 2, d_w: w1, d_h: h1, flags: LEAKY, ..Default::default() },
                &input,
                &c1,
                layer
            );
            conv!(
                Kernel::Conv2,
                2,
                Params { s_c: c / 2, s_w: w1, s_h: h1, d_c: c, d_w: w2, d_h: h2, flags: LEAKY, ..Default::default() },
                &c1,
                &x,
                layer + 1
            );
            // Eight residual blocks: x = leaky(conv(x) + x).
            for r in 0..RESBLOCKS {
                conv!(
                    Kernel::Conv1,
                    8,
                    Params { s_c: c, s_w: w2, s_h: h2, d_c: c, d_w: w2, d_h: h2, flags: LEAKY | RESIDUAL, ..Default::default() },
                    &x,
                    &y,
                    layer + 2 + r
                );
                std::mem::swap(&mut x, &mut y);
            }
            // The deconvolution, then PixelShuffled, resized by the scale and
            // added into the flow and mask.
            let (wt, b, _, _) = &self.weights[layer + 2 + RESBLOCKS];
            let d = storage("rife deconv", u64::from(DECONV_OUT) * u64::from(w1) * u64::from(h1));
            let deconv = Params { s_c: c, s_w: w2, s_h: h2, d_c: DECONV_OUT, d_w: w1, d_h: h1, ..Default::default() };
            let groups = (w2.div_ceil(8), h2.div_ceil(8), 1);
            // Split-K as the convolutions: the small blocks' deconvolutions
            // had 12 to 135 workgroups and took 1.2 ms of a 10.7 ms frame
            // with the GPU mostly idle (#74's pacing fix).
            let (slices, per) = split(groups, c, DECONV_CHANNELS);
            if slices == 1 {
                op(Kernel::Deconv, deconv, &[(1, &x), (2, &d), (4, wt), (5, b)], groups);
            } else {
                let part = storage("rife deconv partial sums", u64::from(slices * DECONV_OUT * w1 * h1));
                op(
                    Kernel::Deconv,
                    Params { flags: PARTIAL, f_off: per, ..deconv },
                    &[(1, &x), (2, &part), (4, wt), (5, b)],
                    (groups.0, groups.1, slices),
                );
                op(
                    Kernel::Reduce,
                    Params { s_c: slices, ..deconv },
                    &[(1, &part), (2, &d), (5, b), (6, &x)],
                    (w1.div_ceil(8), h1.div_ceil(8), DECONV_OUT),
                );
            }
            op(
                Kernel::BlockOut,
                Params { s_w: bw, s_h: bh, d_c: 5, d_w: pw, d_h: ph, mul: s as f32, flags: u32::from(i > 0), ..Default::default() },
                &[(1, &d), (2, &flow)],
                g8(pw, ph),
            );
            layer += 3 + RESBLOCKS;
        }
        op(Kernel::Merge, Params { d_c: 3, d_w: pw, d_h: ph, ..Default::default() }, &[(1, &frames), (2, &result), (3, &flow)], g8(pw, ph));
        op(
            Kernel::Histogram,
            Params { s_w: w, s_h: h, d_w: pw, d_h: ph, ..Default::default() },
            &[(1, &frames), (10, &hist)],
            g8(w.div_ceil(4), h.div_ceil(4)),
        );
        op(Kernel::Cut, Params::default(), &[(10, &hist), (11, &cut)], (1, 1, 1));

        let load_uniform = uniform(Params { s_w: w, s_h: h, d_w: pw, d_h: ph, d_c: 7, mul: TIMESTEP, ..Default::default() });
        let out_uniform = {
            let bytes: Vec<u8> = [w, pw, (plane as u32), 0].iter().flat_map(|v| v.to_le_bytes()).collect();
            device.create_buffer_init(&BufferInitDescriptor {
                label: Some("rife out"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            })
        };
        let out_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rife out"),
            layout: &self.out_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: out_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: result.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: frames.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: cut.as_entire_binding() },
            ],
        });
        debug_assert_eq!(largest.get(), largest_binding((w, h), self.scale), "largest_binding follows plan's buffers");
        Plan { size: (w, h), padded: (pw, ph), load_uniform, frames, hist, result, ops, out_bind }
    }

    /// The network on `prev` and `next` into the plan's `result` (and the
    /// cut flag): every compute pass of a call.
    fn infer(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        prev: &wgpu::TextureView,
        next: &wgpu::TextureView,
        times: Option<&PassTimes<'_>>,
    ) {
        let size = next.texture().size();
        let size = (size.width, size.height);
        if self.plan.as_ref().is_none_or(|p| p.size != size) {
            self.plan = Some(self.plan(device, size));
        }
        let plan = self.plan.as_ref().expect("planned above");
        let load = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rife load"),
            layout: &self.pipeline(Kernel::Load).get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: plan.load_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(prev) },
                wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(next) },
                wgpu::BindGroupEntry { binding: 9, resource: plan.frames.as_entire_binding() },
            ],
        });
        encoder.clear_buffer(&plan.hist, 0, None);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("rife"),
            timestamp_writes: times.and_then(|t| t.compute(true, false)),
        });
        let (pw, ph) = plan.padded;
        pass.set_pipeline(self.pipeline(Kernel::Load));
        pass.set_bind_group(0, &load, &[]);
        pass.dispatch_workgroups(pw.div_ceil(8), ph.div_ceil(8), 1);
        for op in &plan.ops {
            pass.set_pipeline(self.pipeline(op.kernel));
            pass.set_bind_group(0, &op.bind, &[]);
            pass.dispatch_workgroups(op.groups.0, op.groups.1, op.groups.2);
        }
    }
}

impl Generator for Rife {
    fn generate(
        &mut self,
        device: &wgpu::Device,
        _: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frames: Pair<'_>,
        times: Option<PassTimes<'_>>,
    ) -> bool {
        self.infer(device, encoder, frames.prev, frames.next, times.as_ref());
        let plan = self.plan.as_ref().expect("infer plans");
        let attachments = [Some(wgpu::RenderPassColorAttachment {
            view: frames.out,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })];
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rife out"),
            color_attachments: &attachments,
            depth_stencil_attachment: None,
            timestamp_writes: times.as_ref().and_then(|t| t.render(false, true)),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.out_pipeline);
        pass.set_bind_group(0, &plan.out_bind, &[]);
        pass.draw(0..3, 0..1);
        // A cut writes the new frame in this same pass (the flag is only
        // known on the GPU): the seam's real frame, so always true.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_converts_as_ieee_binary16() {
        assert_eq!(f16(0x3c00), 1.0);
        assert_eq!(f16(0xc000), -2.0);
        assert_eq!(f16(0x3555), 0.333_251_95);
        assert_eq!(f16(0x0001), 2f32.powi(-24)); // the smallest subnormal
        assert_eq!(f16(0x7bff), 65504.0);
        assert_eq!(f16(0x0000), 0.0);
        assert!(f16(0x7c00).is_infinite());
    }

    #[test]
    fn the_embedded_weights_are_44_layers_and_nothing_left_over() {
        let l = layers(WEIGHTS).expect("the embedded weights parse");
        assert_eq!(l.len(), 4 * (3 + RESBLOCKS));
        // flownet.param: convrelu_0 6048 = 96 x 7 x 9, deconv_60 73728 =
        // 24 x 192 x 16, convrelu_6 3456 = 32 x 12 x 9.
        assert_eq!((l[0].out, l[0].inp, l[0].weight.len()), (96, 7, 6048));
        assert_eq!((l[10].out, l[10].inp, l[10].weight.len()), (24, 192, 73728));
        assert_eq!((l[33].out, l[33].inp, l[33].weight.len()), (32, 12, 3456));
        assert!(l.iter().all(|l| l.bias.len() == l.out as usize));
        assert!(l.iter().flat_map(|l| &l.weight).all(|w| w.is_finite()));
        // A truncated file is refused, not read short.
        assert!(layers(&WEIGHTS[..WEIGHTS.len() - 2]).is_err());
    }

    /// A device, two frames uploaded, the generator run; the readback of
    /// the 8-bit output and of the network's f32 result.
    struct Run {
        out: Vec<u8>,
        result: Vec<f32>,
        padded: (u32, u32),
    }

    fn device() -> (wgpu::Device, wgpu::Queue) {
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
        eprintln!("GPU: {}", adapter.get_info().name);
        let features = adapter.features() & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::SHADER_F16);
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features: features, ..Default::default() })).unwrap()
    }

    fn texture(device: &wgpu::Device, queue: &wgpu::Queue, (w, h): (u32, u32), rgba: Option<&[u8]>) -> wgpu::Texture {
        let t = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        if let Some(px) = rgba {
            queue.write_texture(
                t.as_image_copy(),
                px,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        t
    }

    fn rgba(rgb: &[u8]) -> Vec<u8> {
        rgb.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
    }

    fn read(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: wgpu::CommandEncoder,
        out: Option<(&wgpu::Texture, (u32, u32))>,
        result: &wgpu::Buffer,
    ) -> (Vec<u8>, Vec<f32>) {
        let mut encoder = encoder;
        // Rows padded to wgpu's copy alignment, stripped after the read.
        let out_rb = out.map(|(t, (w, h))| {
            let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(row * h),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                t.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &b,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            (b, row as usize, (w * 4) as usize)
        });
        let res_rb = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: result.size(),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(result, 0, &res_rb, 0, result.size());
        queue.submit([encoder.finish()]);
        if let Some((b, _, _)) = &out_rb {
            b.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        }
        res_rb.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = out_rb
            .map(|(b, row, used)| b.get_mapped_range(..).unwrap().chunks_exact(row).flat_map(|r| r[..used].to_vec()).collect())
            .unwrap_or_default();
        let result = res_rb.get_mapped_range(..).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        (out, result)
    }

    fn run(rife: &mut Rife, device: &wgpu::Device, queue: &wgpu::Queue, size: (u32, u32), prev: &[u8], next: &[u8]) -> Run {
        let tex = [texture(device, queue, size, Some(prev)), texture(device, queue, size, Some(next)), texture(device, queue, size, None)];
        let views = tex.each_ref().map(|t| t.create_view(&Default::default()));
        let mut encoder = device.create_command_encoder(&Default::default());
        assert!(rife.generate(device, queue, &mut encoder, Pair { prev: &views[0], next: &views[1], out: &views[2] }, None));
        let plan = rife.plan.as_ref().unwrap();
        let padded = plan.padded;
        let (out, result) = read(device, queue, encoder, Some((&tex[2], size)), &plan.result);
        Run { out, result, padded }
    }

    /// The golden test (#74): the port against the original. The fixture
    /// (golden/) was made once by golden/reference.py: rife-ncnn-vulkan's
    /// models/rife-v4.6 run by ncnn's own CPU runtime (the `ncnn` wheel
    /// 1.0.20261020 in a scratch venv, Python 3.14, fp32 throughout),
    /// with rife.Warp ported to numpy from rife-ncnn-vulkan's src/warp.cpp;
    /// two 96x64 frames of gradients, a textured square moved by (8, 4) and
    /// shared noise; timestep 0.5. `cargo test --features dev -- --ignored gpu_rife`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_matches_ncnn_on_the_golden_frames() {
        let (device, queue) = device();
        let size = (96, 64);
        let prev = rgba(include_bytes!("rife/golden/in0.rgb"));
        let next = rgba(include_bytes!("rife/golden/in1.rgb"));
        let want: Vec<f32> = include_bytes!("rife/golden/out.f32").as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        // f32: the reference's own precision.
        let mut rife = Rife::with_precision(&device, 1.0, false).unwrap();
        let got = run(&mut rife, &device, &queue, size, &prev, &next);
        assert_eq!(got.padded, size, "96x64 needs no padding");
        assert_eq!(got.result.len(), want.len());
        let diffs: Vec<f32> = got.result.iter().zip(&want).map(|(a, b)| (a - b).abs()).collect();
        let max = diffs.iter().copied().fold(0.0, f32::max);
        let mean = diffs.iter().sum::<f32>() / diffs.len() as f32;
        eprintln!("golden: max |diff| {max:.2e}, mean {mean:.2e} over {} values", diffs.len());
        // f32 throughout on both sides; the order of the sums differs.
        assert!(max < 1e-3, "max |diff| {max}");
        assert!(mean < 1e-5, "mean |diff| {mean}");
        // The 8-bit output is the result, rounded.
        let (w, h) = size;
        let plane = (w * h) as usize;
        for i in 0..plane {
            for c in 0..3 {
                let want8 = (want[c * plane + i].clamp(0.0, 1.0) * 255.0).round();
                assert!((f32::from(got.out[i * 4 + c]) - want8).abs() <= 1.0, "pixel {i} channel {c}");
            }
        }
    }

    /// fp16 (where the GPU has it) against f32 on the golden frames: the
    /// 8-bit pictures within a few levels.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_fp16_follows_fp32() {
        let (device, queue) = device();
        if !device.features().contains(wgpu::Features::SHADER_F16) {
            eprintln!("no fp16 shaders on this GPU");
            return;
        }
        let size = (96, 64);
        let prev = rgba(include_bytes!("rife/golden/in0.rgb"));
        let next = rgba(include_bytes!("rife/golden/in1.rgb"));
        let full = run(&mut Rife::with_precision(&device, 1.0, false).unwrap(), &device, &queue, size, &prev, &next);
        let half = run(&mut Rife::with_precision(&device, 1.0, true).unwrap(), &device, &queue, size, &prev, &next);
        let d: Vec<i32> = full.out.iter().zip(&half.out).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).collect();
        let max = d.iter().copied().max().unwrap();
        let mean = d.iter().sum::<i32>() as f64 / d.len() as f64;
        let over2 = d.iter().filter(|&&v| v > 2).count() as f64 / d.len() as f64;
        eprintln!("fp16 against fp32: max {max} levels, mean {mean:.3}, {:.3} % over 2 levels", over2 * 100.0);
        // fp16 sums 1728 products a convolution: a few pixels where the
        // flow is sensitive move by a few levels, the picture does not.
        assert!(max <= 8 && mean < 0.25 && over2 < 0.005, "max {max}, mean {mean}, over 2 levels {over2}");
    }

    /// A bright square moving across a gradient: its halfway position.
    fn moving(size: (u32, u32), at: (u32, u32)) -> Vec<u8> {
        moving_sized(size, at, 32)
    }

    /// `moving` with a square of `side` px, its checks `side / 8` px.
    fn moving_sized(size: (u32, u32), at: (u32, u32), side: u32) -> Vec<u8> {
        let (w, h) = size;
        let check = side / 8;
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let inside = x >= at.0 && x < at.0 + side && y >= at.1 && y < at.1 + side;
                let v = if inside {
                    if ((x - at.0) / check + (y - at.1) / check).is_multiple_of(2) { 230 } else { 170 }
                } else {
                    (40 + (x + y) / check % 60) as u8
                };
                px.extend([v, v, v, 255]);
            }
        }
        px
    }

    /// The square's centre in a frame: the mean of the pixels over 150.
    fn centre(px: &[u8], (w, _): (u32, u32)) -> (f32, f32) {
        let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
        for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
            if p[0] > 150 {
                sx += (i as u32 % w) as f32;
                sy += (i as u32 / w) as f32;
                n += 1.0;
            }
        }
        (sx / n, sy / n)
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_generates_the_halfway_picture() {
        let (device, queue) = device();
        // Full resolution, and the scale `--framegen rife` runs at. At scale
        // `s` the network sees the frame at `s` of its size, so the frame,
        // the square and its motion grow by 1/s: every scale gets the
        // picture full resolution gets at 256x192 (the fine detail a lower
        // scale loses is #81's score's to show, not this test's).
        for scale in [1.0, SCALE] {
            let k = (1.0 / scale) as u32;
            let size = (256 * k, 192 * k);
            let (a, b) = (moving_sized(size, (64 * k, 64 * k), 32 * k), moving_sized(size, (96 * k, 80 * k), 32 * k));
            let mut rife = Rife::new(&device, scale).unwrap();
            let got = run(&mut rife, &device, &queue, size, &a, &b);
            let (ca, cb, cg) = (centre(&a, size), centre(&b, size), centre(&got.out, size));
            eprintln!("scale {scale} at {size:?} centre: prev {ca:?}, next {cb:?}, generated {cg:?}");
            // Halfway, within 2 px of the 256x192 picture. A blend would put
            // the mean halfway too, so the square must also stay one square.
            let tol = 2.0 * k as f32;
            assert!((cg.0 - (ca.0 + cb.0) / 2.0).abs() < tol && (cg.1 - (ca.1 + cb.1) / 2.0).abs() < tol, "{cg:?} within {tol} px");
            let bright = got.out.as_chunks::<4>().0.iter().filter(|p| p[0] > 150).count();
            let one = a.as_chunks::<4>().0.iter().filter(|p| p[0] > 150).count();
            assert!((bright as f32 - one as f32).abs() / (one as f32) < 0.15, "{bright} bright pixels against {one}: not one square");
        }
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_a_cut_shows_the_new_frame() {
        let (device, queue) = device();
        let size = (128, 96);
        let dark: Vec<u8> = (0..128 * 96).flat_map(|i| [(20 + i % 13) as u8, 25, 30, 255]).collect();
        let bright: Vec<u8> = (0..128 * 96).flat_map(|i| [(200 + i % 17) as u8, 210, 220, 255]).collect();
        let mut rife = Rife::new(&device, 1.0).unwrap();
        let got = run(&mut rife, &device, &queue, size, &dark, &bright);
        assert!(got.out == bright, "a cut shows the new frame, texel for texel");
        // No cut: the moving square is interpolated, not the new frame.
        let (a, b) = (moving(size, (16, 16)), moving(size, (40, 32)));
        let got = run(&mut rife, &device, &queue, size, &a, &b);
        assert!(got.out != b);
    }

    /// GPU time per generated frame at 1080p: `cargo test --release
    /// --features dev -- --ignored --nocapture gpu_rife_time`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_time_at_1080p() {
        let (device, queue) = device();
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            eprintln!("no timestamp queries on this GPU");
            return;
        }
        let size = (1920, 1080);
        let (a, b) = (moving(size, (600, 400)), moving(size, (640, 420)));
        let tex =
            [texture(&device, &queue, size, Some(&a)), texture(&device, &queue, size, Some(&b)), texture(&device, &queue, size, None)];
        let views = tex.each_ref().map(|t| t.create_view(&Default::default()));
        let mut medians = Vec::new();
        for scale in [1.0, 0.5, 0.25] {
            let mut rife = Rife::new(&device, scale).unwrap();
            if scale == 1.0 {
                eprintln!("fp16: {}", rife.fp16);
            }
            let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
                label: None,
                ty: wgpu::QueryType::Timestamp,
                count: crate::gputime::QUERIES,
            });
            let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(crate::gputime::QUERIES) * 8,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let mut ms = Vec::new();
            for i in 0..40 {
                let rb = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: resolve.size(),
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let mut encoder = device.create_command_encoder(&Default::default());
                let times = crate::gputime::PassTimes::for_test(&queries);
                rife.generate(&device, &queue, &mut encoder, Pair { prev: &views[0], next: &views[1], out: &views[2] }, Some(times));
                encoder.resolve_query_set(&queries, 0..crate::gputime::QUERIES, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &rb, 0, resolve.size());
                queue.submit([encoder.finish()]);
                rb.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                let t: Vec<u64> = rb.get_mapped_range(..).unwrap().as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b)).collect();
                let (s, e) = crate::gputime::generate_span(&t);
                if i >= 5 {
                    ms.push((e - s) as f64 * f64::from(queue.get_timestamp_period()) / 1e6);
                }
            }
            ms.sort_by(f64::total_cmp);
            eprintln!(
                "rife scale {scale} at 1920x1080: median {:.2} ms, p90 {:.2} ms, {} frames",
                ms[ms.len() / 2],
                ms[ms.len() * 9 / 10],
                ms.len()
            );
            medians.push((scale, ms[ms.len() / 2]));
        }
        let at = medians.iter().find(|(s, _)| *s == SCALE).expect("SCALE is measured").1;
        assert!(at <= 10.0, "the budget is 10 ms at 1080p; scale {SCALE} took {at:.2} ms");
    }

    /// Where a frame's GPU time goes at 1080p: each dispatch in its own
    /// timed pass, summed per kernel and per block (#74's pacing fix):
    /// `cargo test --release --features dev -- --ignored --nocapture gpu_rife_kernel_times`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_kernel_times_at_1080p() {
        let (device, queue) = device();
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            eprintln!("no timestamp queries on this GPU");
            return;
        }
        let size = (1920, 1080);
        let (a, b) = (moving(size, (600, 400)), moving(size, (640, 420)));
        let tex = [texture(&device, &queue, size, Some(&a)), texture(&device, &queue, size, Some(&b))];
        let views = tex.each_ref().map(|t| t.create_view(&Default::default()));
        let mut rife = Rife::new(&device, SCALE).unwrap();
        let mut encoder = device.create_command_encoder(&Default::default());
        rife.infer(&device, &mut encoder, &views[0], &views[1], None);
        queue.submit([encoder.finish()]);
        let plan = rife.plan.as_ref().unwrap();
        let n = plan.ops.len() as u32;
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count: n * 2 });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(n) * 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let rb = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: resolve.size(),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let name = |k: Kernel| KERNELS.iter().find(|(x, _)| *x == k).unwrap().1;
        let mut per_op = vec![Vec::new(); n as usize];
        for i in 0..60 {
            let mut encoder = device.create_command_encoder(&Default::default());
            for (j, op) in plan.ops.iter().enumerate() {
                let j = j as u32;
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: Some(wgpu::ComputePassTimestampWrites {
                        query_set: &queries,
                        beginning_of_pass_write_index: Some(j * 2),
                        end_of_pass_write_index: Some(j * 2 + 1),
                    }),
                });
                pass.set_pipeline(rife.pipeline(op.kernel));
                pass.set_bind_group(0, &op.bind, &[]);
                pass.dispatch_workgroups(op.groups.0, op.groups.1, op.groups.2);
            }
            encoder.resolve_query_set(&queries, 0..n * 2, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &rb, 0, resolve.size());
            queue.submit([encoder.finish()]);
            rb.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let t: Vec<u64> = rb.get_mapped_range(..).unwrap().as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b)).collect();
            rb.unmap();
            if i >= 10 {
                for j in 0..n as usize {
                    per_op[j].push((t[j * 2 + 1] - t[j * 2]) as f64 * f64::from(queue.get_timestamp_period()) / 1e6);
                }
            }
        }
        let median = |v: &Vec<f64>| {
            let mut v = v.clone();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let mut by_kernel: Vec<(&str, f64, usize)> = Vec::new();
        let mut total = 0.0;
        for (op, times) in plan.ops.iter().zip(&per_op) {
            let m = median(times);
            total += m;
            match by_kernel.iter_mut().find(|(k, _, _)| *k == name(op.kernel)) {
                Some(e) => {
                    e.1 += m;
                    e.2 += 1;
                }
                None => by_kernel.push((name(op.kernel), m, 1)),
            }
        }
        by_kernel.sort_by(|a, b| b.1.total_cmp(&a.1));
        eprintln!("rife scale {SCALE}: {n} dispatches, {total:.2} ms summed (each in its own pass)");
        for (k, ms, count) in by_kernel {
            eprintln!("  {k:<10} {ms:6.2} ms over {count} dispatches");
        }
        let mut cum = 0.0;
        for (j, (op, times)) in plan.ops.iter().zip(&per_op).enumerate() {
            let m = median(times);
            cum += m;
            if m > 0.1 {
                eprintln!("  op {j:3} {:<10} {m:5.2} ms (cumulative {cum:5.2}) groups {:?}", name(op.kernel), op.groups);
            }
        }
    }

    /// GPU time per generated frame at 1080p paced as the window runs it:
    /// one frame every 16.7 ms, the GPU idle between (#74's pacing fix):
    /// `cargo test --release --features dev -- --ignored --nocapture gpu_rife_paced`.
    /// Its median, p99 and max per scale, and the frames a 60 Hz window
    /// would miss (over 16.7 ms minus the rest of a frame's GPU work, ~1 ms).
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_rife_paced_time_at_1080p() {
        let (device, queue) = device();
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            eprintln!("no timestamp queries on this GPU");
            return;
        }
        let size = (1920, 1080);
        let (a, b) = (moving(size, (600, 400)), moving(size, (640, 420)));
        let tex =
            [texture(&device, &queue, size, Some(&a)), texture(&device, &queue, size, Some(&b)), texture(&device, &queue, size, None)];
        let views = tex.each_ref().map(|t| t.create_view(&Default::default()));
        let scales: Vec<f32> = std::env::var("RIFE_SCALES")
            .ok()
            .map(|s| s.split(',').map(|x| x.parse().expect("RIFE_SCALES: numbers")).collect())
            .unwrap_or_else(|| vec![SCALE]);
        let frames: usize = std::env::var("RIFE_FRAMES").ok().map_or(600, |s| s.parse().expect("RIFE_FRAMES: a number"));
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: None,
            ty: wgpu::QueryType::Timestamp,
            count: crate::gputime::QUERIES,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(crate::gputime::QUERIES) * 8,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let rb = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: resolve.size(),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let interval = std::time::Duration::from_micros(16_667);
        for scale in scales {
            let mut rife = Rife::new(&device, scale).unwrap();
            let mut ms = Vec::new();
            let mut next = std::time::Instant::now();
            for i in 0..frames + 30 {
                let mut encoder = device.create_command_encoder(&Default::default());
                let times = crate::gputime::PassTimes::for_test(&queries);
                rife.generate(&device, &queue, &mut encoder, Pair { prev: &views[0], next: &views[1], out: &views[2] }, Some(times));
                encoder.resolve_query_set(&queries, 0..crate::gputime::QUERIES, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &rb, 0, resolve.size());
                queue.submit([encoder.finish()]);
                rb.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                let t: Vec<u64> = rb.get_mapped_range(..).unwrap().as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b)).collect();
                rb.unmap();
                let (s, e) = crate::gputime::generate_span(&t);
                // The first frames settle the clocks.
                if i >= 30 {
                    ms.push((e - s) as f64 * f64::from(queue.get_timestamp_period()) / 1e6);
                }
                next += interval;
                if let Some(rest) = next.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(rest);
                }
            }
            let mut sorted = ms.clone();
            sorted.sort_by(f64::total_cmp);
            let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
            let over = ms.iter().filter(|&&m| m > 15.7).count();
            eprintln!(
                "rife paced scale {scale}, fp16 {}: median {:.2} ms, p90 {:.2}, p99 {:.2}, max {:.2}; over 15.7 ms: {over} of {}",
                rife.fp16,
                at(0.5),
                at(0.9),
                at(0.99),
                at(1.0),
                ms.len()
            );
            // The window's budget: a refresh with room for its other passes
            // and the compositor (#74's pacing fix).
            if scale == SCALE {
                assert!(at(0.99) <= 8.0, "rife at scale {SCALE}: paced p99 {:.2} ms, over the 8 ms budget", at(0.99));
            }
        }
    }
}
