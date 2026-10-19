//! SCALING: how the RGB frame is scaled into the letterboxed viewport. The
//! presets name the scalers view.sh's player uses, and the filters are
//! libplacebo 7.360's (mpv's gpu-next resolves the names there):
//! - fast: the GPU's bilinear sampler, never widened (mpv's fast profile:
//!   scale and dscale bilinear, correct-downscaling off);
//! - balanced: spline36 up, mitchell down;
//! - high-quality: ewa_lanczossharp up, mitchell down.
//!
//! Each filter is sampled as `pl_filter_sample` does (kernel, blur, window)
//! into a 256-entry table the shader interpolates linearly, as libplacebo's
//! LUTs are. An orthogonal filter (spline36, mitchell) runs as two passes,
//! as mpv's separated scaling does: across the rows into an intermediate
//! texture at the viewport's width and the frame's height, then down the
//! columns into the viewport. One 2-D pass reads (2n)² texels a pixel, two
//! read 4n: a mitchell downscale to a 720p window was 31 ms a frame on an
//! Intel UHD 620 in one pass (#19). The polar filter stays one pass. Downscaling widens the kernel by the ratio (mpv's
//! correct-downscaling, on by default), and a ratio of 1 runs no filter
//! (libplacebo's no-op, within 1e-6), so a 1:1 feed is the frame itself.
//! Widening is bounded: a filter needing more than `MAX_TAPS` texels a side
//! (a downscale past about 1/8, a sliver of a window) falls back to the
//! bilinear sampler instead of millions of fetches per pixel. libplacebo
//! caps its row size too (`max_tex_2d_dim / 4`), far higher.
//!
//! Not ported: mpv's sigmoid-light upscaling and linear-light downscaling
//! (both on by default in view.sh's balanced); the filters run on the
//! gamma-encoded frame. Chroma stays the convert pass's linear interpolation
//! (mpv's cscale is spline36).

/// SCALING, in the order c cycles them. `config` (mpv.conf's own scalers)
/// has no meaning here and reads as balanced (ADR 0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    Balanced,
    Fast,
    HighQuality,
}

impl Scaling {
    pub fn from_settings(v: &str) -> Result<Self, String> {
        Ok(match v {
            "balanced" | "config" => Self::Balanced,
            "fast" => Self::Fast,
            "high-quality" => Self::HighQuality,
            other => return Err(format!("SCALING={other} is not balanced, fast, high-quality or config")),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Fast => "fast",
            Self::HighQuality => "high-quality",
        }
    }

    /// The overlay's c: settings.rows' order, without config.
    pub fn next(self) -> Self {
        match self {
            Self::Balanced => Self::Fast,
            Self::Fast => Self::HighQuality,
            Self::HighQuality => Self::Balanced,
        }
    }
}

/// libplacebo's filter functions (src/filters.c).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Function {
    Spline36,
    /// The B,C cubic family.
    Cubic { b: f64, c: f64 },
    Jinc,
}

/// `JINC_ZERO3`: the third zero of jinc.
const JINC_ZERO3: f64 = 3.238_315_484_166_236;

impl Function {
    /// `pl_filter_function.radius`.
    fn radius(self) -> f64 {
        match self {
            Self::Spline36 => 3.0,
            Self::Cubic { .. } => 2.0,
            // The first zero.
            Self::Jinc => 1.219_669_891_266_504_5,
        }
    }

    /// `pl_filter_function.weight`, for x >= 0.
    fn weight(self, x: f64) -> f64 {
        match self {
            Self::Spline36 => {
                if x < 1.0 {
                    ((13.0 / 11.0 * x - 453.0 / 209.0) * x - 3.0 / 209.0) * x + 1.0
                } else if x < 2.0 {
                    let x = x - 1.0;
                    ((-6.0 / 11.0 * x + 270.0 / 209.0) * x - 156.0 / 209.0) * x
                } else {
                    let x = x - 2.0;
                    ((1.0 / 11.0 * x - 45.0 / 209.0) * x + 26.0 / 209.0) * x
                }
            }
            Self::Cubic { b, c } => {
                let p0 = 6.0 - 2.0 * b;
                let p2 = -18.0 + 12.0 * b + 6.0 * c;
                let p3 = 12.0 - 9.0 * b - 6.0 * c;
                let q0 = 8.0 * b + 24.0 * c;
                let q1 = -12.0 * b - 48.0 * c;
                let q2 = 6.0 * b + 30.0 * c;
                let q3 = -b - 6.0 * c;
                if x < 1.0 { (p0 + x * x * (p2 + x * p3)) / p0 } else { (q0 + x * (q1 + x * (q2 + x * q3))) / p0 }
            }
            Self::Jinc => {
                if x < 1e-8 {
                    return 1.0;
                }
                let x = x * std::f64::consts::PI;
                2.0 * j1(x) / x
            }
        }
    }
}

/// Bessel J1 (Numerical Recipes' `bessj1`, absolute error near 1e-8; the
/// table it feeds is interpolated far more coarsely). libplacebo calls
/// libm's j1, which Windows spells differently (#9).
fn j1(x: f64) -> f64 {
    let ax = x.abs();
    if ax < 8.0 {
        let y = x * x;
        let n = x * (72_362_614_232.0
            + y * (-7_895_059_235.0 + y * (242_396_853.1 + y * (-2_972_611.439 + y * (15_704.482_60 + y * -30.160_366_06)))));
        let d = 144_725_228_442.0 + y * (2_300_535_178.0 + y * (18_583_304.74 + y * (99_447.433_94 + y * (376.999_139_7 + y))));
        n / d
    } else {
        let z = 8.0 / ax;
        let y = z * z;
        let xx = ax - 3.0 * std::f64::consts::FRAC_PI_4;
        let p = 1.0 + y * (0.183_105e-2 + y * (-0.351_639_649_6e-4 + y * (0.245_752_017_4e-5 + y * -0.240_337_019e-6)));
        let q = 0.046_874_999_95 + y * (-0.200_269_087_3e-3 + y * (0.844_919_909_6e-5 + y * (-0.882_289_87e-6 + y * 0.105_787_412e-6)));
        let v = (std::f64::consts::FRAC_2_PI / ax).sqrt() * (xx.cos() * p - z * xx.sin() * q);
        if x < 0.0 { -v } else { v }
    }
}

/// A `pl_filter_config`: what the presets use of it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Config {
    kernel: Function,
    window: Option<Function>,
    /// For a resizable kernel (jinc); else the kernel's own.
    radius: Option<f64>,
    blur: f64,
    polar: bool,
}

const SPLINE36: Config = Config { kernel: Function::Spline36, window: None, radius: None, blur: 1.0, polar: false };
const MITCHELL: Config =
    Config { kernel: Function::Cubic { b: 1.0 / 3.0, c: 1.0 / 3.0 }, window: None, radius: None, blur: 1.0, polar: false };
const EWA_LANCZOSSHARP: Config = Config {
    kernel: Function::Jinc,
    window: Some(Function::Jinc),
    radius: Some(JINC_ZERO3),
    blur: 0.981_250_583_722_370_7,
    polar: true,
};

impl Config {
    /// `pl_filter_radius_bound`.
    fn bound(&self) -> f64 {
        self.radius.unwrap_or(self.kernel.radius()) * self.blur
    }

    /// `pl_filter_sample` (taper 0, clamp 0).
    fn sample(&self, x: f64) -> f64 {
        let r = self.bound();
        let x = x.abs();
        if x > r {
            return 0.0;
        }
        let mut k = self.kernel.weight(x / self.blur);
        if let Some(w) = self.window {
            k *= w.weight(x / r * w.radius());
        }
        k
    }

    /// `filter_cutoffs`: the last zero crossing within the bound, where
    /// the weights stay within `cutoff` of zero beyond it.
    fn cutoff_radius(&self, cutoff: f64) -> f64 {
        let bound = self.bound() as f32;
        let (mut prev, mut fprev) = (0f32, self.sample(0.0) as f32);
        let mut radius = None;
        let step = 1e-2f32;
        let mut x = 0f32;
        let cutoff = cutoff as f32;
        while x < bound + step {
            let fx = self.sample(f64::from(x)) as f32;
            if (fprev > cutoff && fx <= cutoff) || (fprev < -cutoff && fx >= -cutoff) {
                radius = Some((x - fx * (x - prev) / (fx - fprev)).min(bound));
            }
            prev = x;
            fprev = fx;
            x += step;
        }
        f64::from(radius.unwrap_or(bound))
    }
}

/// The most texels a filter reaches on each side of the centre (1 - n ..= n).
pub const MAX_TAPS: i32 = 16;

/// Entries in a filter's table (libplacebo's `SCALER_LUT_SIZE`).
pub const LUT_SIZE: usize = 256;

/// A filter ready for the shader.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub name: &'static str,
    pub polar: bool,
    /// Orthogonal: the bound per axis, the table spanning [0, bound].
    /// Polar: the cutoff radius (both entries), the table spanning it.
    pub radius: [f32; 2],
    /// Texels each side of the centre: offsets 1 - taps ..= taps.
    pub taps: [i32; 2],
    pub lut: [f32; LUT_SIZE],
}

/// The filter that scales `src` into `dst` pixels, or None for the bilinear
/// sampler (fast, and every 1:1 copy).
pub fn filter(scaling: Scaling, src: (u32, u32), dst: (u32, u32)) -> Option<Filter> {
    // sample_src_info: per axis up, down or neither; down wins.
    let rx = f64::from(dst.0) / f64::from(src.0);
    let ry = f64::from(dst.1) / f64::from(src.1);
    let down = rx < 1.0 - 1e-6 || ry < 1.0 - 1e-6;
    let up = rx > 1.0 + 1e-6 || ry > 1.0 + 1e-6;
    let (name, config) = match (scaling, down, up) {
        (Scaling::Fast, _, _) | (_, false, false) => return None,
        (_, true, _) => ("mitchell", MITCHELL),
        (Scaling::Balanced, false, true) => ("spline36", SPLINE36),
        (Scaling::HighQuality, false, true) => ("ewa_lanczossharp", EWA_LANCZOSSHARP),
    };
    // Widening: blur times the inverse ratio when downscaling.
    let widen = |ratio: f64| Config { blur: config.blur * (1.0 / ratio).max(1.0), ..config };
    // Too wide to run per pixel: the sampler instead. Checked on the bound,
    // before the cutoff search walks it.
    let too_wide = |c: Config| c.bound().ceil() > f64::from(MAX_TAPS);
    if too_wide(widen(rx)) || too_wide(widen(ry)) {
        return None;
    }
    if config.polar {
        // pl_shader_sample_polar: one ratio, the smaller; the table over
        // the cutoff radius (SCALER_LUT_CUTOFF).
        let c = widen(rx.min(ry));
        let radius = c.cutoff_radius(1e-3);
        let lut = std::array::from_fn(|i| c.sample(radius * i as f64 / (LUT_SIZE - 1) as f64) as f32);
        let taps = radius.ceil() as i32;
        Some(Filter { name, polar: true, radius: [radius as f32; 2], taps: [taps; 2], lut })
    } else {
        // pl_shader_sample_ortho2: each axis with its own ratio. The
        // table's shape over [0, bound] does not depend on the blur, so
        // one table serves both.
        let (cx, cy) = (widen(rx), widen(ry));
        let lut = std::array::from_fn(|i| cx.sample(cx.bound() * i as f64 / (LUT_SIZE - 1) as f64) as f32);
        // The row size: twice the radius to the last zero (cutoff 0).
        let taps = |c: Config| c.cutoff_radius(0.0).ceil() as i32;
        Some(Filter {
            name,
            polar: false,
            radius: [cx.bound() as f32, cy.bound() as f32],
            taps: [taps(cx), taps(cy)],
            lut,
        })
    }
}

/// Which way a pass of an orthogonal filter runs (ignored for a polar one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Across the rows: the frame into the intermediate texture.
    X = 0,
    /// Down the columns: the intermediate texture into the viewport.
    Y = 1,
}

impl Filter {
    /// The uniform `Scale` in `WGSL` for a pass reading a `src`-sized
    /// texture: the frame, or for `Axis::Y` the intermediate texture.
    pub fn bytes(&self, src: (u32, u32), axis: Axis) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + LUT_SIZE * 4);
        for v in [src.0 as f32, src.1 as f32, self.radius[0], self.radius[1]] {
            b.extend(v.to_ne_bytes());
        }
        for v in self.taps {
            b.extend(v.to_ne_bytes());
        }
        b.extend(u32::from(self.polar).to_ne_bytes());
        b.extend((axis as u32).to_ne_bytes());
        for v in self.lut {
            b.extend(v.to_ne_bytes());
        }
        b
    }
}

/// Bytes in `Filter::bytes`.
pub const UNIFORM_SIZE: u64 = (32 + LUT_SIZE * 4) as u64;

/// The intermediate texture between an orthogonal filter's passes: float,
/// so the first pass's overshoot (negative lobes) and precision carry
/// into the second as they would within one pass.
pub const INTERMEDIATE: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The filter pass: libplacebo's sampling, with texel fetches clamped to
/// the edge and the weights normalised by their sum. A pass of an
/// orthogonal filter reads along its axis only; the other axis is the
/// output's own row or column (taken from the pixel, not the scaled
/// position, so float error never moves it).
pub const WGSL: &str = r#"
struct Scale {
    size: vec2f,
    radius: vec2f,
    taps: vec2i,
    polar: u32,
    axis: u32,
    lut: array<vec4f, 64>,
};

@group(0) @binding(0) var rgb: texture_2d<f32>;
@group(0) @binding(1) var<uniform> sc: Scale;

fn entry(i: u32) -> f32 {
    return sc.lut[i / 4u][i % 4u];
}

// The table at u in [0, 1], interpolated linearly.
fn lut(u: f32) -> f32 {
    let f = clamp(u, 0.0, 1.0) * 255.0;
    let i = u32(floor(f));
    return mix(entry(i), entry(min(i + 1u, 255u)), f - floor(f));
}

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let p = in.uv * sc.size - 0.5;
    let base = floor(p);
    let f = p - base;
    let b = vec2i(base);
    let edge = vec2i(sc.size) - 1;
    var color = vec3f(0.0);
    var wsum = 0.0;
    if (sc.polar == 1u) {
        let n = sc.taps.x;
        for (var y = 1 - n; y <= n; y++) {
            for (var x = 1 - n; x <= n; x++) {
                let d = length(vec2f(f32(x), f32(y)) - f);
                if (d < sc.radius.x) {
                    let w = lut(d / sc.radius.x);
                    wsum += w;
                    color += w * textureLoad(rgb, clamp(b + vec2i(x, y), vec2i(0), edge), 0).rgb;
                }
            }
        }
    } else if (sc.axis == 0u) {
        let row = clamp(i32(floor(in.uv.y * sc.size.y)), 0, edge.y);
        for (var x = 1 - sc.taps.x; x <= sc.taps.x; x++) {
            let w = lut(abs(f32(x) - f.x) / sc.radius.x);
            wsum += w;
            color += w * textureLoad(rgb, vec2i(clamp(b.x + x, 0, edge.x), row), 0).rgb;
        }
    } else {
        let col = clamp(i32(floor(in.uv.x * sc.size.x)), 0, edge.x);
        for (var y = 1 - sc.taps.y; y <= sc.taps.y; y++) {
            let w = lut(abs(f32(y) - f.y) / sc.radius.y);
            wsum += w;
            color += w * textureLoad(rgb, vec2i(col, clamp(b.y + y, 0, edge.y)), 0).rgb;
        }
    }
    return vec4f(color / wsum, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn j1_and_jinc() {
        assert!((j1(1.0) - 0.440_050_585_744_933_5).abs() < 1e-7);
        assert!((j1(10.0) - 0.043_472_746_168_861_44).abs() < 1e-7);
        assert!(j1(3.831_705_970_207_512).abs() < 1e-7, "J1's first zero");
        // jinc's zeros: libplacebo's radius and JINC_ZERO3.
        assert!(Function::Jinc.weight(Function::Jinc.radius()).abs() < 1e-7);
        assert!(Function::Jinc.weight(JINC_ZERO3).abs() < 1e-7);
        assert_eq!(Function::Jinc.weight(0.0), 1.0);
    }

    #[test]
    fn kernels_as_libplacebo_defines_them() {
        let s = Function::Spline36;
        assert_eq!(s.weight(0.0), 1.0);
        for z in [1.0, 2.0, 3.0] {
            assert!(s.weight(z).abs() < 1e-12, "spline36 is zero at {z}");
        }
        // Continuous at the joints.
        assert!((s.weight(1.0 - 1e-9) - s.weight(1.0)).abs() < 1e-6);
        assert!((s.weight(2.0 - 1e-9) - s.weight(2.0)).abs() < 1e-6);
        let m = MITCHELL.kernel;
        assert_eq!(m.weight(0.0), 1.0, "libplacebo's cubic is scaled to 1 at 0");
        // The unscaled Mitchell-Netravali at 1 is 1/18 of 16/18 at 0.
        assert!((m.weight(1.0) - 1.0 / 16.0).abs() < 1e-12);
        assert!(m.weight(2.0).abs() < 1e-12);
    }

    #[test]
    fn ewa_lanczossharp_is_windowed_and_blurred() {
        let c = EWA_LANCZOSSHARP;
        assert!((c.bound() - JINC_ZERO3 * 0.981_250_583_722_370_7).abs() < 1e-12);
        // The window's first zero is stretched to the bound.
        assert!(c.sample(c.bound()).abs() < 1e-6);
        assert_eq!(c.sample(c.bound() + 0.01), 0.0);
        // SCALER_LUT_CUTOFF trims the tail: where the weight falls through
        // 1e-3, libplacebo's secant step extrapolates that 0.01 segment to
        // zero (3.099 here, the bound being 3.178).
        let r = c.cutoff_radius(1e-3);
        assert!(r < c.bound() && c.sample(r) < 1e-3 && c.sample(r - 0.08) > 1e-3, "{r}");
    }

    #[test]
    fn presets_pick_the_scaler_per_direction() {
        let at = |s, dst| filter(s, (1920, 1080), dst).map(|f| f.name);
        assert_eq!(at(Scaling::Balanced, (1920, 1080)), None, "1:1 runs no filter");
        assert_eq!(at(Scaling::HighQuality, (1920, 1080)), None);
        assert_eq!(at(Scaling::Fast, (2560, 1440)), None);
        assert_eq!(at(Scaling::Balanced, (2560, 1440)), Some("spline36"));
        assert_eq!(at(Scaling::HighQuality, (2560, 1440)), Some("ewa_lanczossharp"));
        assert_eq!(at(Scaling::Balanced, (1280, 720)), Some("mitchell"));
        assert_eq!(at(Scaling::HighQuality, (1280, 720)), Some("mitchell"));
        assert_eq!(Scaling::from_settings("config"), Ok(Scaling::Balanced));
    }

    #[test]
    fn taps_and_widening() {
        let up = filter(Scaling::Balanced, (1920, 1080), (2560, 1440)).unwrap();
        assert_eq!(up.taps, [3, 3], "spline36: 6 taps a side pair");
        assert_eq!(up.radius, [3.0, 3.0]);
        // Downscaling to two thirds: mitchell's radius 2 widened to 3.
        let down = filter(Scaling::Balanced, (1920, 1080), (1280, 720)).unwrap();
        assert!((down.radius[0] - 3.0).abs() < 1e-5);
        assert_eq!(down.taps, [3, 3]);
        let ewa = filter(Scaling::HighQuality, (1920, 1080), (2560, 1440)).unwrap();
        assert!(ewa.polar);
        assert_eq!(ewa.taps, [4, 4]);
    }

    #[test]
    fn widening_is_bounded() {
        // mitchell's radius 2 widened 8x is 16 taps: still the filter.
        let f = filter(Scaling::Balanced, (1920, 1080), (240, 135)).unwrap();
        assert_eq!(f.taps, [16, 16]);
        assert!(f.taps.iter().all(|&t| t <= MAX_TAPS));
        // Past that, down to a 1x1 window: the sampler.
        for dst in [(239, 134), (100, 56), (1, 1)] {
            assert_eq!(filter(Scaling::Balanced, (1920, 1080), dst), None, "{dst:?}");
            assert_eq!(filter(Scaling::HighQuality, (1920, 1080), dst), None, "{dst:?}");
        }
    }

    #[test]
    fn the_table_interpolates_closely() {
        // The worst gap between the table read linearly and the filter.
        for f in [
            filter(Scaling::Balanced, (1920, 1080), (2560, 1440)).unwrap(),
            filter(Scaling::Balanced, (1920, 1080), (1280, 720)).unwrap(),
            filter(Scaling::HighQuality, (1920, 1080), (2560, 1440)).unwrap(),
        ] {
            let c = match f.name {
                "spline36" => SPLINE36,
                "mitchell" => Config { blur: 1.5, ..MITCHELL },
                _ => EWA_LANCZOSSHARP,
            };
            let span = f64::from(f.radius[0]);
            let mut worst = 0f64;
            for k in 0..=10_000 {
                let u = k as f64 / 10_000.0;
                let fi = u * 255.0;
                let i = fi.floor() as usize;
                let j = (i + 1).min(255);
                let t = fi - fi.floor();
                let read = f64::from(f.lut[i]) * (1.0 - t) + f64::from(f.lut[j]) * t;
                worst = worst.max((read - c.sample(u * span)).abs());
            }
            assert!(worst < 2e-4, "{}: {worst}", f.name);
        }
    }

    #[test]
    fn uniform_layout() {
        let f = filter(Scaling::HighQuality, (1920, 1080), (2560, 1440)).unwrap();
        let b = f.bytes((1920, 1080), Axis::X);
        assert_eq!(b.len() as u64, UNIFORM_SIZE);
        assert_eq!(f32::from_ne_bytes(b[0..4].try_into().unwrap()), 1920.0);
        assert_eq!(u32::from_ne_bytes(b[24..28].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(b[28..32].try_into().unwrap()), 0);
        assert_eq!(f32::from_ne_bytes(b[32..36].try_into().unwrap()), f.lut[0]);
        let b = f.bytes((1280, 1080), Axis::Y);
        assert_eq!(u32::from_ne_bytes(b[28..32].try_into().unwrap()), 1);
    }

    /// The filter shader on the GPU, against the filters sampled exactly
    /// (no table), and each filter's GPU time:
    /// `cargo test --release -- --ignored --nocapture gpu_filters`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_filters_match_the_filters_and_are_timed() {
        use crate::app::{BACKENDS, pipeline, texture_entry, uniform_entry};
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = BACKENDS;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("a GPU");
        let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() }))
                .unwrap();
        eprintln!("GPU: {}", adapter.get_info().name);

        // Noise in red, sharp-edged blocks in green (ringing), a ramp in blue.
        let (w, h) = (1920u32, 1080u32);
        let src: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let noise = (i.wrapping_mul(2_654_435_761) >> 24) as u8;
                let block = if (x / 37 + y / 41) % 2 == 0 { 230 } else { 20 };
                [noise, block, (x * 255 / (w - 1)) as u8, 255]
            })
            .collect();
        let texel = |x: i32, y: i32| {
            let (x, y) = (x.clamp(0, w as i32 - 1) as usize, y.clamp(0, h as i32 - 1) as usize);
            let o = (y * w as usize + x) * 4;
            [0, 1, 2].map(|c| f64::from(src[o + c]) / 255.0)
        };
        let texture = |size: (u32, u32), usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        let rgb = texture((w, h), wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        queue.write_texture(
            rgb.as_image_copy(),
            &src,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let rgb_view = rgb.create_view(&Default::default());
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[texture_entry(0, false), uniform_entry(1)],
        });
        let pipe = pipeline(&device, "filter", WGSL, &layout, wgpu::TextureFormat::Rgba8Unorm);
        let pipe_x = pipeline(&device, "filter x", WGSL, &layout, INTERMEDIATE);
        let read = |buffer: &wgpu::Buffer| -> Vec<u8> {
            buffer.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let v = buffer.get_mapped_range(..).unwrap().to_vec();
            buffer.unmap();
            v
        };

        for (scaling, dst) in [
            (Scaling::Balanced, (2560, 1440)),
            (Scaling::HighQuality, (2560, 1440)),
            (Scaling::Balanced, (1280, 720)),
            (Scaling::Balanced, (950, 534)),
            (Scaling::Balanced, (240, 135)),
        ] {
            let f = filter(scaling, (w, h), dst).unwrap();
            // One pass for the polar filter; across, then down, for the others.
            let bind = |view: &wgpu::TextureView, bytes: Vec<u8>| {
                let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: UNIFORM_SIZE,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&uniform, 0, &bytes);
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                        wgpu::BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                    ],
                })
            };
            let target = texture(dst, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
            let target_view = target.create_view(&Default::default());
            let intermediate = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: dst.0, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: INTERMEDIATE,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let intermediate_view = intermediate.create_view(&Default::default());
            let passes: Vec<(&wgpu::RenderPipeline, wgpu::BindGroup, &wgpu::TextureView)> = if f.polar {
                vec![(&pipe, bind(&rgb_view, f.bytes((w, h), Axis::X)), &target_view)]
            } else {
                vec![
                    (&pipe_x, bind(&rgb_view, f.bytes((w, h), Axis::X)), &intermediate_view),
                    (&pipe, bind(&intermediate_view, f.bytes((dst.0, h), Axis::Y)), &target_view),
                ]
            };
            const RUNS: u32 = 120;
            let queries = (!required_features.is_empty()).then(|| {
                device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count: RUNS * 2 })
            });
            let row = (dst.0 * 4).next_multiple_of(256);
            let out = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(row * dst.1),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let times_size = u64::from(RUNS * 2 * 8);
            let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: times_size,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let times = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: times_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device.create_command_encoder(&Default::default());
            for run in 0..RUNS {
                let last = passes.len() - 1;
                for (i, (pipe, bind, view)) in passes.iter().enumerate() {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: None,
                        // From the first pass's start to the last one's end.
                        timestamp_writes: queries.as_ref().map(|q| wgpu::RenderPassTimestampWrites {
                            query_set: q,
                            beginning_of_pass_write_index: (i == 0).then_some(run * 2),
                            end_of_pass_write_index: (i == last).then_some(run * 2 + 1),
                        }),
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(pipe);
                    pass.set_bind_group(0, bind, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
            if let Some(q) = &queries {
                encoder.resolve_query_set(q, 0..RUNS * 2, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &times, 0, times_size);
            }
            encoder.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &out,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(dst.1) },
                },
                wgpu::Extent3d { width: dst.0, height: dst.1, depth_or_array_layers: 1 },
            );
            queue.submit([encoder.finish()]);
            let pixels = read(&out);

            // The same sampling on the CPU, with the filter itself.
            let base = match f.name {
                "spline36" => SPLINE36,
                "mitchell" => MITCHELL,
                _ => EWA_LANCZOSSHARP,
            };
            let (rx, ry) = (f64::from(dst.0) / f64::from(w), f64::from(dst.1) / f64::from(h));
            let widen = |r: f64| Config { blur: base.blur * (1.0 / r).max(1.0), ..base };
            let mut worst = 0f64;
            for j in (0..dst.1).step_by(13) {
                for i in (0..dst.0).step_by(17) {
                    let p = [
                        (f64::from(i) + 0.5) / f64::from(dst.0) * f64::from(w) - 0.5,
                        (f64::from(j) + 0.5) / f64::from(dst.1) * f64::from(h) - 0.5,
                    ];
                    let b = p.map(f64::floor);
                    let fr = [p[0] - b[0], p[1] - b[1]];
                    let (mut sum, mut wsum) = ([0f64; 3], 0f64);
                    let (nx, ny) = (f.taps[0], f.taps[1]);
                    for y in 1 - ny..=ny {
                        for x in 1 - nx..=nx {
                            let wt = if f.polar {
                                let d = (f64::from(x) - fr[0]).hypot(f64::from(y) - fr[1]);
                                if d >= f64::from(f.radius[0]) {
                                    continue;
                                }
                                widen(rx.min(ry)).sample(d)
                            } else {
                                widen(rx).sample(f64::from(x) - fr[0]) * widen(ry).sample(f64::from(y) - fr[1])
                            };
                            let t = texel(b[0] as i32 + x, b[1] as i32 + y);
                            for c in 0..3 {
                                sum[c] += wt * t[c];
                            }
                            wsum += wt;
                        }
                    }
                    let o = (j * row + i * 4) as usize;
                    for c in 0..3 {
                        let want = (sum[c] / wsum).clamp(0.0, 1.0) * 255.0;
                        worst = worst.max((f64::from(pixels[o + c]) - want).abs());
                    }
                }
            }
            let gpu = if queries.is_some() {
                let t = read(&times);
                let ticks: Vec<u64> = t.as_chunks::<8>().0.iter().map(|c| u64::from_ne_bytes(*c)).collect();
                let period = f64::from(queue.get_timestamp_period());
                let mut ms: Vec<f64> =
                    (0..RUNS as usize).map(|r| (ticks[r * 2 + 1] - ticks[r * 2]) as f64 * period / 1e6).collect();
                ms.sort_by(f64::total_cmp);
                format!("GPU median {:.3} ms, max {:.3} ms", ms[ms.len() / 2], ms[ms.len() - 1])
            } else {
                "no timestamps".into()
            };
            eprintln!("{} {w}x{h} -> {}x{} ({}): worst {worst:.2} of 255; {gpu}", scaling.as_str(), dst.0, dst.1, f.name);
            assert!(worst <= 1.0, "{}: the shader is {worst:.2} levels off the filter", f.name);
        }
    }

    #[test]
    fn scaling_values_match_lib_settings_sh() {
        let rows = crate::settings::rows();
        let values = rows.iter().find(|r| r.name == "SCALING").unwrap().values.clone();
        let order = [Scaling::Balanced, Scaling::Balanced.next(), Scaling::Balanced.next().next()].map(Scaling::as_str);
        assert_eq!(values, format!("enum:{},config", order.join(",")));
        assert_eq!(Scaling::Balanced.next().next().next(), Scaling::Balanced);
    }
}
