//! A skip-frame score for frame generation (#81, dev builds only): how
//! close a generator's in-between picture comes to a real one, on a
//! recorded clip, headless.
//!
//! - **The test:** three new frames in a row, N0, N1 and N2 (duplicates
//!   skipped, duplicate.rs's detector as the recorder ran it). The generator
//!   makes the picture halfway between N0 and N2, and it is compared with
//!   the real N1. The gap is twice the live one (N0 to N2 spans two source
//!   frames), so the scores are harsher than what the window shows, but the
//!   same for every approach.
//! - **Chained:** the triples share their ends, N0 N1 N2, then N2 N3 N4, so
//!   the generator sees each pair's first frame as the last pair's second,
//!   as in the window, and a generator that keeps state from pair to pair
//!   (ffx-quality's static-HUD counts) keeps it as it would live. A gap in
//!   the clip starts the chain again. `--every N` scores every Nth triple
//!   only, faster, with the chain broken between them.
//! - **The pictures:** each frame goes through the window's own convert
//!   pass (app.rs `convert_shader`, picture.rs at its defaults), on the GPU,
//!   at the clip's size, as the window feeds a generator.
//! - **The scores**, on BT.709 luma in 8-bit levels: PSNR, and **tear
//!   blocks**: 16×16 blocks whose mean absolute error is over `TEAR`
//!   levels, the hard local failures a person sees (a tear, a seam, a
//!   block of wrong pixels), which a whole-picture PSNR averages away. With
//!   `--hud X,Y,W,H` the same two inside that rectangle too (a HUD such as
//!   a minimap): a static-HUD mask that drags or freezes it shows there. A
//!   mask that freezes moving content outside it shows as tear blocks.
//! - **The worst triples** (`--worst K`, most tear blocks, then the lowest
//!   PSNR), with `--out DIR`: N1, the generated picture and the error map
//!   (the error ×4, tear blocks outlined red) as PNGs, to look at.
//! - A pair the generator declines (a scene cut: `generate` false) is not
//!   scored: the window shows a real frame there. Counted.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::clip::{Reader, Record};
use crate::duplicate::{Detector, Kind};
use crate::framegen::{self, Generator, Mode, Pair};
use crate::picture::{self, Coding, Picture};

/// A block's side, pixels.
pub const BLOCK: u32 = 16;
/// A block whose mean absolute luma error is over this many 8-bit levels is
/// a tear block. About a tenth of the range: a block that far off is a
/// visible local failure, while the shimmer of fine texture misplaced by a
/// pixel stays under it.
pub const TEAR: f64 = 24.0;
/// `--worst` when not given.
pub const WORST: usize = 5;

/// A rectangle of the frame, pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// `X,Y,W,H`, inside a `width`×`height` frame.
    pub fn parse(v: &str, width: u32, height: u32) -> Result<Self, String> {
        let bad = || format!("--hud {v}: X,Y,W,H in pixels, inside the {width}x{height} frame");
        let n: Vec<u32> = v.split(',').map(|p| p.trim().parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
        let [x, y, w, h] = n[..] else { return Err(bad()) };
        if w == 0 || h == 0 || x.checked_add(w).is_none_or(|r| r > width) || y.checked_add(h).is_none_or(|b| b > height) {
            return Err(bad());
        }
        Ok(Self { x, y, w, h })
    }

    fn whole(width: u32, height: u32) -> Self {
        Self { x: 0, y: 0, w: width, h: height }
    }
}

/// What to score and how (main.rs's flags).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub clip: PathBuf,
    pub mode: Mode,
    /// Score every Nth triple (1: all).
    pub every: usize,
    /// How many of the worst triples to keep as PNGs.
    pub worst: usize,
    /// Where the PNGs go; None: none written.
    pub out: Option<PathBuf>,
    /// `--hud X,Y,W,H`, checked against the clip's size once it is open.
    pub hud: Option<String>,
}

/// Picks chained triples of new frames from a clip's frames: N0 N1 N2,
/// then N2 N3 N4. `T` is what the caller keeps for each new frame.
#[derive(Debug, Default)]
pub struct Triples<T> {
    held: Vec<T>,
}

impl<T: Clone> Triples<T> {
    pub fn new() -> Self {
        Self { held: Vec::new() }
    }

    /// A frame of the clip, in order: a triple when this new frame closes
    /// one. Duplicates are skipped.
    pub fn frame(&mut self, kind: Kind, value: impl FnOnce() -> T) -> Option<[T; 3]> {
        if kind == Kind::Duplicate {
            return None;
        }
        self.held.push(value());
        if self.held.len() < 3 {
            return None;
        }
        let triple: [T; 3] = [self.held[0].clone(), self.held[1].clone(), self.held[2].clone()];
        self.held = vec![triple[2].clone()];
        Some(triple)
    }

    /// Frames are missing here: no triple spans it.
    pub fn gap(&mut self) {
        self.held.clear();
    }

    /// What is held for the next triple.
    pub fn held(&self) -> &[T] {
        &self.held
    }
}

/// BT.709 luma of RGBA8 pixels, in 8-bit levels.
pub fn luma(rgba: &[u8]) -> Vec<f32> {
    rgba.as_chunks::<4>().0.iter().map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])).collect()
}

/// A region's error: its squared error and pixels for PSNR, and its blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Region {
    pub sq_err: f64,
    pub pixels: u64,
    pub blocks: u32,
    pub tear_blocks: u32,
}

impl Region {
    pub fn psnr(&self) -> f64 {
        psnr(self.sq_err, self.pixels)
    }

    fn add(&mut self, o: &Region) {
        self.sq_err += o.sq_err;
        self.pixels += o.pixels;
        self.blocks += o.blocks;
        self.tear_blocks += o.tear_blocks;
    }
}

/// PSNR over 8-bit levels; infinite for no error.
pub fn psnr(sq_err: f64, pixels: u64) -> f64 {
    if pixels == 0 || sq_err == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / (sq_err / pixels as f64)).log10()
}

/// The error of `got` against `real` (luma planes `width` wide) inside
/// `rect`, in `BLOCK` blocks from its corner (a partial block at its right
/// or bottom edge counts over its own pixels). Also each tear block's
/// rectangle, for the error map.
pub fn region(got: &[f32], real: &[f32], width: u32, rect: Rect) -> (Region, Vec<Rect>) {
    let mut r = Region::default();
    let mut tears = Vec::new();
    for by in (rect.y..rect.y + rect.h).step_by(BLOCK as usize) {
        for bx in (rect.x..rect.x + rect.w).step_by(BLOCK as usize) {
            let (bw, bh) = (BLOCK.min(rect.x + rect.w - bx), BLOCK.min(rect.y + rect.h - by));
            let (mut abs, mut sq) = (0.0f64, 0.0f64);
            for y in by..by + bh {
                let row = (y * width) as usize;
                for x in bx..bx + bw {
                    let d = f64::from(got[row + x as usize] - real[row + x as usize]);
                    abs += d.abs();
                    sq += d * d;
                }
            }
            let n = u64::from(bw * bh);
            r.sq_err += sq;
            r.pixels += n;
            r.blocks += 1;
            if abs / n as f64 > TEAR {
                r.tear_blocks += 1;
                tears.push(Rect { x: bx, y: by, w: bw, h: bh });
            }
        }
    }
    (r, tears)
}

/// One scored triple.
#[derive(Debug, Clone)]
struct Scored {
    /// The clip's frame indices of N0, N1, N2.
    frames: [usize; 3],
    picture: Region,
    hud: Option<Region>,
}

/// Worse first: more tear blocks, then a lower PSNR.
fn worse(a: &Scored, b: &Scored) -> Ordering {
    b.picture.tear_blocks.cmp(&a.picture.tear_blocks).then(a.picture.psnr().total_cmp(&b.picture.psnr()))
}

/// The pictures kept for a worst triple.
struct Kept {
    scored: Scored,
    real: Vec<u8>,
    got: Vec<u8>,
    error: Vec<u8>,
}

/// The whole clip's result.
#[derive(Debug, Default)]
pub struct Totals {
    pub triples: u64,
    pub cuts: u64,
    pub picture: Region,
    pub hud: Region,
    /// Triples with at least one tear block, and the most in one.
    pub torn: u64,
    pub max_tears: u32,
    pub worst_psnr: f64,
    /// The largest difference in any channel of any pixel, generated
    /// against real (8-bit): what a test of the frames' colour reads.
    pub max_channel: u8,
}

/// The window's GPU work for the score: the convert pass, the frames a
/// generator reads, its output, and the readback.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: (u32, u32),
    yuyv: wgpu::Texture,
    convert_pipeline: wgpu::RenderPipeline,
    convert_bind: wgpu::BindGroup,
    /// The new frames a chain holds (at most three at once), by slot.
    frames: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Bytes per row in `readback`: a multiple of 256.
    row: u32,
}

impl Gpu {
    fn new((w, h): (u32, u32)) -> Result<Self, String> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = crate::app::BACKENDS;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("graphics adapter: {e}"))?;
        let info = adapter.get_info();
        eprintln!("score: on {} ({:?})", info.name, info.backend);
        // Half floats where the GPU has them, for generators that use them.
        let required_features = adapter.features() & wgpu::Features::SHADER_F16;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() }))
                .map_err(|e| format!("graphics device: {e}"))?;
        let texture = |label, width, usage| {
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
        // As the window makes them (app.rs): the YUYV upload, the new frames
        // a generator reads, and the picture it writes.
        let yuyv = texture("score yuyv", w / 2, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let frame_usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC;
        let frames: Vec<wgpu::Texture> = (0..3).map(|_| texture("score frame", w, frame_usage)).collect();
        let views = frames.iter().map(|t| t.create_view(&Default::default())).collect();
        let out = texture("score out", w, frame_usage | wgpu::TextureUsages::COPY_DST);
        let out_view = out.create_view(&Default::default());
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("score convert"),
            entries: &[crate::app::texture_entry(0, false), crate::app::uniform_entry(1)],
        });
        let picture_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("score picture"),
            size: size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The picture controls at their defaults: a clip is raw YUYV.
        queue.write_buffer(&picture_buffer, 0, &picture::bytes(&Picture::DEFAULT.uniform(w, h, Coding::Raw)));
        let yuyv_view = yuyv.create_view(&Default::default());
        let convert_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("score convert"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&yuyv_view) },
                wgpu::BindGroupEntry { binding: 1, resource: picture_buffer.as_entire_binding() },
            ],
        });
        let convert_pipeline =
            crate::app::pipeline(&device, "score convert", &crate::app::convert_shader(w), &layout, wgpu::TextureFormat::Rgba8Unorm);
        let row = (w * 4).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("score readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self { device, queue, size: (w, h), yuyv, convert_pipeline, convert_bind, frames, views, out, out_view, readback, row })
    }

    /// A YUYV frame through the window's convert pass into `slot`.
    fn convert(&self, data: &[u8], slot: usize) {
        let (w, h) = self.size;
        self.queue.write_texture(
            self.yuyv.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w / 2, height: h, depth_or_array_layers: 1 },
        );
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &self.views[slot],
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("score convert"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.convert_pipeline);
            pass.set_bind_group(0, &self.convert_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
    }

    /// The picture halfway between the frames in slots `prev` and `next`;
    /// false: the generator declined the pair.
    fn generate(&self, generator: &mut dyn Generator, prev: usize, next: usize) -> bool {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let frames = Pair { prev: &self.views[prev], next: &self.views[next], out: &self.out_view };
        let made = generator.generate(&self.device, &self.queue, &mut encoder, frames, None);
        self.queue.submit([encoder.finish()]);
        made
    }

    /// A texture's pixels, RGBA8, tightly packed.
    fn read(&self, texture: &wgpu::Texture) -> Result<Vec<u8>, String> {
        let (w, h) = self.size;
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback.map_async(wgpu::MapMode::Read, .., move |r| {
            // The receiver waits below; it is gone only if this function
            // already returned with an error.
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("the GPU: {e}"))?;
        rx.recv().map_err(|e| format!("the readback: {e}"))?.map_err(|e| format!("the readback: {e}"))?;
        let mapped = self.readback.get_mapped_range(..).map_err(|e| format!("the readback: {e}"))?;
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let start = (y * self.row) as usize;
            px.extend_from_slice(&mapped[start..start + (w * 4) as usize]);
        }
        drop(mapped);
        self.readback.unmap();
        Ok(px)
    }
}

/// The error map: the luma error ×4 in grey, each tear block outlined red;
/// RGB8.
fn error_map(got: &[f32], real: &[f32], width: u32, tears: &[Rect]) -> Vec<u8> {
    let mut rgb: Vec<u8> = got.iter().zip(real).flat_map(|(g, r)| [((g - r).abs() * 4.0).min(255.0) as u8; 3]).collect();
    for t in tears {
        for y in t.y..t.y + t.h {
            for x in t.x..t.x + t.w {
                if x == t.x || y == t.y || x + 1 == t.x + t.w || y + 1 == t.y + t.h {
                    let i = ((y * width + x) * 3) as usize;
                    rgb[i..i + 3].copy_from_slice(&[255, 0, 0]);
                }
            }
        }
    }
    rgb
}

fn rgb_of(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect()
}

fn write_png(path: &Path, (w, h): (u32, u32), rgb: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    let mut writer = e.write_header().map_err(|e| format!("{}: {e}", path.display()))?;
    writer.write_image_data(rgb).map_err(|e| format!("{}: {e}", path.display()))?;
    writer.finish().map_err(|e| format!("{}: {e}", path.display()))
}

/// Scores `opts.mode`'s generator on `opts.clip`; the table to stderr.
pub fn run(opts: &Options) -> Result<Totals, String> {
    if opts.every == 0 {
        return Err("--every N: N is 1 or more".into());
    }
    let reader = Reader::open(&opts.clip)?;
    let header = reader.header();
    let size = (header.width, header.height);
    let hud = opts.hud.as_deref().map(|v| Rect::parse(v, size.0, size.1)).transpose()?;
    if let Some(dir) = &opts.out {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let gpu = Gpu::new(size)?;
    let generator = framegen::generator(opts.mode, &gpu.device)
        .ok_or_else(|| format!("--framegen {}: runs no generator to score", opts.mode.as_str()))?;
    score(opts, reader, hud, gpu, generator)
}

/// The score with `generator` on `gpu` (tests bring their own).
fn score(opts: &Options, mut reader: Reader, hud: Option<Rect>, gpu: Gpu, mut generator: Box<dyn Generator>) -> Result<Totals, String> {
    let header = reader.header();
    let size = (header.width, header.height);
    let mut detector = Detector::new(size.0, size.1);
    // The frames the chain holds: (clip index, slot).
    let mut chain: Triples<(usize, usize)> = Triples::new();
    let mut totals = Totals { worst_psnr: f64::INFINITY, ..Default::default() };
    let mut kept: Vec<Kept> = Vec::new();
    let whole = Rect::whole(size.0, size.1);
    let (mut index, mut seen) = (0usize, 0u64);
    while let Some(record) = reader.next()? {
        let data = match record {
            Record::Gap { .. } => {
                chain.gap();
                continue;
            }
            Record::Frame { data, .. } => data,
        };
        let kind = detector.classify(&data);
        let i = index;
        index += 1;
        // A free slot: the chain holds at most two frames before a third.
        let slot = (0..3).find(|s| !chain.held().iter().any(|(_, held)| held == s)).expect("three slots for two held frames");
        let Some([(f0, s0), (f1, s1), (f2, s2)]) = chain.frame(kind, || (i, slot)) else {
            if kind == Kind::New {
                gpu.convert(&data, slot);
            }
            continue;
        };
        gpu.convert(&data, s2);
        seen += 1;
        if (seen - 1) % opts.every as u64 != 0 {
            continue;
        }
        if !gpu.generate(generator.as_mut(), s0, s2) {
            totals.cuts += 1;
            continue;
        }
        let got_px = gpu.read(&gpu.out)?;
        let real_px = gpu.read(&gpu.frames[s1])?;
        let channel = got_px.iter().zip(&real_px).map(|(g, r)| g.abs_diff(*r)).max().unwrap_or(0);
        totals.max_channel = totals.max_channel.max(channel);
        let (got, real) = (luma(&got_px), luma(&real_px));
        let (picture, tears) = region(&got, &real, size.0, whole);
        let hud_region = hud.map(|r| region(&got, &real, size.0, r).0);
        totals.triples += 1;
        totals.picture.add(&picture);
        if let Some(h) = &hud_region {
            totals.hud.add(h);
        }
        if picture.tear_blocks > 0 {
            totals.torn += 1;
        }
        totals.max_tears = totals.max_tears.max(picture.tear_blocks);
        totals.worst_psnr = totals.worst_psnr.min(picture.psnr());
        let scored = Scored { frames: [f0, f1, f2], picture, hud: hud_region };
        // Keep the worst `opts.worst`, with their pictures when they are
        // written out.
        if opts.worst > 0 && (kept.len() < opts.worst || kept.last().is_some_and(|k| worse(&scored, &k.scored) == Ordering::Less)) {
            let pictures = opts.out.is_some();
            kept.push(Kept {
                error: if pictures { error_map(&got, &real, size.0, &tears) } else { Vec::new() },
                real: if pictures { rgb_of(&real_px) } else { Vec::new() },
                got: if pictures { rgb_of(&got_px) } else { Vec::new() },
                scored,
            });
            kept.sort_by(|a, b| worse(&a.scored, &b.scored));
            kept.truncate(opts.worst);
        }
    }
    report(opts, hud, &totals, &kept);
    if let Some(dir) = &opts.out {
        for (n, k) in kept.iter().enumerate() {
            let [_, f1, _] = k.scored.frames;
            for (what, px) in [("real", &k.real), ("generated", &k.got), ("error", &k.error)] {
                write_png(&dir.join(format!("worst-{}-frame-{f1}-{what}.png", n + 1)), size, px)?;
            }
        }
        eprintln!("score: the worst {} triples' pictures in {}", kept.len(), dir.display());
    }
    Ok(totals)
}

fn report(opts: &Options, hud: Option<Rect>, t: &Totals, kept: &[Kept]) {
    let name = opts.clip.file_name().map_or_else(|| opts.clip.display().to_string(), |n| n.to_string_lossy().into_owned());
    eprintln!(
        "score: {name}, framegen {}: {} triples scored (every {}), {} declined (scene cuts); tear block: a {BLOCK}x{BLOCK} block over {TEAR} levels mean abs luma error",
        opts.mode.as_str(),
        t.triples,
        opts.every,
        t.cuts
    );
    if t.triples == 0 {
        eprintln!("score: no triple of new frames in the clip");
        return;
    }
    let share = t.torn as f64 / t.triples as f64 * 100.0;
    eprintln!(
        "score: picture  PSNR {:.2} dB (worst triple {:.2})  tear blocks {} ({:.2} a triple, max {}, {share:.0} % of triples have one)",
        t.picture.psnr(),
        t.worst_psnr,
        t.picture.tear_blocks,
        t.picture.tear_blocks as f64 / t.triples as f64,
        t.max_tears
    );
    if let Some(r) = hud {
        eprintln!(
            "score: hud {},{},{},{}  PSNR {:.2} dB  tear blocks {} ({:.2} a triple)",
            r.x,
            r.y,
            r.w,
            r.h,
            t.hud.psnr(),
            t.hud.tear_blocks,
            t.hud.tear_blocks as f64 / t.triples as f64
        );
    }
    for (n, k) in kept.iter().enumerate() {
        let [f0, f1, f2] = k.scored.frames;
        let hud = k.scored.hud.map_or(String::new(), |h| format!(", hud {} tear blocks", h.tear_blocks));
        eprintln!(
            "score: worst {}: frames {f0} {f1} {f2}: {} tear blocks, {:.2} dB{hud}",
            n + 1,
            k.scored.picture.tear_blocks,
            k.scored.picture.psnr()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Kind::{Duplicate as D, New as N};

    /// Each frame's index into the chain picker, as `run` feeds it.
    fn picked(frames: &[Option<Kind>]) -> Vec<[usize; 3]> {
        let mut t = Triples::new();
        let mut out = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            match f {
                Some(k) => out.extend(t.frame(*k, || i)),
                None => t.gap(),
            }
        }
        out
    }

    #[test]
    fn triples_of_new_frames_are_chained_past_duplicates() {
        // 2:2, the clip starting on a new frame.
        let frames: Vec<Option<Kind>> = [N, D, N, D, N, D, N, D, N, D].map(Some).to_vec();
        assert_eq!(picked(&frames), [[0, 2, 4], [4, 6, 8]]);
        // An uneven cadence (holds of 1 and 3): new frames 0 1 4 6; 4 and 6
        // wait for one more.
        let frames: Vec<Option<Kind>> = [N, N, D, D, N, D, N].map(Some).to_vec();
        assert_eq!(picked(&frames), [[0, 1, 4]]);
    }

    #[test]
    fn no_triple_spans_a_gap() {
        // N N | gap | N N N: the two before the gap make no triple with
        // those after it.
        let frames = vec![Some(N), Some(D), Some(N), None, Some(N), Some(D), Some(N), Some(D), Some(N)];
        assert_eq!(picked(&frames), [[4, 6, 8]]);
    }

    fn plane(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> Vec<f32> {
        (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect()
    }

    #[test]
    fn a_moved_square_gives_the_blocks_it_touches_as_tears() {
        // A bright 32x32 square moved by 16 pixels on a dark 64x64 frame:
        // the real picture has it at x 8..40, the generated one at 24..56,
        // y 16..48 in both.
        let (w, h) = (64, 64);
        let square = |x0: u32| move |x: u32, y: u32| if (x0..x0 + 32).contains(&x) && (16..48).contains(&y) { 200.0 } else { 20.0 };
        let (real, got) = (plane(w, h, square(8)), plane(w, h, square(24)));
        let (r, tears) = region(&got, &real, w, Rect::whole(w, h));
        assert_eq!(r.blocks, 16);
        // Wrong pixels: x 8..24 (square only in real) and 40..56 (only in
        // generated), rows 16..48. Blocks x0 (8..16: half), x16 (16..24:
        // full), x32 (40..48: half), x48 (48..56: half); rows 16 and 32.
        // A full block is off by 180, a half by 90: all over 24.
        let mut at: Vec<(u32, u32)> = tears.iter().map(|t| (t.x, t.y)).collect();
        at.sort();
        assert_eq!(at, [(0, 16), (0, 32), (16, 16), (16, 32), (32, 16), (32, 32), (48, 16), (48, 32)]);
        assert_eq!(r.tear_blocks, 8);
        // The same picture: nothing torn, PSNR infinite.
        let (same, none) = region(&real, &real, w, Rect::whole(w, h));
        assert_eq!((same.tear_blocks, none.len()), (0, 0));
        assert!(same.psnr().is_infinite());
    }

    #[test]
    fn a_block_just_under_the_threshold_is_not_a_tear() {
        let (w, h) = (16, 16);
        let real = plane(w, h, |_, _| 100.0);
        let under = plane(w, h, |_, _| 100.0 + TEAR as f32);
        let over = plane(w, h, |_, _| 101.0 + TEAR as f32);
        assert_eq!(region(&under, &real, w, Rect::whole(w, h)).0.tear_blocks, 0);
        assert_eq!(region(&over, &real, w, Rect::whole(w, h)).0.tear_blocks, 1);
    }

    #[test]
    fn a_hud_rectangle_scores_only_its_pixels_and_partial_blocks_count() {
        let (w, h) = (64, 40);
        let real = plane(w, h, |_, _| 50.0);
        // Wrong only at the bottom-right corner, x 48.., y 32..
        let got = plane(w, h, |x, y| if x >= 48 && y >= 32 { 150.0 } else { 50.0 });
        let whole = region(&got, &real, w, Rect::whole(w, h)).0;
        // 4 x 3 blocks, the bottom row 8 pixels high: the corner block (16x8)
        // is a tear.
        assert_eq!((whole.blocks, whole.tear_blocks, whole.pixels), (12, 1, 64 * 40));
        let hud = region(&got, &real, w, Rect { x: 0, y: 0, w: 32, h: 32 }).0;
        assert_eq!((hud.blocks, hud.tear_blocks, hud.pixels), (4, 0, 32 * 32));
        assert!(hud.psnr().is_infinite());
        // PSNR from the squared error: 128 pixels off by 100.
        let want = 10.0 * (255.0f64 * 255.0 / (128.0 * 100.0 * 100.0 / (64.0 * 40.0))).log10();
        assert!((whole.psnr() - want).abs() < 1e-9);
    }

    #[test]
    fn the_hud_flag_is_a_rectangle_inside_the_frame() {
        assert_eq!(Rect::parse("40,900,320,160", 1920, 1080), Ok(Rect { x: 40, y: 900, w: 320, h: 160 }));
        for bad in ["40,900,320", "a,b,c,d", "0,0,0,10", "1900,0,40,10", "0,1000,10,100", "4294967295,0,2,1"] {
            assert!(Rect::parse(bad, 1920, 1080).is_err(), "{bad}");
        }
    }

    #[test]
    fn luma_is_bt709() {
        assert_eq!(luma(&[255, 255, 255, 255, 0, 0, 0, 255]), [255.0, 0.0]);
        assert!((luma(&[0, 255, 0, 255])[0] - 0.7152 * 255.0).abs() < 1e-3);
    }

    #[test]
    fn the_worst_are_the_most_torn_then_the_lowest_psnr() {
        let s =
            |tears, sq| Scored { frames: [0, 1, 2], picture: Region { sq_err: sq, pixels: 100, blocks: 1, tear_blocks: tears }, hud: None };
        let mut v = [s(1, 10.0), s(3, 1.0), s(3, 50.0), s(0, 99.0)];
        v.sort_by(worse);
        let got: Vec<(u32, f64)> = v.iter().map(|x| (x.picture.tear_blocks, x.picture.sq_err)).collect();
        assert_eq!(got, [(3, 50.0), (3, 1.0), (1, 10.0), (0, 99.0)]);
    }

    /// A YUYV clip of `frames` (Y, U, V per pixel pair, by frame) at
    /// 60 Hz, each picture twice (2:2), at `path`.
    fn clip(path: &Path, (w, h): (u32, u32), frames: &[Vec<u8>]) {
        use crate::clip::{Header, Writer};
        use std::time::Duration;
        let format = crate::capture::Format { width: w, height: h, fps: 60.0, pixels: crate::capture::Pixels::Yuyv };
        let mut writer = Writer::create(path, Header::of(format)).unwrap();
        for (n, f) in frames.iter().enumerate() {
            for d in 0..2u64 {
                let t = Duration::from_micros(n as u64 * 33_333 + d * 16_667);
                writer.frame(t, t, f).unwrap();
            }
        }
        writer.finish().unwrap();
    }

    /// A generator of one fragment pass over `prev` and `next`, as the
    /// branches' copy and blend are (#67).
    struct TestPass(wgpu::RenderPipeline, wgpu::BindGroupLayout);

    impl TestPass {
        fn new(device: &wgpu::Device, body: &str) -> Self {
            let shader = format!(
                "@group(0) @binding(0) var prev: texture_2d<f32>;\n@group(0) @binding(1) var next: texture_2d<f32>;\n\
                 @fragment fn fs(in: VOut) -> @location(0) vec4f {{ let at = vec2u(in.pos.xy); let p = textureLoad(prev, at, 0); let n = textureLoad(next, at, 0); return {body}; }}"
            );
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("score test pass"),
                entries: &[crate::app::texture_entry(0, false), crate::app::texture_entry(1, false)],
            });
            let pipeline = crate::app::pipeline(device, "score test pass", &shader, &layout, wgpu::TextureFormat::Rgba8Unorm);
            Self(pipeline, layout)
        }
    }

    impl Generator for TestPass {
        fn generate(
            &mut self,
            device: &wgpu::Device,
            _: &wgpu::Queue,
            encoder: &mut wgpu::CommandEncoder,
            frames: Pair<'_>,
            _: Option<crate::gputime::PassTimes<'_>>,
        ) -> bool {
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("score test pass"),
                layout: &self.1,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(frames.prev) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(frames.next) },
                ],
            });
            let attachments = [Some(wgpu::RenderPassColorAttachment {
                view: frames.out,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("score test pass"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.0);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
            true
        }
    }

    /// The frames a generator gets are the window's: on a still, coloured
    /// picture whose brightness steps evenly from new frame to new frame,
    /// the mean of N0 and N2 is N1, and `next` alone is N2, within a
    /// level per channel, in every triple of a long chain.
    /// `cargo test --release --features dev -- --ignored score_`.
    #[test]
    #[ignore = "needs a GPU"]
    fn score_hands_generators_the_windows_frames() {
        let (w, h) = (64u32, 32u32);
        let dir = std::env::temp_dir().join(format!("phc-score-colour-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ramp.clip");
        // Warm on the left, cool on the right, in every frame, no channel
        // clipped at 0 or 255; luma 60, 70, 80, ... a new frame (each 10 levels over the last: new by the
        // detector). Twelve new frames: five chained triples.
        let frames: Vec<Vec<u8>> = (0..12u32)
            .map(|k| {
                (0..h)
                    .flat_map(|_| 0..w / 2)
                    .flat_map(|m| {
                        let y = (60 + 10 * k) as u8;
                        let (u, v) = if m < w / 4 { (115, 141) } else { (141, 115) };
                        [y, u, y, v]
                    })
                    .collect()
            })
            .collect();
        clip(&path, (w, h), &frames);
        let opts = Options { clip: path.clone(), mode: Mode::Copy, every: 1, worst: 0, out: None, hud: None };
        for (name, body, want) in [("mean", "mix(p, n, 0.5)", 1u8), ("next", "n", 0)] {
            let gpu = Gpu::new((w, h)).unwrap();
            let generator: Box<dyn Generator> = Box::new(TestPass::new(&gpu.device, body));
            let t = score(&opts, Reader::open(&path).unwrap(), None, gpu, generator).unwrap();
            assert_eq!(t.triples, 5, "{name}");
            if name == "mean" {
                // The mean of N0 and N2 is N1 (rounding: a level).
                assert!(t.max_channel <= want, "{name}: off by {} in a channel", t.max_channel);
            } else {
                // `next` is N2: 10 luma levels over N1, so never N1 itself,
                // and the same picture: about 10 levels a channel, no more.
                assert!((8..=14).contains(&t.max_channel), "{name}: off by {} in a channel", t.max_channel);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `copy` on a synthetic clip, end to end on the GPU: a square moving
    /// 16 pixels a new frame in 2:2. Copy shows N0 for N1, so the moved
    /// square's blocks are tears.
    /// `cargo test --release --features dev -- --ignored score_`.
    #[test]
    #[ignore = "needs a GPU"]
    fn score_copy_on_a_moving_square() {
        use crate::clip::{Header, Writer};
        use std::time::Duration;
        let (w, h) = (128u32, 64u32);
        let dir = std::env::temp_dir().join(format!("phc-score-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("square.clip");
        let format = crate::capture::Format { width: w, height: h, fps: 60.0, pixels: crate::capture::Pixels::Yuyv };
        let mut writer = Writer::create(&path, Header::of(format)).unwrap();
        // YUYV: grey 16 (limited black) with a white 16x16 square at x0.
        let frame = |x0: u32| -> Vec<u8> {
            (0..h)
                .flat_map(|y| (0..w / 2).map(move |m| (m, y)))
                .flat_map(|(m, y)| {
                    let lum = |x: u32| if (x0..x0 + 16).contains(&x) && (24..40).contains(&y) { 235 } else { 16 };
                    [lum(m * 2), 128, lum(m * 2 + 1), 128]
                })
                .collect()
        };
        for n in 0..6u32 {
            let f = frame(8 + n * 16);
            for d in 0..2u64 {
                let t = Duration::from_micros(u64::from(n) * 33_333 + d * 16_667);
                writer.frame(t, t, &f).unwrap();
            }
        }
        writer.finish().unwrap();
        let opts = Options { clip: path.clone(), mode: Mode::Copy, every: 1, worst: 2, out: Some(dir.join("out")), hud: None };
        let t = run(&opts).unwrap();
        // Two chained triples (new frames 0 2 4 and 4 6 8 by clip index).
        // Copy shows N0 for N1: the square is 16 pixels off, its wrong
        // pixels x 8..40 (N0's 8..24 and N1's 24..40, or one further on),
        // rows 24..40, in blocks x 0, 16, 32 by rows 16 and 32: 6 a triple.
        assert_eq!((t.triples, t.cuts, t.picture.tear_blocks, t.max_tears), (2, 0, 12, 6));
        // Each worst triple's three pictures written.
        let pngs = std::fs::read_dir(dir.join("out")).unwrap().count();
        assert_eq!(pngs, 2 * 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
