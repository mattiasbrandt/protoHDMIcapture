//! UPSCALING=nis (#71, #85): NVIDIA Image Scaling 1.0.3's NVScaler,
//! its vendor-neutral fp32 path (NIS_Scaler.h, NIS_Config.h), ported to a
//! WGSL fragment pass. A 6-tap scaling filter is blended with four
//! directional filters by an edge map, and an unsharp mask limited by the
//! local contrast sharpens the luma; chroma is the bilinear tap, shifted by
//! the luma's change (NIS_HDR_MODE none: the frame is display-referred SDR).
//!
//! The original is a compute shader that loads a block's luma into group
//! memory; here each output pixel reads its own 6×6 luma (clamped to the
//! edge, as NIS's linear-clamp sampler is), so the arithmetic per pixel is
//! NIS's and the pass slots into the scale pass like the other scalers.
//! The coefficient tables are NIS's (`coef_scale`, `coef_usm`), the first
//! six taps of each phase (the shader's kFilterSize; the last two are zero),
//! in the pass's uniform. The constants are `NVScalerUpdateConfig`'s.
//!
//! NVScaler is defined for 1x to 2x a side (`NVScalerUpdateConfig` refuses
//! a kScale outside [0.5, 1]): UPSCALING=nis uses it there; past 2x and
//! for a downscale the resize filter (SCALING, scale.rs) scales, and 1:1
//! is the frame.
//!
//! Ported from https://github.com/NVIDIAGameWorks/NVIDIAImageScaling under
//! its licence:
//!
//! The MIT License(MIT)
//!
//! Copyright(c) 2022 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy of
//! this software and associated documentation files(the "Software"), to deal in
//! the Software without restriction, including without limitation the rights to
//! use, copy, modify, merge, publish, distribute, sublicense, and / or sell copies of
//! the Software, and to permit persons to whom the Software is furnished to do so,
//! subject to the following conditions :
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
//! FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.IN NO EVENT SHALL THE AUTHORS OR
//! COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
//! IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
//! CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

/// NVScaler's sharpness slider, 0 to 1: 0.5 is its middle, where neither
/// of `NVScalerUpdateConfig`'s two ranges stretches it.
pub const SHARPNESS: f32 = 0.5;

/// The constants `NVScalerUpdateConfig` sets (SDR), for one sharpness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Constants {
    pub detect_ratio: f32,
    pub detect_thres: f32,
    pub min_contrast_ratio: f32,
    pub ratio_norm: f32,
    pub contrast_boost: f32,
    pub eps: f32,
    pub sharp_start_y: f32,
    pub sharp_scale_y: f32,
    pub sharp_strength_min: f32,
    pub sharp_strength_scale: f32,
    pub sharp_limit_min: f32,
    pub sharp_limit_scale: f32,
}

impl Constants {
    /// `NVScalerUpdateConfig`, NISHDRMode::None.
    pub fn new(sharpness: f32) -> Self {
        let sharpen_slider = sharpness.clamp(0.0, 1.0) - 0.5;
        // Different ranges below and above the middle (NIS's comment: 0 is
        // no sharpening, 1 not too much).
        let (max_scale, min_scale, limit_scale) = if sharpen_slider >= 0.0 { (1.25, 1.25, 1.25) } else { (1.75, 1.0, 1.0) };
        let (min_contrast_ratio, max_contrast_ratio) = (2.0f32, 10.0f32);
        let (sharp_start_y, sharp_end_y) = (0.45f32, 0.9f32);
        let sharp_strength_min = (0.4 + sharpen_slider * min_scale * 1.2f32).max(0.0);
        let sharp_strength_max = 1.6 + sharpen_slider * max_scale * 1.8f32;
        let sharp_limit_min = (0.14 + sharpen_slider * limit_scale * 0.32f32).max(0.1);
        let sharp_limit_max = 0.5 + sharpen_slider * limit_scale * 0.6f32;
        Self {
            detect_ratio: 2.0 * 1127.0 / 1024.0,
            detect_thres: 64.0 / 1024.0,
            min_contrast_ratio,
            ratio_norm: 1.0 / (max_contrast_ratio - min_contrast_ratio),
            contrast_boost: 1.0,
            eps: 1.0 / 255.0,
            sharp_start_y,
            sharp_scale_y: 1.0 / (sharp_end_y - sharp_start_y),
            sharp_strength_min,
            sharp_strength_scale: sharp_strength_max - sharp_strength_min,
            sharp_limit_min,
            sharp_limit_scale: sharp_limit_max - sharp_limit_min,
        }
    }
}

/// The upscale from `src` into `dst` pixels if NVScaler covers it: 1x to
/// 2x on each side, and not 1:1 on both. None: the caller's other scaler.
pub fn nis(src: (u32, u32), dst: (u32, u32)) -> Option<Nis> {
    // kScale: input over output, as NVScalerUpdateConfig has it.
    let scale = [src.0 as f32 / dst.0 as f32, src.1 as f32 / dst.1 as f32];
    let covered = |s: f32| (0.5 - 1e-6..=1.0 + 1e-6).contains(&s);
    let one = |s: f32| (s - 1.0).abs() <= 1e-6;
    if !scale.iter().all(|&s| covered(s)) || scale.iter().all(|&s| one(s)) {
        return None;
    }
    Some(Nis { src, scale, constants: Constants::new(SHARPNESS) })
}

/// NVScaler, ready for the shader.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nis {
    /// The frame's size, which the pass reads.
    pub src: (u32, u32),
    /// kScaleX, kScaleY: input over output, in f32 as NVScalerUpdateConfig
    /// computes them. The pass takes the source position as NIS does,
    /// `(dstX + 0.5) * kScaleX - 0.5` from the pixel's own coordinate: at a
    /// ratio like 4:3 its fraction lands exactly on a phase (24 of 64), and
    /// a position taken from the interpolated uv falls a hair short and
    /// picks the phase before.
    pub scale: [f32; 2],
    pub constants: Constants,
}

/// Bytes in `Nis::bytes`: 20 floats, then the two tables at two vec4s a
/// phase.
pub const UNIFORM_SIZE: u64 = (20 * 4 + 2 * 64 * 2 * 16) as u64;

impl Nis {
    /// The uniform `Nis` in `WGSL`, for a viewport whose top-left pixel is
    /// `origin` on the target.
    pub fn bytes(&self, origin: (u32, u32)) -> Vec<u8> {
        let c = &self.constants;
        let mut b = Vec::with_capacity(UNIFORM_SIZE as usize);
        for v in [
            self.src.0 as f32,
            self.src.1 as f32,
            self.scale[0],
            self.scale[1],
            origin.0 as f32,
            origin.1 as f32,
            0.0,
            0.0,
            c.detect_ratio,
            c.detect_thres,
            c.min_contrast_ratio,
            c.ratio_norm,
            c.contrast_boost,
            c.eps,
            c.sharp_start_y,
            c.sharp_scale_y,
            c.sharp_strength_min,
            c.sharp_strength_scale,
            c.sharp_limit_min,
            c.sharp_limit_scale,
        ] {
            b.extend(v.to_ne_bytes());
        }
        for table in [&COEF_SCALE, &COEF_USM] {
            for phase in table {
                for v in phase.iter().chain(&[0.0, 0.0]) {
                    b.extend(v.to_ne_bytes());
                }
            }
        }
        b
    }
}

/// The pass: NVScaler per output pixel (NIS_Scaler.h's `NVScaler`, with
/// `GetEdgeMap`, `CalcLTI`, `EvalPoly6`, `FilterNormal`, `AddDirFilters`).
/// Its 6×6 window `P` starts two texels up and left of the source
/// position's floor; edge map (i, j) is the 3×3 around floor + (j, i).
pub const WGSL: &str = r#"
struct Nis {
    size: vec2f,
    scale: vec2f,
    origin: vec2f,
    unused: vec2f,
    detect_ratio: f32,
    detect_thres: f32,
    min_contrast_ratio: f32,
    ratio_norm: f32,
    contrast_boost: f32,
    eps: f32,
    sharp_start_y: f32,
    sharp_scale_y: f32,
    sharp_strength_min: f32,
    sharp_strength_scale: f32,
    sharp_limit_min: f32,
    sharp_limit_scale: f32,
    coef_scale: array<vec4f, 128>,
    coef_usm: array<vec4f, 128>,
};

@group(0) @binding(0) var rgb: texture_2d<f32>;
@group(0) @binding(1) var<uniform> nis: Nis;

const PHASES: i32 = 64;

// The 6×6 luma window, rows then columns.
var<private> P: array<array<f32, 6>, 6>;

fn texel(p: vec2i) -> vec3f {
    return textureLoad(rgb, clamp(p, vec2i(0), vec2i(nis.size) - 1), 0).rgb;
}

fn get_y(c: vec3f) -> f32 {
    return 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
}

fn coef_scale(phase: i32, i: i32) -> f32 {
    let v = nis.coef_scale[clamp(phase, 0, PHASES - 1) * 2 + i / 4];
    return v[i % 4];
}

fn coef_usm(phase: i32, i: i32) -> f32 {
    let v = nis.coef_usm[clamp(phase, 0, PHASES - 1) * 2 + i / 4];
    return v[i % 4];
}

fn edge_map(i: i32, j: i32) -> vec4f {
    let r = 1 + i;
    let c = 1 + j;
    let g_0 = abs(P[r][c] + P[r][c + 1] + P[r][c + 2] - P[r + 2][c] - P[r + 2][c + 1] - P[r + 2][c + 2]);
    let g_45 = abs(P[r + 1][c] + P[r][c] + P[r][c + 1] - P[r + 2][c + 1] - P[r + 2][c + 2] - P[r + 1][c + 2]);
    let g_90 = abs(P[r][c] + P[r + 1][c] + P[r + 2][c] - P[r][c + 2] - P[r + 1][c + 2] - P[r + 2][c + 2]);
    let g_135 = abs(P[r + 1][c] + P[r + 2][c] + P[r + 2][c + 1] - P[r][c + 1] - P[r][c + 2] - P[r + 1][c + 2]);

    let g_0_90_max = max(g_0, g_90);
    let g_0_90_min = min(g_0, g_90);
    let g_45_135_max = max(g_45, g_135);
    let g_45_135_min = min(g_45, g_135);

    if (g_0_90_max + g_45_135_max == 0.0) {
        return vec4f(0.0);
    }

    let e_0_90 = min(g_0_90_max / (g_0_90_max + g_45_135_max), 1.0);
    let e_45_135 = 1.0 - e_0_90;

    let c_0_90 = (g_0_90_max > (g_0_90_min * nis.detect_ratio)) && (g_0_90_max > nis.detect_thres) && (g_0_90_max > g_45_135_min);
    let c_45_135 = (g_45_135_max > (g_45_135_min * nis.detect_ratio)) && (g_45_135_max > nis.detect_thres) && (g_45_135_max > g_0_90_min);
    let c_g_0_90 = g_0_90_max == g_0;
    let c_g_45_135 = g_45_135_max == g_45;

    let f_e_0_90 = select(1.0, e_0_90, c_0_90 && c_45_135);
    let f_e_45_135 = select(1.0, e_45_135, c_0_90 && c_45_135);

    let weight_0 = select(0.0, f_e_0_90, c_0_90 && c_g_0_90);
    let weight_90 = select(0.0, f_e_0_90, c_0_90 && !c_g_0_90);
    let weight_45 = select(0.0, f_e_45_135, c_45_135 && c_g_45_135);
    let weight_135 = select(0.0, f_e_45_135, c_45_135 && !c_g_45_135);

    return vec4f(weight_0, weight_90, weight_45, weight_135);
}

fn calc_lti(p0: f32, p1: f32, p2: f32, p3: f32, p4: f32, p5: f32, phase: i32) -> f32 {
    let selector = phase <= PHASES / 2;
    var sel = select(p3, p0, selector);
    let a_min = min(min(p1, p2), sel);
    let a_max = max(max(p1, p2), sel);
    sel = select(p5, p2, selector);
    let b_min = min(min(p3, p4), sel);
    let b_max = max(max(p3, p4), sel);

    let a_cont = a_max - a_min;
    let b_cont = b_max - b_min;

    let cont_ratio = max(a_cont, b_cont) / (min(a_cont, b_cont) + nis.eps);
    return (1.0 - saturate((cont_ratio - nis.min_contrast_ratio) * nis.ratio_norm)) * nis.contrast_boost;
}

fn eval_poly6(pixels: array<f32, 6>, phase: i32) -> f32 {
    var pxl = pixels;
    var y = 0.0;
    for (var i = 0; i < 6; i++) {
        y += coef_scale(phase, i) * pxl[i];
    }
    var y_usm = 0.0;
    for (var i = 0; i < 6; i++) {
        y_usm += coef_usm(phase, i) * pxl[i];
    }

    // A piece-wise ramp on the luma: sharpen and limit by it.
    let y_scale = 1.0 - saturate((y - nis.sharp_start_y) * nis.sharp_scale_y);
    let y_sharpness = y_scale * nis.sharp_strength_scale + nis.sharp_strength_min;
    y_usm *= y_sharpness;
    let y_sharpness_limit = (y_scale * nis.sharp_limit_scale + nis.sharp_limit_min) * y;
    y_usm = min(y_sharpness_limit, max(-y_sharpness_limit, y_usm));
    // Less ringing.
    y_usm *= calc_lti(pxl[0], pxl[1], pxl[2], pxl[3], pxl[4], pxl[5], phase);

    return y + y_usm;
}

fn filter_normal(phase_x: i32, phase_y: i32) -> f32 {
    var h_acc = 0.0;
    for (var j = 0; j < 6; j++) {
        var v_acc = 0.0;
        for (var i = 0; i < 6; i++) {
            v_acc += P[i][j] * coef_scale(phase_y, i);
        }
        h_acc += v_acc * coef_scale(phase_x, j);
    }
    return h_acc;
}

fn add_dir_filters(fx: f32, fy: f32, phase_x: i32, phase_y: i32, w: vec4f) -> f32 {
    var f = 0.0;
    if (w.x > 0.0) {
        // 0 degrees.
        var interp: array<f32, 6>;
        for (var i = 0; i < 6; i++) {
            interp[i] = mix(P[i][2], P[i][3], fx);
        }
        f += eval_poly6(interp, phase_y) * w.x;
    }
    if (w.y > 0.0) {
        // 90 degrees.
        var interp: array<f32, 6>;
        for (var i = 0; i < 6; i++) {
            interp[i] = mix(P[2][i], P[3][i], fy);
        }
        f += eval_poly6(interp, phase_x) * w.y;
    }
    if (w.z > 0.0) {
        // 45 degrees.
        var pphase_b45 = 0.5 + 0.5 * (fx - fy);
        var temp: array<f32, 7>;
        temp[1] = mix(P[2][1], P[1][2], pphase_b45);
        temp[3] = mix(P[3][2], P[2][3], pphase_b45);
        temp[5] = mix(P[4][3], P[3][4], pphase_b45);
        pphase_b45 = pphase_b45 - 0.5;
        let after = pphase_b45 >= 0.0;
        let a = select(P[2][0], P[0][2], after);
        let b = select(P[3][1], P[1][3], after);
        let c = select(P[4][2], P[2][4], after);
        let d = select(P[5][3], P[3][5], after);
        temp[0] = mix(P[1][1], a, abs(pphase_b45));
        temp[2] = mix(P[2][2], b, abs(pphase_b45));
        temp[4] = mix(P[3][3], c, abs(pphase_b45));
        temp[6] = mix(P[4][4], d, abs(pphase_b45));

        var interp: array<f32, 6>;
        var pphase_p45 = fx + fy;
        let shift = select(0, 1, pphase_p45 >= 1.0);
        for (var i = 0; i < 6; i++) {
            interp[i] = temp[i + shift];
        }
        pphase_p45 = pphase_p45 - f32(shift);
        f += eval_poly6(interp, i32(pphase_p45 * 64.0)) * w.z;
    }
    if (w.w > 0.0) {
        // 135 degrees.
        var pphase_b135 = 0.5 * (fx + fy);
        var temp: array<f32, 7>;
        temp[1] = mix(P[3][1], P[4][2], pphase_b135);
        temp[3] = mix(P[2][2], P[3][3], pphase_b135);
        temp[5] = mix(P[1][3], P[2][4], pphase_b135);
        pphase_b135 = pphase_b135 - 0.5;
        let after = pphase_b135 >= 0.0;
        let a = select(P[3][0], P[5][2], after);
        let b = select(P[2][1], P[4][3], after);
        let c = select(P[1][2], P[3][4], after);
        let d = select(P[0][3], P[2][5], after);
        temp[0] = mix(P[4][1], a, abs(pphase_b135));
        temp[2] = mix(P[3][2], b, abs(pphase_b135));
        temp[4] = mix(P[2][3], c, abs(pphase_b135));
        temp[6] = mix(P[1][4], d, abs(pphase_b135));

        var interp: array<f32, 6>;
        var pphase_p135 = 1.0 + (fx - fy);
        let shift = select(0, 1, pphase_p135 >= 1.0);
        for (var i = 0; i < 6; i++) {
            interp[i] = temp[i + shift];
        }
        pphase_p135 = pphase_p135 - f32(shift);
        f += eval_poly6(interp, i32(pphase_p135 * 64.0)) * w.w;
    }
    return f;
}

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    // srcX = (0.5 + dstX) * kScaleX - 0.5: the pixel's centre on the target
    // is the viewport's origin + dstX + 0.5.
    let src = (in.pos.xy - nis.origin) * nis.scale - 0.5;
    let fl = floor(src);
    let fr = src - fl;
    let b = vec2i(fl);
    for (var i = 0; i < 6; i++) {
        for (var j = 0; j < 6; j++) {
            P[i][j] = get_y(texel(b + vec2i(j - 2, i - 2)));
        }
    }
    let phase_x = i32(fr.x * f32(PHASES));
    let phase_y = i32(fr.y * f32(PHASES));

    // The directional filters' weights, between the four edge maps around.
    let w = mix(mix(edge_map(0, 0), edge_map(0, 1), fr.x), mix(edge_map(1, 0), edge_map(1, 1), fr.x), fr.y);
    let base_weight = 1.0 - w.x - w.y - w.z - w.w;
    var op_y = filter_normal(phase_x, phase_y) * base_weight;
    op_y += add_dir_filters(fr.x, fr.y, phase_x, phase_y, w);

    // Chroma: the bilinear tap (NIS's linear-clamp sample at the source
    // position), its luma replaced.
    let c = mix(mix(texel(b), texel(b + vec2i(1, 0)), fr.x), mix(texel(b + vec2i(0, 1)), texel(b + vec2i(1, 1)), fr.x), fr.y);
    return vec4f(c + (op_y - get_y(c)), 1.0);
}
"#;

/// NIS_Config.h's `coef_scale`: the scaling filter, 64 phases of 6 taps.
pub(crate) const COEF_SCALE: [[f32; 6]; 64] = [
    [0.0, 0.0, 1.0000, 0.0, 0.0, 0.0],
    [0.0029, -0.0127, 1.0000, 0.0132, -0.0034, 0.0],
    [0.0063, -0.0249, 0.9985, 0.0269, -0.0068, 0.0],
    [0.0088, -0.0361, 0.9956, 0.0415, -0.0103, 0.0005],
    [0.0117, -0.0474, 0.9932, 0.0562, -0.0142, 0.0005],
    [0.0142, -0.0576, 0.9897, 0.0713, -0.0181, 0.0005],
    [0.0166, -0.0674, 0.9844, 0.0874, -0.0220, 0.0010],
    [0.0186, -0.0762, 0.9785, 0.1040, -0.0264, 0.0015],
    [0.0205, -0.0850, 0.9727, 0.1206, -0.0308, 0.0020],
    [0.0225, -0.0928, 0.9648, 0.1382, -0.0352, 0.0024],
    [0.0239, -0.1006, 0.9575, 0.1558, -0.0396, 0.0029],
    [0.0254, -0.1074, 0.9487, 0.1738, -0.0439, 0.0034],
    [0.0264, -0.1138, 0.9390, 0.1929, -0.0488, 0.0044],
    [0.0278, -0.1191, 0.9282, 0.2119, -0.0537, 0.0049],
    [0.0288, -0.1245, 0.9170, 0.2310, -0.0581, 0.0059],
    [0.0293, -0.1294, 0.9058, 0.2510, -0.0630, 0.0063],
    [0.0303, -0.1333, 0.8926, 0.2710, -0.0679, 0.0073],
    [0.0308, -0.1367, 0.8789, 0.2915, -0.0728, 0.0083],
    [0.0308, -0.1401, 0.8657, 0.3120, -0.0776, 0.0093],
    [0.0313, -0.1426, 0.8506, 0.3330, -0.0825, 0.0103],
    [0.0313, -0.1445, 0.8354, 0.3540, -0.0874, 0.0112],
    [0.0313, -0.1460, 0.8193, 0.3755, -0.0923, 0.0122],
    [0.0313, -0.1470, 0.8022, 0.3965, -0.0967, 0.0137],
    [0.0308, -0.1479, 0.7856, 0.4185, -0.1016, 0.0146],
    [0.0303, -0.1479, 0.7681, 0.4399, -0.1060, 0.0156],
    [0.0298, -0.1479, 0.7505, 0.4614, -0.1104, 0.0166],
    [0.0293, -0.1470, 0.7314, 0.4829, -0.1147, 0.0181],
    [0.0288, -0.1460, 0.7119, 0.5049, -0.1187, 0.0190],
    [0.0278, -0.1445, 0.6929, 0.5264, -0.1226, 0.0200],
    [0.0273, -0.1431, 0.6724, 0.5479, -0.1260, 0.0215],
    [0.0264, -0.1411, 0.6528, 0.5693, -0.1299, 0.0225],
    [0.0254, -0.1387, 0.6323, 0.5903, -0.1328, 0.0234],
    [0.0244, -0.1357, 0.6113, 0.6113, -0.1357, 0.0244],
    [0.0234, -0.1328, 0.5903, 0.6323, -0.1387, 0.0254],
    [0.0225, -0.1299, 0.5693, 0.6528, -0.1411, 0.0264],
    [0.0215, -0.1260, 0.5479, 0.6724, -0.1431, 0.0273],
    [0.0200, -0.1226, 0.5264, 0.6929, -0.1445, 0.0278],
    [0.0190, -0.1187, 0.5049, 0.7119, -0.1460, 0.0288],
    [0.0181, -0.1147, 0.4829, 0.7314, -0.1470, 0.0293],
    [0.0166, -0.1104, 0.4614, 0.7505, -0.1479, 0.0298],
    [0.0156, -0.1060, 0.4399, 0.7681, -0.1479, 0.0303],
    [0.0146, -0.1016, 0.4185, 0.7856, -0.1479, 0.0308],
    [0.0137, -0.0967, 0.3965, 0.8022, -0.1470, 0.0313],
    [0.0122, -0.0923, 0.3755, 0.8193, -0.1460, 0.0313],
    [0.0112, -0.0874, 0.3540, 0.8354, -0.1445, 0.0313],
    [0.0103, -0.0825, 0.3330, 0.8506, -0.1426, 0.0313],
    [0.0093, -0.0776, 0.3120, 0.8657, -0.1401, 0.0308],
    [0.0083, -0.0728, 0.2915, 0.8789, -0.1367, 0.0308],
    [0.0073, -0.0679, 0.2710, 0.8926, -0.1333, 0.0303],
    [0.0063, -0.0630, 0.2510, 0.9058, -0.1294, 0.0293],
    [0.0059, -0.0581, 0.2310, 0.9170, -0.1245, 0.0288],
    [0.0049, -0.0537, 0.2119, 0.9282, -0.1191, 0.0278],
    [0.0044, -0.0488, 0.1929, 0.9390, -0.1138, 0.0264],
    [0.0034, -0.0439, 0.1738, 0.9487, -0.1074, 0.0254],
    [0.0029, -0.0396, 0.1558, 0.9575, -0.1006, 0.0239],
    [0.0024, -0.0352, 0.1382, 0.9648, -0.0928, 0.0225],
    [0.0020, -0.0308, 0.1206, 0.9727, -0.0850, 0.0205],
    [0.0015, -0.0264, 0.1040, 0.9785, -0.0762, 0.0186],
    [0.0010, -0.0220, 0.0874, 0.9844, -0.0674, 0.0166],
    [0.0005, -0.0181, 0.0713, 0.9897, -0.0576, 0.0142],
    [0.0005, -0.0142, 0.0562, 0.9932, -0.0474, 0.0117],
    [0.0005, -0.0103, 0.0415, 0.9956, -0.0361, 0.0088],
    [0.0, -0.0068, 0.0269, 0.9985, -0.0249, 0.0063],
    [0.0, -0.0034, 0.0132, 1.0000, -0.0127, 0.0029],
];

/// NIS_Config.h's `coef_usm`: the unsharp mask, 64 phases of 6 taps.
pub(crate) const COEF_USM: [[f32; 6]; 64] = [
    [0.0, -0.6001, 1.2002, -0.6001, 0.0, 0.0],
    [0.0029, -0.6084, 1.1987, -0.5903, -0.0029, 0.0],
    [0.0049, -0.6147, 1.1958, -0.5791, -0.0068, 0.0005],
    [0.0073, -0.6196, 1.1890, -0.5659, -0.0103, 0.0],
    [0.0093, -0.6235, 1.1802, -0.5513, -0.0151, 0.0],
    [0.0112, -0.6265, 1.1699, -0.5352, -0.0195, 0.0005],
    [0.0122, -0.6270, 1.1582, -0.5181, -0.0259, 0.0005],
    [0.0142, -0.6284, 1.1455, -0.5005, -0.0317, 0.0005],
    [0.0156, -0.6265, 1.1274, -0.4790, -0.0386, 0.0005],
    [0.0166, -0.6235, 1.1089, -0.4570, -0.0454, 0.0010],
    [0.0176, -0.6187, 1.0879, -0.4346, -0.0532, 0.0010],
    [0.0181, -0.6138, 1.0659, -0.4102, -0.0615, 0.0015],
    [0.0190, -0.6069, 1.0405, -0.3843, -0.0698, 0.0015],
    [0.0195, -0.6006, 1.0161, -0.3574, -0.0796, 0.0020],
    [0.0200, -0.5928, 0.9893, -0.3286, -0.0898, 0.0024],
    [0.0200, -0.5820, 0.9580, -0.2988, -0.1001, 0.0029],
    [0.0200, -0.5728, 0.9292, -0.2690, -0.1104, 0.0034],
    [0.0200, -0.5620, 0.8975, -0.2368, -0.1226, 0.0039],
    [0.0205, -0.5498, 0.8643, -0.2046, -0.1343, 0.0044],
    [0.0200, -0.5371, 0.8301, -0.1709, -0.1465, 0.0049],
    [0.0195, -0.5239, 0.7944, -0.1367, -0.1587, 0.0054],
    [0.0195, -0.5107, 0.7598, -0.1021, -0.1724, 0.0059],
    [0.0190, -0.4966, 0.7231, -0.0649, -0.1865, 0.0063],
    [0.0186, -0.4819, 0.6846, -0.0288, -0.1997, 0.0068],
    [0.0186, -0.4668, 0.6460, 0.0093, -0.2144, 0.0073],
    [0.0176, -0.4507, 0.6055, 0.0479, -0.2290, 0.0083],
    [0.0171, -0.4370, 0.5693, 0.0859, -0.2446, 0.0088],
    [0.0161, -0.4199, 0.5283, 0.1255, -0.2598, 0.0098],
    [0.0161, -0.4048, 0.4883, 0.1655, -0.2754, 0.0103],
    [0.0151, -0.3887, 0.4497, 0.2041, -0.2910, 0.0107],
    [0.0142, -0.3711, 0.4072, 0.2446, -0.3066, 0.0117],
    [0.0137, -0.3555, 0.3672, 0.2852, -0.3228, 0.0122],
    [0.0132, -0.3394, 0.3262, 0.3262, -0.3394, 0.0132],
    [0.0122, -0.3228, 0.2852, 0.3672, -0.3555, 0.0137],
    [0.0117, -0.3066, 0.2446, 0.4072, -0.3711, 0.0142],
    [0.0107, -0.2910, 0.2041, 0.4497, -0.3887, 0.0151],
    [0.0103, -0.2754, 0.1655, 0.4883, -0.4048, 0.0161],
    [0.0098, -0.2598, 0.1255, 0.5283, -0.4199, 0.0161],
    [0.0088, -0.2446, 0.0859, 0.5693, -0.4370, 0.0171],
    [0.0083, -0.2290, 0.0479, 0.6055, -0.4507, 0.0176],
    [0.0073, -0.2144, 0.0093, 0.6460, -0.4668, 0.0186],
    [0.0068, -0.1997, -0.0288, 0.6846, -0.4819, 0.0186],
    [0.0063, -0.1865, -0.0649, 0.7231, -0.4966, 0.0190],
    [0.0059, -0.1724, -0.1021, 0.7598, -0.5107, 0.0195],
    [0.0054, -0.1587, -0.1367, 0.7944, -0.5239, 0.0195],
    [0.0049, -0.1465, -0.1709, 0.8301, -0.5371, 0.0200],
    [0.0044, -0.1343, -0.2046, 0.8643, -0.5498, 0.0205],
    [0.0039, -0.1226, -0.2368, 0.8975, -0.5620, 0.0200],
    [0.0034, -0.1104, -0.2690, 0.9292, -0.5728, 0.0200],
    [0.0029, -0.1001, -0.2988, 0.9580, -0.5820, 0.0200],
    [0.0024, -0.0898, -0.3286, 0.9893, -0.5928, 0.0200],
    [0.0020, -0.0796, -0.3574, 1.0161, -0.6006, 0.0195],
    [0.0015, -0.0698, -0.3843, 1.0405, -0.6069, 0.0190],
    [0.0015, -0.0615, -0.4102, 1.0659, -0.6138, 0.0181],
    [0.0010, -0.0532, -0.4346, 1.0879, -0.6187, 0.0176],
    [0.0010, -0.0454, -0.4570, 1.1089, -0.6235, 0.0166],
    [0.0005, -0.0386, -0.4790, 1.1274, -0.6265, 0.0156],
    [0.0005, -0.0317, -0.5005, 1.1455, -0.6284, 0.0142],
    [0.0005, -0.0259, -0.5181, 1.1582, -0.6270, 0.0122],
    [0.0005, -0.0195, -0.5352, 1.1699, -0.6265, 0.0112],
    [0.0, -0.0151, -0.5513, 1.1802, -0.6235, 0.0093],
    [0.0, -0.0103, -0.5659, 1.1890, -0.6196, 0.0073],
    [0.0005, -0.0068, -0.5791, 1.1958, -0.6147, 0.0049],
    [0.0, -0.0029, -0.5903, 1.1987, -0.6084, 0.0029],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// NVScaler's luma for output pixel (x, y), on the CPU from
    /// NIS_Scaler.h's per-pixel definition (6×6 window clamped to the
    /// frame), for a grey frame where the output is the luma.
    fn reference(luma: &[f64], (w, h): (u32, u32), n: &Nis, (x, y): (u32, u32)) -> f64 {
        let c = n.constants;
        let k = |v: f32| f64::from(v);
        let at = |tx: i64, ty: i64| luma[(ty.clamp(0, i64::from(h) - 1) * i64::from(w) + tx.clamp(0, i64::from(w) - 1)) as usize];
        // In f32, as NIS computes it: the phase follows f32's rounding.
        let sx = f64::from((x as f32 + 0.5) * n.scale[0] - 0.5);
        let sy = f64::from((y as f32 + 0.5) * n.scale[1] - 0.5);
        let (bx, by) = (sx.floor(), sy.floor());
        let (fx, fy) = (sx - bx, sy - by);
        let p: [[f64; 6]; 6] = std::array::from_fn(|i| std::array::from_fn(|j| at(bx as i64 + j as i64 - 2, by as i64 + i as i64 - 2)));
        let cs = |ph: usize, i: usize| f64::from(COEF_SCALE[ph.min(63)][i]);
        let cu = |ph: usize, i: usize| f64::from(COEF_USM[ph.min(63)][i]);
        let edge = |i: usize, j: usize| -> [f64; 4] {
            let (r, q) = (1 + i, 1 + j);
            let g0 = (p[r][q] + p[r][q + 1] + p[r][q + 2] - p[r + 2][q] - p[r + 2][q + 1] - p[r + 2][q + 2]).abs();
            let g45 = (p[r + 1][q] + p[r][q] + p[r][q + 1] - p[r + 2][q + 1] - p[r + 2][q + 2] - p[r + 1][q + 2]).abs();
            let g90 = (p[r][q] + p[r + 1][q] + p[r + 2][q] - p[r][q + 2] - p[r + 1][q + 2] - p[r + 2][q + 2]).abs();
            let g135 = (p[r + 1][q] + p[r + 2][q] + p[r + 2][q + 1] - p[r][q + 1] - p[r][q + 2] - p[r + 1][q + 2]).abs();
            let (a, b, cc, d) = (g0.max(g90), g0.min(g90), g45.max(g135), g45.min(g135));
            if a + cc == 0.0 {
                return [0.0; 4];
            }
            let e0 = (a / (a + cc)).min(1.0);
            let c0 = a > b * k(c.detect_ratio) && a > k(c.detect_thres) && a > d;
            let c45 = cc > d * k(c.detect_ratio) && cc > k(c.detect_thres) && cc > b;
            let (f0, f45) = if c0 && c45 { (e0, 1.0 - e0) } else { (1.0, 1.0) };
            let pick = |on: bool, v: f64| if on { v } else { 0.0 };
            [pick(c0 && a == g0, f0), pick(c0 && a != g0, f0), pick(c45 && cc == g45, f45), pick(c45 && cc != g45, f45)]
        };
        let lti = |q: &[f64; 6], ph: usize| {
            let first = ph <= 32;
            let s1 = if first { q[0] } else { q[3] };
            let s2 = if first { q[2] } else { q[5] };
            let a = q[1].max(q[2]).max(s1) - q[1].min(q[2]).min(s1);
            let b = q[3].max(q[4]).max(s2) - q[3].min(q[4]).min(s2);
            let ratio = a.max(b) / (a.min(b) + k(c.eps));
            (1.0 - ((ratio - k(c.min_contrast_ratio)) * k(c.ratio_norm)).clamp(0.0, 1.0)) * k(c.contrast_boost)
        };
        let poly6 = |q: [f64; 6], ph: usize| {
            let y: f64 = (0..6).map(|i| cs(ph, i) * q[i]).sum();
            let mut usm: f64 = (0..6).map(|i| cu(ph, i) * q[i]).sum();
            let ys = 1.0 - ((y - k(c.sharp_start_y)) * k(c.sharp_scale_y)).clamp(0.0, 1.0);
            usm *= ys * k(c.sharp_strength_scale) + k(c.sharp_strength_min);
            let limit = (ys * k(c.sharp_limit_scale) + k(c.sharp_limit_min)) * y;
            y + usm.max(-limit).min(limit) * lti(&q, ph)
        };
        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        let e = [[edge(0, 0), edge(0, 1)], [edge(1, 0), edge(1, 1)]];
        let wt: [f64; 4] = std::array::from_fn(|i| lerp(lerp(e[0][0][i], e[0][1][i], fx), lerp(e[1][0][i], e[1][1][i], fx), fy));
        let (phx, phy) = ((fx * 64.0) as usize, (fy * 64.0) as usize);
        let normal: f64 = (0..6).map(|j| (0..6).map(|i| p[i][j] * cs(phy, i)).sum::<f64>() * cs(phx, j)).sum();
        let mut out = normal * (1.0 - wt.iter().sum::<f64>());
        if wt[0] > 0.0 {
            out += poly6(std::array::from_fn(|i| lerp(p[i][2], p[i][3], fx)), phy) * wt[0];
        }
        if wt[1] > 0.0 {
            out += poly6(std::array::from_fn(|i| lerp(p[2][i], p[3][i], fy)), phx) * wt[1];
        }
        let diagonal = |t: [f64; 7], pp: f64| -> f64 {
            let (shift, pp) = if pp >= 1.0 { (1, pp - 1.0) } else { (0, pp) };
            poly6(std::array::from_fn(|i| t[i + shift]), (pp * 64.0) as usize)
        };
        if wt[2] > 0.0 {
            let mut b45 = 0.5 + 0.5 * (fx - fy);
            let mut t = [0.0; 7];
            t[1] = lerp(p[2][1], p[1][2], b45);
            t[3] = lerp(p[3][2], p[2][3], b45);
            t[5] = lerp(p[4][3], p[3][4], b45);
            b45 -= 0.5;
            let up = b45 >= 0.0;
            let [a, b, cc, d] = if up { [p[0][2], p[1][3], p[2][4], p[3][5]] } else { [p[2][0], p[3][1], p[4][2], p[5][3]] };
            t[0] = lerp(p[1][1], a, b45.abs());
            t[2] = lerp(p[2][2], b, b45.abs());
            t[4] = lerp(p[3][3], cc, b45.abs());
            t[6] = lerp(p[4][4], d, b45.abs());
            out += diagonal(t, fx + fy) * wt[2];
        }
        if wt[3] > 0.0 {
            let mut b135 = 0.5 * (fx + fy);
            let mut t = [0.0; 7];
            t[1] = lerp(p[3][1], p[4][2], b135);
            t[3] = lerp(p[2][2], p[3][3], b135);
            t[5] = lerp(p[1][3], p[2][4], b135);
            b135 -= 0.5;
            let up = b135 >= 0.0;
            let [a, b, cc, d] = if up { [p[5][2], p[4][3], p[3][4], p[2][5]] } else { [p[3][0], p[2][1], p[1][2], p[0][3]] };
            t[0] = lerp(p[4][1], a, b135.abs());
            t[2] = lerp(p[3][2], b, b135.abs());
            t[4] = lerp(p[2][3], cc, b135.abs());
            t[6] = lerp(p[1][4], d, b135.abs());
            out += diagonal(t, 1.0 + (fx - fy)) * wt[3];
        }
        out
    }

    #[test]
    fn constants_are_nvscalerupdateconfigs_at_the_middle() {
        let c = Constants::new(SHARPNESS);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(near(c.detect_ratio, 2.201_171_9));
        assert!(near(c.detect_thres, 0.0625));
        assert!(near(c.ratio_norm, 0.125), "1 / (10 - 2)");
        assert!(near(c.sharp_scale_y, 1.0 / 0.45));
        assert!(near(c.sharp_strength_min, 0.4));
        assert!(near(c.sharp_strength_scale, 1.2), "1.6 - 0.4");
        assert!(near(c.sharp_limit_min, 0.14));
        assert!(near(c.sharp_limit_scale, 0.36), "0.5 - 0.14");
        assert!(near(c.eps, 1.0 / 255.0));
        // The ends of the slider: 0 sharpens least, 1 most.
        let (lo, hi) = (Constants::new(0.0), Constants::new(1.0));
        assert!(near(lo.sharp_strength_min, 0.0) && near(lo.sharp_limit_min, 0.1));
        assert!(hi.sharp_strength_min + hi.sharp_strength_scale > c.sharp_strength_min + c.sharp_strength_scale);
    }

    #[test]
    fn nvscaler_covers_1x_to_2x_a_side() {
        let f = (1920, 1080);
        assert!(nis(f, (2560, 1440)).is_some());
        assert!(nis(f, (3840, 2160)).is_some(), "exactly 2x");
        assert!(nis(f, (2560, 1080)).is_some(), "one side 1:1");
        assert_eq!(nis(f, (1920, 1080)), None, "1:1 is the frame itself");
        assert_eq!(nis(f, (1280, 720)), None, "a downscale is balanced's");
        assert_eq!(nis(f, (3842, 2162)), None, "past 2x is balanced's");
        assert_eq!(nis((1280, 720), (3840, 2160)), None, "3x");
    }

    #[test]
    fn the_tables_are_niss() {
        // Each phase of the scaling filter sums to about 1, the unsharp
        // mask's to about 0 (NIS's tables are rounded to 1/2048).
        for (scale, usm) in COEF_SCALE.iter().zip(&COEF_USM) {
            assert!((scale.iter().sum::<f32>() - 1.0).abs() < 2e-3, "{scale:?}");
            assert!(usm.iter().sum::<f32>().abs() < 2e-3, "{usm:?}");
        }
        assert_eq!(COEF_SCALE[0], [0.0, 0.0, 1.0, 0.0, 0.0, 0.0], "phase 0 is the centre texel");
        assert_eq!(COEF_SCALE[32][2], COEF_SCALE[32][3], "phase 32 is symmetric");
    }

    #[test]
    fn uniform_layout() {
        let n = nis((1920, 1080), (2560, 1440)).unwrap();
        let b = n.bytes((440, 0));
        assert_eq!(b.len() as u64, UNIFORM_SIZE);
        let f = |i: usize| f32::from_ne_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!((f(0), f(1)), (1920.0, 1080.0));
        assert_eq!((f(2), f(3)), (0.75, 0.75), "kScale, exact at 4:3");
        assert_eq!((f(4), f(5)), (440.0, 0.0), "the viewport's origin");
        assert_eq!(f(8), n.constants.detect_ratio);
        assert_eq!(f(19), n.constants.sharp_limit_scale);
        // Phase 1 of coef_scale: its first vec4, then taps 4 and 5 padded.
        assert_eq!([f(20 + 8), f(20 + 9), f(20 + 10), f(20 + 11)], [0.0029, -0.0127, 1.0, 0.0132]);
        assert_eq!([f(20 + 12), f(20 + 13), f(20 + 14), f(20 + 15)], [-0.0034, 0.0, 0.0, 0.0]);
        // coef_usm after the 128 vec4s of coef_scale.
        assert_eq!(f(20 + 512 + 1), COEF_USM[0][1]);
    }

    /// The pass on a headless GPU, as scale.rs's GPU test runs its filters.
    struct Gpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
        layout: wgpu::BindGroupLayout,
        pipe: wgpu::RenderPipeline,
        timestamps: bool,
    }

    impl Gpu {
        fn new() -> Self {
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
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() })).unwrap();
            eprintln!("GPU: {}", adapter.get_info().name);
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[texture_entry(0, false), uniform_entry(1)],
            });
            let pipe = pipeline(&device, "nis", WGSL, &layout, wgpu::TextureFormat::Rgba8Unorm);
            Self { device, queue, layout, pipe, timestamps: !required_features.is_empty() }
        }

        fn read(&self, buffer: &wgpu::Buffer) -> Vec<u8> {
            buffer.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
            self.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let v = buffer.get_mapped_range(..).unwrap().to_vec();
            buffer.unmap();
            v
        }

        /// `src` (RGBA, `size`) upscaled into a `dst` viewport at `origin` on
        /// a target of `target` pixels, `runs` times: the viewport's pixels
        /// (RGBA rows) and the median GPU time of a run, where measurable.
        fn upscale(&self, src: &[u8], size: (u32, u32), dst: (u32, u32), origin: (u32, u32), runs: u32) -> (Vec<u8>, Option<f64>) {
            let (device, queue) = (&self.device, &self.queue);
            let texture = |s: (u32, u32), usage| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: s.0, height: s.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage,
                    view_formats: &[],
                })
            };
            let rgb = texture(size, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
            queue.write_texture(
                rgb.as_image_copy(),
                src,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size.0 * 4), rows_per_image: Some(size.1) },
                wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            );
            let n = nis(size, dst).expect("an upscale NVScaler covers");
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: UNIFORM_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&uniform, 0, &n.bytes(origin));
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&rgb.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                ],
            });
            let full = (origin.0 + dst.0, origin.1 + dst.1);
            let target = texture(full, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
            let target_view = target.create_view(&Default::default());
            let queries = self.timestamps.then(|| {
                device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count: runs * 2 })
            });
            let row = (dst.0 * 4).next_multiple_of(256);
            let out = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(row * dst.1),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let times_size = u64::from(runs * 2 * 8);
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
            for run in 0..runs {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: queries.as_ref().map(|q| wgpu::RenderPassTimestampWrites {
                        query_set: q,
                        beginning_of_pass_write_index: Some(run * 2),
                        end_of_pass_write_index: Some(run * 2 + 1),
                    }),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                // The viewport as the window letterboxes the frame (app.rs).
                pass.set_viewport(origin.0 as f32, origin.1 as f32, dst.0 as f32, dst.1 as f32, 0.0, 1.0);
                pass.set_pipeline(&self.pipe);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            }
            if let Some(q) = &queries {
                encoder.resolve_query_set(q, 0..runs * 2, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &times, 0, times_size);
            }
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: origin.0, y: origin.1, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &out,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(dst.1) },
                },
                wgpu::Extent3d { width: dst.0, height: dst.1, depth_or_array_layers: 1 },
            );
            queue.submit([encoder.finish()]);
            let padded = self.read(&out);
            let pixels = (0..dst.1).flat_map(|y| padded[(y * row) as usize..(y * row + dst.0 * 4) as usize].to_vec()).collect();
            let median = queries.is_some().then(|| {
                let t = self.read(&times);
                let ticks: Vec<u64> = t.as_chunks::<8>().0.iter().map(|c| u64::from_ne_bytes(*c)).collect();
                let period = f64::from(queue.get_timestamp_period());
                let mut ms: Vec<f64> = (0..runs as usize).map(|r| (ticks[r * 2 + 1] - ticks[r * 2]) as f64 * period / 1e6).collect();
                ms.sort_by(f64::total_cmp);
                ms[ms.len() / 2]
            });
            (pixels, median)
        }
    }

    /// The shader against NVScaler on the CPU (`reference`), on a grey frame
    /// of noise, blocks and diagonal stripes, through a letterboxed
    /// viewport; and the pass's GPU time. Every pattern carries a little
    /// noise: on exact stripes the edge test's comparisons tie (g_45 equal
    /// to g_0_90_min), and f32 on the GPU and f64 here break a tie apart;
    /// NVIDIA's own GLSL breaks it as the shader does. Run with
    /// `cargo test --release -- --ignored --nocapture gpu_nis`.
    /// `cargo test --release -- --ignored --nocapture gpu_nis`.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_nis_matches_nvscaler_and_is_timed() {
        let gpu = Gpu::new();
        let (w, h) = (1920u32, 1080u32);
        let grey: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let noise = (i.wrapping_mul(2_654_435_761) >> 24) as u8;
                let stripe = |on: bool, a: u8, b: u8| if on { a } else { b };
                let pattern = match x * 4 / w {
                    0 => return noise,
                    1 => stripe((x / 37 + y / 41) % 2 == 0, 220, 30),
                    2 => stripe(((x + y) / 9) % 2 == 0, 200, 40),
                    _ => stripe(((x + 2 * y) / 13) % 2 == 0, 180, 60),
                };
                pattern + noise / 16
            })
            .collect();
        let src: Vec<u8> = grey.iter().flat_map(|&v| [v, v, v, 255]).collect();
        let luma: Vec<f64> = grey.iter().map(|&v| f64::from(v) / 255.0).collect();
        // 4:3 lands on exact phases; 1.1667 and 2x as the window has them.
        for (dst, origin) in [((2560u32, 1440u32), (440u32, 0u32)), ((2240, 1260), (0, 90)), ((3840, 2160), (0, 0))] {
            let (pixels, median) = gpu.upscale(&src, (w, h), dst, origin, 60);
            let n = nis((w, h), dst).unwrap();
            let (mut worst, mut over, mut samples) = (0f64, 0u32, 0u32);
            for y in (0..dst.1).step_by(3) {
                for x in (0..dst.0).step_by(5) {
                    let want = reference(&luma, (w, h), &n, (x, y)).clamp(0.0, 1.0) * 255.0;
                    let got = f64::from(pixels[((y * dst.0 + x) * 4) as usize]);
                    let d = (got - want).abs();
                    worst = worst.max(d);
                    over += u32::from(d > 1.0);
                    samples += 1;
                }
            }
            let gpu_ms = median.map_or("no timestamps".into(), |ms| format!("GPU median {ms:.3} ms"));
            eprintln!(
                "nis {w}x{h} -> {}x{} at {origin:?}: {over} of {samples} over 1 level off NVScaler, worst {worst:.2}; {gpu_ms}",
                dst.0, dst.1
            );
            // Within a level, but for the odd float near-tie in the edge test.
            assert!(
                over <= samples / 10_000 && worst <= 8.0,
                "{dst:?}: {over} of {samples} values over 1 level off NVScaler, worst {worst:.2}"
            );
        }
    }

    /// NVScaler against the bilinear upscale of the same frame: flat stays
    /// flat, an edge stays where it is and gets narrower.
    #[test]
    #[ignore = "needs a GPU"]
    fn gpu_nis_sharpens_an_edge_in_place() {
        let gpu = Gpu::new();
        // Left half flat grey; right half a soft vertical edge (a 4-texel
        // ramp from dark to light at x = 1440).
        let (w, h) = (1920u32, 1080u32);
        let level = |x: u32| -> u8 {
            match x {
                ..1438 => 40,
                1438..1442 => (40 + (x - 1437) * 40) as u8,
                _ => 200,
            }
        };
        let src: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = i % w;
                let v = if x < w / 2 { 120 } else { level(x) };
                [v, v, v, 255]
            })
            .collect();
        for dst in [(2560u32, 1440u32), (3840, 2160)] {
            let (pixels, _) = gpu.upscale(&src, (w, h), dst, (0, 0), 1);
            let at = |x: u32, y: u32| f64::from(pixels[((y * dst.0 + x) * 4) as usize]);
            let y = dst.1 / 2;
            let worst_flat = (0..dst.0 / 2 - 8).step_by(7).map(|x| (at(x, y) - 120.0).abs()).fold(0f64, f64::max);
            assert!(worst_flat <= 1.0, "{dst:?}: a flat area moved {worst_flat} levels");

            // The middle row against the bilinear upscale of the same row:
            // where each crosses 10 %, 50 % and 90 % of the edge.
            let scale = f64::from(w) / f64::from(dst.0);
            let source = |x: i64| f64::from(level(x.clamp(0, i64::from(w) - 1) as u32));
            let bilinear = |dx: u32| {
                let s = (f64::from(dx) + 0.5) * scale - 0.5;
                let (f, i) = (s - s.floor(), s.floor() as i64);
                source(i) * (1.0 - f) + source(i + 1) * f
            };
            let (lo_x, hi_x) = ((1400.0 / scale) as u32, (1480.0 / scale) as u32);
            let crossing = |value: &dyn Fn(u32) -> f64, frac: f64| -> f64 {
                let t = 40.0 + (200.0 - 40.0) * frac;
                (lo_x..hi_x)
                    .find_map(|x| {
                        let (a, b) = (value(x), value(x + 1));
                        (a < t && b >= t).then(|| f64::from(x) + (t - a) / (b - a))
                    })
                    .unwrap_or_else(|| panic!("{dst:?}: no crossing at {frac}"))
            };
            let nis_row = |x: u32| at(x, y);
            let width = |v: &dyn Fn(u32) -> f64| crossing(v, 0.9) - crossing(v, 0.1);
            let (nis_w, bil_w) = (width(&nis_row), width(&bilinear));
            let shift = crossing(&nis_row, 0.5) - crossing(&bilinear, 0.5);
            eprintln!("nis -> {}x{}: edge 10-90 % {nis_w:.2} px (bilinear {bil_w:.2}), 50 % shift {shift:+.2} px", dst.0, dst.1);
            assert!(nis_w < bil_w * 0.9, "{dst:?}: the edge is {nis_w:.2} px wide, bilinear's {bil_w:.2}");
            assert!(shift.abs() < 0.5, "{dst:?}: the edge moved {shift:+.2} px");
        }
    }
}
