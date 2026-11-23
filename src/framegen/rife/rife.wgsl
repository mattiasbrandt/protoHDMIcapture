// RIFE v4.6's IFNet (#74) as WGSL compute: the layers of rife-ncnn-vulkan's
// converted model, models/rife-v4.6/flownet.param, each with ncnn's own
// semantics so the port reproduces ncnn's output (the golden test).
//
// RIFE: Copyright (c) 2021 hzwer, MIT (https://github.com/hzwer/Practical-RIFE).
// rife-ncnn-vulkan: Copyright (c) 2020 nihui, MIT
// (https://github.com/nihui/rife-ncnn-vulkan); the Warp layer below follows
// its src/warp.cpp. Both notices in full: NOTICE.md beside this file.
//
// Tensors are f32 storage buffers laid out CHW: channel c, row y, column x
// at c * h * w + y * w + x. A concatenation is several kernels writing one
// buffer at their own channel offsets.

// The type the heavy kernels (conv1, conv2, deconv) compute and keep
// workgroup memory in: f32, or f16 where the device has SHADER_F16 (rife.rs
// makes that variant: `enable f16;` and this alias, as rife-ncnn-vulkan
// runs fp16 on GPUs that have it).
alias A = f32;

struct P {
    // Source: channel offset, channels, width, height.
    s_off: u32, s_c: u32, s_w: u32, s_h: u32,
    // Destination: channel offset, channels (of the buffer), width, height.
    d_off: u32, d_c: u32, d_w: u32, d_h: u32,
    // Second source's channel offset (warp: the flow), a flag word, and a
    // multiplier.
    f_off: u32, flags: u32, mul: f32, scale: f32,
}

@group(0) @binding(0) var<uniform> p: P;

// ---- the frames in: two Rgba8Unorm textures into [img0, img1, timestep],
// zero beyond the frame (rife.cpp pads to 32n with zeros) --------------------

@group(0) @binding(7) var prev_tex: texture_2d<f32>;
@group(0) @binding(8) var next_tex: texture_2d<f32>;
@group(0) @binding(9) var<storage, read_write> frames: array<f32>;

@compute @workgroup_size(8, 8)
fn load(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let plane = p.d_w * p.d_h;
    let i = g.y * p.d_w + g.x;
    var a = vec3f(0.0);
    var b = vec3f(0.0);
    if g.x < p.s_w && g.y < p.s_h {
        // ncnn's from_pixels gives 0..255 floats, rife.cpp multiplies by
        // 1/255: the same as the texture's unorm value up to rounding.
        a = round(textureLoad(prev_tex, g.xy, 0).rgb * 255.0) * (1.0 / 255.0);
        b = round(textureLoad(next_tex, g.xy, 0).rgb * 255.0) * (1.0 / 255.0);
    }
    frames[i] = a.r;
    frames[plane + i] = a.g;
    frames[2u * plane + i] = a.b;
    frames[3u * plane + i] = b.r;
    frames[4u * plane + i] = b.g;
    frames[5u * plane + i] = b.b;
    frames[6u * plane + i] = p.mul; // the timestep
}

// ---- copies, resizes, warps -------------------------------------------------

@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;
@group(0) @binding(3) var<storage, read> flow: array<f32>;

/// `s_c` channels from `s_off` to `d_off`, times `mul`.
@compute @workgroup_size(8, 8)
fn copy(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let i = g.y * p.d_w + g.x;
    let plane = p.d_w * p.d_h;
    for (var c = 0u; c < p.s_c; c++) {
        dst[(p.d_off + c) * plane + i] = src[(p.s_off + c) * plane + i] * p.mul;
    }
}

/// ncnn's linear_coeffs (interp.cpp, align_corner 0): source index and
/// weight of the second sample for output index `d`.
fn coeff(d: u32, sz: u32, osz: u32) -> vec2f {
    let scale = f32(sz) / f32(osz);
    var f = (f32(d) + 0.5) * scale - 0.5;
    var s = floor(f);
    f = f - s;
    if s < 0.0 { s = 0.0; f = 0.0; }
    if s >= f32(sz) - 1.0 { s = f32(sz) - 2.0; f = 1.0; }
    return vec2f(s, f);
}

/// ncnn's Interp, bilinear (resize_type 2): `s_c` channels from `s_off`
/// at s_w x s_h into `d_off` at d_w x d_h, times `mul`.
@compute @workgroup_size(8, 8)
fn interp(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let cx = coeff(g.x, p.s_w, p.d_w);
    let cy = coeff(g.y, p.s_h, p.d_h);
    let x0 = u32(max(cx.x, 0.0));
    let y0 = u32(max(cy.x, 0.0));
    let x1 = min(x0 + 1u, p.s_w - 1u);
    let y1 = min(y0 + 1u, p.s_h - 1u);
    let splane = p.s_w * p.s_h;
    let dplane = p.d_w * p.d_h;
    for (var c = 0u; c < p.s_c; c++) {
        let base = (p.s_off + c) * splane;
        let s0 = src[base + y0 * p.s_w + x0];
        let s1 = src[base + y0 * p.s_w + x1];
        let s2 = src[base + y1 * p.s_w + x0];
        let s3 = src[base + y1 * p.s_w + x1];
        // ncnn resizes the rows first, then blends them.
        let r0 = s0 * (1.0 - cx.y) + s1 * cx.y;
        let r1 = s2 * (1.0 - cx.y) + s3 * cx.y;
        let v = r0 * (1.0 - cy.y) + r1 * cy.y;
        dst[(p.d_off + c) * dplane + g.y * p.d_w + g.x] = v * p.mul;
    }
}

/// rife.Warp (src/warp.cpp): the 3 channels at `s_off` sampled at each
/// pixel plus the 2 flow channels at `f_off`, bilinear; as there, the
/// sample's weight is taken after the corner was clamped to the image.
fn warped(c: u32, x: u32, y: u32) -> f32 {
    let w = p.d_w;
    let h = p.d_h;
    let plane = w * h;
    let i = y * w + x;
    let sx = f32(x) + flow[p.f_off * plane + i];
    let sy = f32(y) + flow[(p.f_off + 1u) * plane + i];
    let fx0 = floor(sx);
    let fy0 = floor(sy);
    let x0 = i32(clamp(fx0, 0.0, f32(w - 1u)));
    let y0 = i32(clamp(fy0, 0.0, f32(h - 1u)));
    let x1 = i32(clamp(fx0 + 1.0, 0.0, f32(w - 1u)));
    let y1 = i32(clamp(fy0 + 1.0, 0.0, f32(h - 1u)));
    let alpha = sx - f32(x0);
    let beta = sy - f32(y0);
    let base = (p.s_off + c) * plane;
    let v0 = src[base + u32(y0) * w + u32(x0)];
    let v1 = src[base + u32(y0) * w + u32(x1)];
    let v2 = src[base + u32(y1) * w + u32(x0)];
    let v3 = src[base + u32(y1) * w + u32(x1)];
    let v4 = v0 * (1.0 - alpha) + v1 * alpha;
    let v5 = v2 * (1.0 - alpha) + v3 * alpha;
    return v4 * (1.0 - beta) + v5 * beta;
}

@compute @workgroup_size(8, 8)
fn warp(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let plane = p.d_w * p.d_h;
    for (var c = 0u; c < 3u; c++) {
        dst[(p.d_off + c) * plane + g.y * p.d_w + g.x] = warped(c, g.x, g.y);
    }
}

// ---- an IFBlock's layers ------------------------------------------------------

@group(0) @binding(4) var<storage, read> weight: array<f32>;
@group(0) @binding(5) var<storage, read> bias: array<f32>;
@group(0) @binding(6) var<storage, read> residual: array<f32>;

const LEAKY: u32 = 1u;
const RESIDUAL: u32 = 2u;

fn activate(v: f32, flags: u32) -> f32 {
    if (flags & LEAKY) != 0u {
        return select(v * 0.2, v, v > 0.0);
    }
    return v;
}

// A 3x3 convolution, padding 1, stride 1 or 2 (ncnn Convolution: weights
// [out][in][3][3], bias; 9=2 a leaky ReLU of 0.2; a residual block's conv,
// add, leaky ReLU: `RESIDUAL`). A workgroup computes a 16x16 output tile
// for 16 output channels, each thread a 2x2 block of it, the 16 channels
// as 4 vec4 per pixel in registers (named, never an indexed array: an
// indexed private array spills on some drivers). Input channels pass
// through workgroup memory a few at a time: the tile, (16 - 1) * stride
// + 3 on a side, and the weights as vec4 of 4 output channels.

const OC: u32 = 16u;
const PARTIAL: u32 = 8u;

/// Split-K's end: the `s_c` slices of `src` (each d_c channels at d_w x d_h)
/// summed, then bias, residual and activation as an unsplit conv does.
@compute @workgroup_size(8, 8)
fn reduce(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let dplane = p.d_w * p.d_h;
    let at = g.z * dplane + g.y * p.d_w + g.x;
    var v = 0.0;
    for (var s = 0u; s < p.s_c; s++) { v += src[s * p.d_c * dplane + at]; }
    v += bias[g.z];
    if (p.flags & RESIDUAL) != 0u { v = v + residual[at]; }
    dst[at] = activate(v, p.flags);
}
var<workgroup> tile: array<A, 2592>; // stride 1: 8 x 18 x 18; stride 2: 2 x 33 x 33
var<workgroup> wt: array<vec4<A>, 288>; // channels x 9 x 4

fn store(x: u32, y: u32, oc0: u32, slice: u32, a: array<vec4<A>, 4>) {
    if x >= p.d_w || y >= p.d_h { return; }
    let dplane = p.d_w * p.d_h;
    for (var j = 0u; j < 4u; j++) {
        let v4 = a[j];
        for (var e = 0u; e < 4u; e++) {
            let oc = oc0 + j * 4u + e;
            if oc >= p.d_c { return; }
            let at = oc * dplane + y * p.d_w + x;
            if (p.flags & PARTIAL) != 0u {
                dst[slice * p.d_c * dplane + at] = f32(v4[e]);
                continue;
            }
            var v = f32(v4[e]) + bias[oc];
            if (p.flags & RESIDUAL) != 0u { v = v + residual[at]; }
            dst[at] = activate(v, p.flags);
        }
    }
}

@compute @workgroup_size(8, 8)
fn conv1(@builtin(local_invocation_id) l: vec3u, @builtin(workgroup_id) wg: vec3u) {
    let lin = l.y * 8u + l.x;
    // Constants, so the tile loops divide by constants.
    const st = 1u;
    const ci = 8u;
    const ts = 18u;
    let ox0 = wg.x * 16u;
    let oy0 = wg.y * 16u;
    // Split-K (`PARTIAL`): z is slice * the output-channel blocks + the
    // block; a slice sums its `f_off` input channels, written raw to its
    // own plane set of `dst`; `reduce` adds the slices up.
    let ocb = (p.d_c + OC - 1u) / OC;
    let oc0 = (wg.z % ocb) * OC;
    let slice = wg.z / ocb;
    let per = select(p.s_c, p.f_off, (p.flags & PARTIAL) != 0u);
    let cbeg = slice * per;
    let cend = min(cbeg + per, p.s_c);
    let splane = p.s_w * p.s_h;
    var a000 = vec4<A>(0.0);
    var a001 = vec4<A>(0.0);
    var a002 = vec4<A>(0.0);
    var a003 = vec4<A>(0.0);
    var a010 = vec4<A>(0.0);
    var a011 = vec4<A>(0.0);
    var a012 = vec4<A>(0.0);
    var a013 = vec4<A>(0.0);
    var a100 = vec4<A>(0.0);
    var a101 = vec4<A>(0.0);
    var a102 = vec4<A>(0.0);
    var a103 = vec4<A>(0.0);
    var a110 = vec4<A>(0.0);
    var a111 = vec4<A>(0.0);
    var a112 = vec4<A>(0.0);
    var a113 = vec4<A>(0.0);
    for (var c0 = cbeg; c0 < cend; c0 += ci) {
        for (var t = lin; t < ci * ts * ts; t += 64u) {
            let c = t / (ts * ts);
            let r = t % (ts * ts);
            let ix = i32(ox0 * st + r % ts) - 1;
            let iy = i32(oy0 * st + r / ts) - 1;
            var v = 0.0;
            if c0 + c < cend && ix >= 0 && iy >= 0 && ix < i32(p.s_w) && iy < i32(p.s_h) {
                v = src[(p.s_off + c0 + c) * splane + u32(iy) * p.s_w + u32(ix)];
            }
            tile[t] = A(v);
        }
        for (var t = lin; t < ci * 9u * 4u; t += 64u) {
            let j = t % 4u;
            let k = (t / 4u) % 9u;
            let c = t / 36u;
            var v = vec4f(0.0);
            if c0 + c < cend {
                for (var e = 0u; e < 4u; e++) {
                    let oc = oc0 + j * 4u + e;
                    if oc < p.d_c { v[e] = weight[(oc * p.s_c + c0 + c) * 9u + k]; }
                }
            }
            wt[t] = vec4<A>(v);
        }
        workgroupBarrier();
        let n = min(ci, cend - c0);
        for (var c = 0u; c < n; c++) {
            let cb = c * ts * ts + l.y * 2u * st * ts + l.x * 2u * st;
            let wc = c * 36u;
            { // tap 0
            let tb = cb + 0u * ts + 0u;
            let w0 = wt[wc + 0u]; let w1 = wt[wc + 1u]; let w2 = wt[wc + 2u]; let w3 = wt[wc + 3u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 1
            let tb = cb + 0u * ts + 1u;
            let w0 = wt[wc + 4u]; let w1 = wt[wc + 5u]; let w2 = wt[wc + 6u]; let w3 = wt[wc + 7u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 2
            let tb = cb + 0u * ts + 2u;
            let w0 = wt[wc + 8u]; let w1 = wt[wc + 9u]; let w2 = wt[wc + 10u]; let w3 = wt[wc + 11u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 3
            let tb = cb + 1u * ts + 0u;
            let w0 = wt[wc + 12u]; let w1 = wt[wc + 13u]; let w2 = wt[wc + 14u]; let w3 = wt[wc + 15u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 4
            let tb = cb + 1u * ts + 1u;
            let w0 = wt[wc + 16u]; let w1 = wt[wc + 17u]; let w2 = wt[wc + 18u]; let w3 = wt[wc + 19u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 5
            let tb = cb + 1u * ts + 2u;
            let w0 = wt[wc + 20u]; let w1 = wt[wc + 21u]; let w2 = wt[wc + 22u]; let w3 = wt[wc + 23u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 6
            let tb = cb + 2u * ts + 0u;
            let w0 = wt[wc + 24u]; let w1 = wt[wc + 25u]; let w2 = wt[wc + 26u]; let w3 = wt[wc + 27u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 7
            let tb = cb + 2u * ts + 1u;
            let w0 = wt[wc + 28u]; let w1 = wt[wc + 29u]; let w2 = wt[wc + 30u]; let w3 = wt[wc + 31u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 8
            let tb = cb + 2u * ts + 2u;
            let w0 = wt[wc + 32u]; let w1 = wt[wc + 33u]; let w2 = wt[wc + 34u]; let w3 = wt[wc + 35u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
        }
        workgroupBarrier();
    }
    store(ox0 + l.x * 2u + 0u, oy0 + l.y * 2u + 0u, oc0, slice, array<vec4<A>, 4>(a000, a001, a002, a003));
    store(ox0 + l.x * 2u + 1u, oy0 + l.y * 2u + 0u, oc0, slice, array<vec4<A>, 4>(a010, a011, a012, a013));
    store(ox0 + l.x * 2u + 0u, oy0 + l.y * 2u + 1u, oc0, slice, array<vec4<A>, 4>(a100, a101, a102, a103));
    store(ox0 + l.x * 2u + 1u, oy0 + l.y * 2u + 1u, oc0, slice, array<vec4<A>, 4>(a110, a111, a112, a113));
}

@compute @workgroup_size(8, 8)
fn conv2(@builtin(local_invocation_id) l: vec3u, @builtin(workgroup_id) wg: vec3u) {
    let lin = l.y * 8u + l.x;
    // Constants, so the tile loops divide by constants.
    const st = 2u;
    const ci = 2u;
    const ts = 33u;
    let ox0 = wg.x * 16u;
    let oy0 = wg.y * 16u;
    // Split-K (`PARTIAL`): z is slice * the output-channel blocks + the
    // block; a slice sums its `f_off` input channels, written raw to its
    // own plane set of `dst`; `reduce` adds the slices up.
    let ocb = (p.d_c + OC - 1u) / OC;
    let oc0 = (wg.z % ocb) * OC;
    let slice = wg.z / ocb;
    let per = select(p.s_c, p.f_off, (p.flags & PARTIAL) != 0u);
    let cbeg = slice * per;
    let cend = min(cbeg + per, p.s_c);
    let splane = p.s_w * p.s_h;
    var a000 = vec4<A>(0.0);
    var a001 = vec4<A>(0.0);
    var a002 = vec4<A>(0.0);
    var a003 = vec4<A>(0.0);
    var a010 = vec4<A>(0.0);
    var a011 = vec4<A>(0.0);
    var a012 = vec4<A>(0.0);
    var a013 = vec4<A>(0.0);
    var a100 = vec4<A>(0.0);
    var a101 = vec4<A>(0.0);
    var a102 = vec4<A>(0.0);
    var a103 = vec4<A>(0.0);
    var a110 = vec4<A>(0.0);
    var a111 = vec4<A>(0.0);
    var a112 = vec4<A>(0.0);
    var a113 = vec4<A>(0.0);
    for (var c0 = cbeg; c0 < cend; c0 += ci) {
        for (var t = lin; t < ci * ts * ts; t += 64u) {
            let c = t / (ts * ts);
            let r = t % (ts * ts);
            let ix = i32(ox0 * st + r % ts) - 1;
            let iy = i32(oy0 * st + r / ts) - 1;
            var v = 0.0;
            if c0 + c < cend && ix >= 0 && iy >= 0 && ix < i32(p.s_w) && iy < i32(p.s_h) {
                v = src[(p.s_off + c0 + c) * splane + u32(iy) * p.s_w + u32(ix)];
            }
            tile[t] = A(v);
        }
        for (var t = lin; t < ci * 9u * 4u; t += 64u) {
            let j = t % 4u;
            let k = (t / 4u) % 9u;
            let c = t / 36u;
            var v = vec4f(0.0);
            if c0 + c < cend {
                for (var e = 0u; e < 4u; e++) {
                    let oc = oc0 + j * 4u + e;
                    if oc < p.d_c { v[e] = weight[(oc * p.s_c + c0 + c) * 9u + k]; }
                }
            }
            wt[t] = vec4<A>(v);
        }
        workgroupBarrier();
        let n = min(ci, cend - c0);
        for (var c = 0u; c < n; c++) {
            let cb = c * ts * ts + l.y * 2u * st * ts + l.x * 2u * st;
            let wc = c * 36u;
            { // tap 0
            let tb = cb + 0u * ts + 0u;
            let w0 = wt[wc + 0u]; let w1 = wt[wc + 1u]; let w2 = wt[wc + 2u]; let w3 = wt[wc + 3u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 1
            let tb = cb + 0u * ts + 1u;
            let w0 = wt[wc + 4u]; let w1 = wt[wc + 5u]; let w2 = wt[wc + 6u]; let w3 = wt[wc + 7u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 2
            let tb = cb + 0u * ts + 2u;
            let w0 = wt[wc + 8u]; let w1 = wt[wc + 9u]; let w2 = wt[wc + 10u]; let w3 = wt[wc + 11u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 3
            let tb = cb + 1u * ts + 0u;
            let w0 = wt[wc + 12u]; let w1 = wt[wc + 13u]; let w2 = wt[wc + 14u]; let w3 = wt[wc + 15u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 4
            let tb = cb + 1u * ts + 1u;
            let w0 = wt[wc + 16u]; let w1 = wt[wc + 17u]; let w2 = wt[wc + 18u]; let w3 = wt[wc + 19u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 5
            let tb = cb + 1u * ts + 2u;
            let w0 = wt[wc + 20u]; let w1 = wt[wc + 21u]; let w2 = wt[wc + 22u]; let w3 = wt[wc + 23u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 6
            let tb = cb + 2u * ts + 0u;
            let w0 = wt[wc + 24u]; let w1 = wt[wc + 25u]; let w2 = wt[wc + 26u]; let w3 = wt[wc + 27u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 7
            let tb = cb + 2u * ts + 1u;
            let w0 = wt[wc + 28u]; let w1 = wt[wc + 29u]; let w2 = wt[wc + 30u]; let w3 = wt[wc + 31u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
            { // tap 8
            let tb = cb + 2u * ts + 2u;
            let w0 = wt[wc + 32u]; let w1 = wt[wc + 33u]; let w2 = wt[wc + 34u]; let w3 = wt[wc + 35u];
            let i00 = tile[tb + 0u * st * ts + 0u * st];
            let i01 = tile[tb + 0u * st * ts + 1u * st];
            let i10 = tile[tb + 1u * st * ts + 0u * st];
            let i11 = tile[tb + 1u * st * ts + 1u * st];
            a000 += i00 * w0;
            a001 += i00 * w1;
            a002 += i00 * w2;
            a003 += i00 * w3;
            a010 += i01 * w0;
            a011 += i01 * w1;
            a012 += i01 * w2;
            a013 += i01 * w3;
            a100 += i10 * w0;
            a101 += i10 * w1;
            a102 += i10 * w2;
            a103 += i10 * w3;
            a110 += i11 * w0;
            a111 += i11 * w1;
            a112 += i11 * w2;
            a113 += i11 * w3;
            }
        }
        workgroupBarrier();
    }
    store(ox0 + l.x * 2u + 0u, oy0 + l.y * 2u + 0u, oc0, slice, array<vec4<A>, 4>(a000, a001, a002, a003));
    store(ox0 + l.x * 2u + 1u, oy0 + l.y * 2u + 0u, oc0, slice, array<vec4<A>, 4>(a010, a011, a012, a013));
    store(ox0 + l.x * 2u + 0u, oy0 + l.y * 2u + 1u, oc0, slice, array<vec4<A>, 4>(a100, a101, a102, a103));
    store(ox0 + l.x * 2u + 1u, oy0 + l.y * 2u + 1u, oc0, slice, array<vec4<A>, 4>(a110, a111, a112, a113));
}

/// ncnn Deconvolution, kernel 4, stride 2, padding 1, bias, 24 outputs
/// (weights [out][in][4][4]; ncnn scatters input pixel (iy, ix) to output
/// (iy * 2 + ky - 1, ix * 2 + kx - 1)). Gathered: output (2m + py, 2n + px)
/// takes rows ky 1 (iy m) and 3 (iy m - 1) for py 0, ky 0 (iy m + 1) and
/// 2 (iy m) for py 1, and the same for columns. A thread is one input
/// position (m, n) and its four outputs for all 24 channels; a workgroup
/// 8x8 positions, the input through workgroup memory with a border of one,
/// 8 channels at a time.

const DCI: u32 = 8u;
var<workgroup> dtile: array<A, 800>; // DCI x 10 x 10
var<workgroup> dwt: array<vec4<A>, 768>; // DCI x 16 taps x 6 (24 outputs)

fn dstore(x: u32, y: u32, a: array<vec4<A>, 6>) {
    if x >= p.d_w || y >= p.d_h { return; }
    let dplane = p.d_w * p.d_h;
    for (var j = 0u; j < 6u; j++) {
        let v4 = a[j];
        for (var e = 0u; e < 4u; e++) {
            let oc = j * 4u + e;
            dst[oc * dplane + y * p.d_w + x] = f32(v4[e]) + bias[oc];
        }
    }
}

@compute @workgroup_size(8, 8)
fn deconv(@builtin(local_invocation_id) l: vec3u, @builtin(workgroup_id) wg: vec3u) {
    let lin = l.y * 8u + l.x;
    let m = wg.y * 8u + l.y;
    let n = wg.x * 8u + l.x;
    let splane = p.s_w * p.s_h;
    var o000 = vec4<A>(0.0);
    var o001 = vec4<A>(0.0);
    var o002 = vec4<A>(0.0);
    var o003 = vec4<A>(0.0);
    var o004 = vec4<A>(0.0);
    var o005 = vec4<A>(0.0);
    var o010 = vec4<A>(0.0);
    var o011 = vec4<A>(0.0);
    var o012 = vec4<A>(0.0);
    var o013 = vec4<A>(0.0);
    var o014 = vec4<A>(0.0);
    var o015 = vec4<A>(0.0);
    var o100 = vec4<A>(0.0);
    var o101 = vec4<A>(0.0);
    var o102 = vec4<A>(0.0);
    var o103 = vec4<A>(0.0);
    var o104 = vec4<A>(0.0);
    var o105 = vec4<A>(0.0);
    var o110 = vec4<A>(0.0);
    var o111 = vec4<A>(0.0);
    var o112 = vec4<A>(0.0);
    var o113 = vec4<A>(0.0);
    var o114 = vec4<A>(0.0);
    var o115 = vec4<A>(0.0);
    for (var c0 = 0u; c0 < p.s_c; c0 += DCI) {
        for (var t = lin; t < DCI * 100u; t += 64u) {
            let c = t / 100u;
            let r = t % 100u;
            let ix = i32(wg.x * 8u + r % 10u) - 1;
            let iy = i32(wg.y * 8u + r / 10u) - 1;
            var v = 0.0;
            if c0 + c < p.s_c && ix >= 0 && iy >= 0 && ix < i32(p.s_w) && iy < i32(p.s_h) {
                v = src[(c0 + c) * splane + u32(iy) * p.s_w + u32(ix)];
            }
            dtile[t] = A(v);
        }
        for (var t = lin; t < DCI * 16u * 6u; t += 64u) {
            let j = t % 6u;
            let k = (t / 6u) % 16u;
            let c = t / 96u;
            var v = vec4f(0.0);
            if c0 + c < p.s_c {
                for (var e = 0u; e < 4u; e++) {
                    v[e] = weight[((j * 4u + e) * p.s_c + c0 + c) * 16u + k];
                }
            }
            dwt[t] = vec4<A>(v);
        }
        workgroupBarrier();
        let nc = min(DCI, p.s_c - c0);
        for (var c = 0u; c < nc; c++) {
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 1u]; let wb = (c * 16u + 5u) * 6u;
              o000 += v * dwt[wb + 0u]; o001 += v * dwt[wb + 1u]; o002 += v * dwt[wb + 2u]; o003 += v * dwt[wb + 3u]; o004 += v * dwt[wb + 4u]; o005 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 0u]; let wb = (c * 16u + 7u) * 6u;
              o000 += v * dwt[wb + 0u]; o001 += v * dwt[wb + 1u]; o002 += v * dwt[wb + 2u]; o003 += v * dwt[wb + 3u]; o004 += v * dwt[wb + 4u]; o005 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 0u) * 10u + l.x + 1u]; let wb = (c * 16u + 13u) * 6u;
              o000 += v * dwt[wb + 0u]; o001 += v * dwt[wb + 1u]; o002 += v * dwt[wb + 2u]; o003 += v * dwt[wb + 3u]; o004 += v * dwt[wb + 4u]; o005 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 0u) * 10u + l.x + 0u]; let wb = (c * 16u + 15u) * 6u;
              o000 += v * dwt[wb + 0u]; o001 += v * dwt[wb + 1u]; o002 += v * dwt[wb + 2u]; o003 += v * dwt[wb + 3u]; o004 += v * dwt[wb + 4u]; o005 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 2u]; let wb = (c * 16u + 4u) * 6u;
              o010 += v * dwt[wb + 0u]; o011 += v * dwt[wb + 1u]; o012 += v * dwt[wb + 2u]; o013 += v * dwt[wb + 3u]; o014 += v * dwt[wb + 4u]; o015 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 1u]; let wb = (c * 16u + 6u) * 6u;
              o010 += v * dwt[wb + 0u]; o011 += v * dwt[wb + 1u]; o012 += v * dwt[wb + 2u]; o013 += v * dwt[wb + 3u]; o014 += v * dwt[wb + 4u]; o015 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 0u) * 10u + l.x + 2u]; let wb = (c * 16u + 12u) * 6u;
              o010 += v * dwt[wb + 0u]; o011 += v * dwt[wb + 1u]; o012 += v * dwt[wb + 2u]; o013 += v * dwt[wb + 3u]; o014 += v * dwt[wb + 4u]; o015 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 0u) * 10u + l.x + 1u]; let wb = (c * 16u + 14u) * 6u;
              o010 += v * dwt[wb + 0u]; o011 += v * dwt[wb + 1u]; o012 += v * dwt[wb + 2u]; o013 += v * dwt[wb + 3u]; o014 += v * dwt[wb + 4u]; o015 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 2u) * 10u + l.x + 1u]; let wb = (c * 16u + 1u) * 6u;
              o100 += v * dwt[wb + 0u]; o101 += v * dwt[wb + 1u]; o102 += v * dwt[wb + 2u]; o103 += v * dwt[wb + 3u]; o104 += v * dwt[wb + 4u]; o105 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 2u) * 10u + l.x + 0u]; let wb = (c * 16u + 3u) * 6u;
              o100 += v * dwt[wb + 0u]; o101 += v * dwt[wb + 1u]; o102 += v * dwt[wb + 2u]; o103 += v * dwt[wb + 3u]; o104 += v * dwt[wb + 4u]; o105 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 1u]; let wb = (c * 16u + 9u) * 6u;
              o100 += v * dwt[wb + 0u]; o101 += v * dwt[wb + 1u]; o102 += v * dwt[wb + 2u]; o103 += v * dwt[wb + 3u]; o104 += v * dwt[wb + 4u]; o105 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 0u]; let wb = (c * 16u + 11u) * 6u;
              o100 += v * dwt[wb + 0u]; o101 += v * dwt[wb + 1u]; o102 += v * dwt[wb + 2u]; o103 += v * dwt[wb + 3u]; o104 += v * dwt[wb + 4u]; o105 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 2u) * 10u + l.x + 2u]; let wb = (c * 16u + 0u) * 6u;
              o110 += v * dwt[wb + 0u]; o111 += v * dwt[wb + 1u]; o112 += v * dwt[wb + 2u]; o113 += v * dwt[wb + 3u]; o114 += v * dwt[wb + 4u]; o115 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 2u) * 10u + l.x + 1u]; let wb = (c * 16u + 2u) * 6u;
              o110 += v * dwt[wb + 0u]; o111 += v * dwt[wb + 1u]; o112 += v * dwt[wb + 2u]; o113 += v * dwt[wb + 3u]; o114 += v * dwt[wb + 4u]; o115 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 2u]; let wb = (c * 16u + 8u) * 6u;
              o110 += v * dwt[wb + 0u]; o111 += v * dwt[wb + 1u]; o112 += v * dwt[wb + 2u]; o113 += v * dwt[wb + 3u]; o114 += v * dwt[wb + 4u]; o115 += v * dwt[wb + 5u]; }
            { let v = dtile[c * 100u + (l.y + 1u) * 10u + l.x + 1u]; let wb = (c * 16u + 10u) * 6u;
              o110 += v * dwt[wb + 0u]; o111 += v * dwt[wb + 1u]; o112 += v * dwt[wb + 2u]; o113 += v * dwt[wb + 3u]; o114 += v * dwt[wb + 4u]; o115 += v * dwt[wb + 5u]; }
        }
        workgroupBarrier();
    }
    dstore(n * 2u + 0u, m * 2u + 0u, array<vec4<A>, 6>(o000, o001, o002, o003, o004, o005));
    dstore(n * 2u + 1u, m * 2u + 0u, array<vec4<A>, 6>(o010, o011, o012, o013, o014, o015));
    dstore(n * 2u + 0u, m * 2u + 1u, array<vec4<A>, 6>(o100, o101, o102, o103, o104, o105));
    dstore(n * 2u + 1u, m * 2u + 1u, array<vec4<A>, 6>(o110, o111, o112, o113, o114, o115));
}

/// A block's end: its 24-channel deconvolution PixelShuffled (factor 2) to
/// 6 channels at s_w x s_h, resized by ncnn's Interp to d_w x d_h (the
/// image's), the flow (channels 0..4) times `mul` (the block's scale), into
/// the running flow and mask (`dst`, 5 channels: flow 0..4, mask 4).
/// flags bit 0: add to them (every block but the first). Scale 1: no resize.
@compute @workgroup_size(8, 8)
fn block_out(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let dplane = p.d_w * p.d_h;
    let i = g.y * p.d_w + g.x;
    let hw = p.s_w / 2u; // the deconvolution's output size
    let hh = p.s_h / 2u;
    let hplane = hw * hh;
    var x0 = g.x;
    var y0 = g.y;
    var x1 = g.x;
    var y1 = g.y;
    var ax = 0.0;
    var ay = 0.0;
    let resize = p.s_w != p.d_w || p.s_h != p.d_h;
    if resize {
        let cx = coeff(g.x, p.s_w, p.d_w);
        let cy = coeff(g.y, p.s_h, p.d_h);
        x0 = u32(max(cx.x, 0.0)); y0 = u32(max(cy.x, 0.0));
        x1 = min(x0 + 1u, p.s_w - 1u); y1 = min(y0 + 1u, p.s_h - 1u);
        ax = cx.y; ay = cy.y;
    }
    for (var c = 0u; c < 5u; c++) {
        // PixelShuffle: channel c at (y, x) is channel c * 4 + (y % 2) * 2 + x % 2
        // of the deconvolution at (y / 2, x / 2).
        let s0 = src[(c * 4u + (y0 % 2u) * 2u + x0 % 2u) * hplane + (y0 / 2u) * hw + x0 / 2u];
        let s1 = src[(c * 4u + (y0 % 2u) * 2u + x1 % 2u) * hplane + (y0 / 2u) * hw + x1 / 2u];
        let s2 = src[(c * 4u + (y1 % 2u) * 2u + x0 % 2u) * hplane + (y1 / 2u) * hw + x0 / 2u];
        let s3 = src[(c * 4u + (y1 % 2u) * 2u + x1 % 2u) * hplane + (y1 / 2u) * hw + x1 / 2u];
        var v = s0;
        if resize {
            let r0 = s0 * (1.0 - ax) + s1 * ax;
            let r1 = s2 * (1.0 - ax) + s3 * ax;
            v = r0 * (1.0 - ay) + r1 * ay;
        }
        if c < 4u { v = v * p.mul; }
        if (p.flags & 1u) != 0u {
            // ncnn: Eltwise sum, previous * 1 + new * scale (flow); add (mask).
            v = dst[c * dplane + i] + v;
        }
        dst[c * dplane + i] = v;
    }
}

/// The merge: sigmoid of the mask, img0 warped by flow 0..2 times it, plus
/// img1 warped by flow 2..4 times one minus it (ncnn: sub_75 0=7, rsub).
/// `src` is the frames ([img0, img1, timestep]), `flow` the flow and mask;
/// out to `dst`, 3 channels at d_w x d_h (the padded size).
@compute @workgroup_size(8, 8)
fn merge(@builtin(global_invocation_id) g: vec3u) {
    if g.x >= p.d_w || g.y >= p.d_h { return; }
    let plane = p.d_w * p.d_h;
    let i = g.y * p.d_w + g.x;
    let m = 1.0 / (1.0 + exp(-flow[4u * plane + i]));
    for (var c = 0u; c < 3u; c++) {
        let a = warp_merge(c, 0u, 0u, g.x, g.y);
        let b = warp_merge(c, 3u, 2u, g.x, g.y);
        dst[c * plane + i] = a * m + b * (1.0 - m);
    }
}

/// `warped`, with the image and flow channels given (merge reads both).
fn warp_merge(c: u32, img: u32, fl: u32, x: u32, y: u32) -> f32 {
    let w = p.d_w;
    let h = p.d_h;
    let plane = w * h;
    let i = y * w + x;
    let sx = f32(x) + flow[fl * plane + i];
    let sy = f32(y) + flow[(fl + 1u) * plane + i];
    let fx0 = floor(sx);
    let fy0 = floor(sy);
    let x0 = i32(clamp(fx0, 0.0, f32(w - 1u)));
    let y0 = i32(clamp(fy0, 0.0, f32(h - 1u)));
    let x1 = i32(clamp(fx0 + 1.0, 0.0, f32(w - 1u)));
    let y1 = i32(clamp(fy0 + 1.0, 0.0, f32(h - 1u)));
    let alpha = sx - f32(x0);
    let beta = sy - f32(y0);
    let base = (img + c) * plane;
    let v0 = src[base + u32(y0) * w + u32(x0)];
    let v1 = src[base + u32(y0) * w + u32(x1)];
    let v2 = src[base + u32(y1) * w + u32(x0)];
    let v3 = src[base + u32(y1) * w + u32(x1)];
    let v4 = v0 * (1.0 - alpha) + v1 * alpha;
    let v5 = v2 * (1.0 - alpha) + v3 * alpha;
    return v4 * (1.0 - beta) + v5 * beta;
}

// ---- a scene cut: RIFE has none, so the two frames' luma histograms (64
// bins, every 4th pixel each way of the frame, s_w x s_h, inside the padded
// d_w x d_h) are compared; more than CUT of the mass moved is a cut, and
// the output is the new frame (#74; the idea of #68's FidelityFX test) ----------

const BINS: u32 = 64u;
const CUT: f32 = 0.5;
@group(0) @binding(10) var<storage, read_write> hist: array<atomic<u32>, 128>;
@group(0) @binding(11) var<storage, read_write> cut_flag: array<u32, 1>;

@compute @workgroup_size(8, 8)
fn histogram(@builtin(global_invocation_id) g: vec3u) {
    let x = g.x * 4u;
    let y = g.y * 4u;
    if x >= p.s_w || y >= p.s_h { return; }
    let plane = p.d_w * p.d_h;
    let i = y * p.d_w + x;
    for (var f = 0u; f < 2u; f++) {
        let o = f * 3u * plane;
        let luma = 0.299 * src[o + i] + 0.587 * src[o + plane + i] + 0.114 * src[o + 2u * plane + i];
        let bin = min(u32(max(luma, 0.0) * f32(BINS)), BINS - 1u);
        atomicAdd(&hist[f * BINS + bin], 1u);
    }
}

@compute @workgroup_size(1)
fn cut() {
    var n0 = 0u;
    var n1 = 0u;
    for (var b = 0u; b < BINS; b++) {
        n0 += atomicLoad(&hist[b]);
        n1 += atomicLoad(&hist[BINS + b]);
    }
    var moved = 0.0;
    for (var b = 0u; b < BINS; b++) {
        moved += abs(f32(atomicLoad(&hist[b])) / f32(max(n0, 1u)) - f32(atomicLoad(&hist[BINS + b])) / f32(max(n1, 1u)));
    }
    // Half the L1 distance: the share of the mass that moved, 0..1.
    cut_flag[0] = select(0u, 1u, moved * 0.5 > CUT);
}
