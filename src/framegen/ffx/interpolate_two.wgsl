// protoHDMIcapture (#73), built on the #68 port of afmf-linux's
// shaders/afmf_interpolate.comp (interpolate.wgsl), under its licence:
//
// MIT License
//
// Copyright (c) 2026 Lain <lain@digitalexperiments.dev>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

// ffx-quality's interpolation (#73 step 3): the flow both ways. Forward
// (`flow_n`, the new frame's blocks): previous = new + v. Backward
// (`flow_p`, the previous frame's blocks): new = previous + w. A motion u
// carries content from the previous frame at x to the new frame at
// x + u, so the frame halfway shows at y what the previous frame had at
// y - u/2 and the new frame has at y + u/2.
//
// Each output pixel tries four motions: -v and w where y lies, each the
// nearest block's and the bilinear field's. A motion is checked at both
// ends: the new frame's own vector at y + u/2 should say -u (that point
// moves with u), and the previous frame's at y - u/2 should say u.
// - Both ends agree: the region is seen in both frames; the two samples
//   are blended. Of several, the motion whose samples match best.
// - Only the new frame's end agrees: a region the motion uncovers (seen
//   in the new frame, hidden behind something in the previous one): the
//   new frame's sample alone.
// - Only the previous frame's end agrees: a region being covered: the
//   previous frame's sample alone.
// - Neither: ffx's interpolation along the forward flow (interpolate.wgsl).
// A blend of the background with the object in front of it, the halo
// around a moving edge, is what the one-sided cases replace.
//
// Before all that, the static-HUD mask (hud_mask.wgsl, #73 step 4): where
// the pixel, or where the forward flow would fetch it from, is a block of
// still HUD, the two frames are blended unmoved, so HUD text over a moving
// background is not dragged along with the background.

struct Interp {
    size: vec2f,
    // Frame pixels per flow vector.
    block: f32,
    // Frame pixels per flow-luma pixel.
    scale: f32,
    of_size: vec2i,
    max_motion: f32,
    hud: f32,
    // The HUD mask's blocks (8x8 frame pixels each); use_mask 0: none.
    mask_size: vec2i,
    use_mask: u32,
    _pad: u32,
}

@group(0) @binding(0) var<uniform> ip: Interp;
@group(0) @binding(1) var prev_frame: texture_2d<f32>;
@group(0) @binding(2) var next_frame: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
@group(0) @binding(4) var<storage, read> flow_n: array<vec2i>;
@group(0) @binding(5) var<storage, read> scd: array<u32>;
@group(0) @binding(6) var<storage, read> flow_p: array<vec2i>;
@group(0) @binding(7) var<storage, read> hud_mask: array<u32>;

fn masked(y: vec2f) -> bool {
    let b = clamp(vec2i(floor(y / 8.0)), vec2i(0), ip.mask_size - 1);
    return hud_mask[b.y * ip.mask_size.x + b.x] != 0u;
}

fn at_block(b: vec2i) -> i32 {
    let at = clamp(b, vec2i(0), ip.of_size - 1);
    return at.y * ip.of_size.x + at.x;
}

// A flow's vector in frame pixels: the nearest block's.
fn nearest_n(y: vec2f) -> vec2f {
    return vec2f(flow_n[at_block(vec2i(floor(y / ip.block)))]) * ip.scale;
}
fn nearest_p(y: vec2f) -> vec2f {
    return vec2f(flow_p[at_block(vec2i(floor(y / ip.block)))]) * ip.scale;
}

// Bilinear between block centres, as interpolate.wgsl's flow_at.
fn bilinear_n(y: vec2f) -> vec2f {
    let b = y / ip.block - 0.5;
    let b0 = vec2i(floor(b));
    let f = b - vec2f(b0);
    let v00 = vec2f(flow_n[at_block(b0)]);
    let v10 = vec2f(flow_n[at_block(b0 + vec2i(1, 0))]);
    let v01 = vec2f(flow_n[at_block(b0 + vec2i(0, 1))]);
    let v11 = vec2f(flow_n[at_block(b0 + vec2i(1, 1))]);
    return mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) * ip.scale;
}
fn bilinear_p(y: vec2f) -> vec2f {
    let b = y / ip.block - 0.5;
    let b0 = vec2i(floor(b));
    let f = b - vec2f(b0);
    let v00 = vec2f(flow_p[at_block(b0)]);
    let v10 = vec2f(flow_p[at_block(b0 + vec2i(1, 0))]);
    let v01 = vec2f(flow_p[at_block(b0 + vec2i(0, 1))]);
    let v11 = vec2f(flow_p[at_block(b0 + vec2i(1, 1))]);
    return mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) * ip.scale;
}

// How a motion fares: which ends agree, its samples, how well they match.
struct Try {
    a: vec4f,
    b: vec4f,
    next_agrees: bool,
    prev_agrees: bool,
    mismatch: f32,
}

fn attempt(y: vec2f, u: vec2f) -> Try {
    var t: Try;
    let inv = 1.0 / ip.size;
    let half_u = 0.5 * u;
    t.a = textureSampleLevel(prev_frame, smp, (y - half_u) * inv, 0.0);
    t.b = textureSampleLevel(next_frame, smp, (y + half_u) * inv, 0.0);
    // Agreement within two flow-luma pixels (vectors are whole luma
    // pixels; bilinear ones fall between).
    let tolerance = 2.0 * ip.scale;
    let trusted = dot(u, u) <= ip.max_motion * ip.max_motion;
    t.next_agrees = trusted && length(nearest_n(y + half_u) + u) <= tolerance;
    t.prev_agrees = trusted && length(nearest_p(y - half_u) - u) <= tolerance;
    let d = abs(t.a.rgb - t.b.rgb);
    t.mismatch = max(d.r, max(d.g, d.b));
    return t;
}

// interpolate.wgsl's answer along the forward flow.
fn one_way(y: vec2f, at_next: vec4f) -> vec4f {
    let inv = 1.0 / ip.size;
    let v = bilinear_n(y);
    if (dot(v, v) > ip.max_motion * ip.max_motion) {
        return 0.5 * (textureSampleLevel(next_frame, smp, y * inv, 0.0) + textureSampleLevel(prev_frame, smp, y * inv, 0.0));
    }
    let half_v = 0.5 * v;
    let from_next = textureSampleLevel(next_frame, smp, (y - half_v) * inv, 0.0);
    let from_prev = textureSampleLevel(prev_frame, smp, (y + half_v) * inv, 0.0);
    let d = abs(from_next.rgb - from_prev.rgb);
    let level = 0.5 * (dot(from_next.rgb, vec3f(1.0 / 3.0)) + dot(from_prev.rgb, vec3f(1.0 / 3.0)));
    let disagreement = max(d.r, max(d.g, d.b)) / (level + 0.05);
    let unmoved = smoothstep(0.15, 0.6, disagreement);
    return mix(0.5 * (from_next + from_prev), at_next, unmoved);
}

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let pix = vec2i(in.pos.xy);
    let y = in.pos.xy;
    let at_next = textureLoad(next_frame, pix, 0);
    if ((scd[SCD_HISTORY] & 1u) != 0u) {
        return at_next;
    }
    // A pixel equal in both frames where something moves: a HUD, kept
    // (interpolate.wgsl's rule).
    let vn = bilinear_n(y);
    if (ip.hud > 0.0 && dot(vn, vn) > 0.0) {
        let at_prev = textureLoad(prev_frame, pix, 0);
        let d = abs(at_next.rgb - at_prev.rgb);
        if (max(d.r, max(d.g, d.b)) <= ip.hud) {
            return at_next;
        }
    }
    if (ip.use_mask != 0u) {
        let half_v = 0.5 * vn;
        if (masked(y) || masked(y - half_v) || masked(y + half_v)) {
            return vec4f((0.5 * (at_next + textureLoad(prev_frame, pix, 0))).rgb, 1.0);
        }
    }
    var motions: array<vec2f, 4>;
    motions[0] = -nearest_n(y);
    motions[1] = nearest_p(y);
    motions[2] = -vn;
    motions[3] = bilinear_p(y);

    var best = 1e9;
    var blended = vec4f(0.0);
    var one_sided = vec4f(0.0);
    var have_one_sided = false;
    for (var i = 0; i < 4; i++) {
        let t = attempt(y, motions[i]);
        if (t.next_agrees && t.prev_agrees) {
            if (t.mismatch < best) {
                best = t.mismatch;
                blended = 0.5 * (t.a + t.b);
            }
        } else if (!have_one_sided && t.next_agrees) {
            one_sided = t.b;
            have_one_sided = true;
        } else if (!have_one_sided && t.prev_agrees) {
            one_sided = t.a;
            have_one_sided = true;
        }
    }
    if (best < 1e9) {
        return vec4f(blended.rgb, 1.0);
    }
    if (have_one_sided) {
        return vec4f(one_sided.rgb, 1.0);
    }
    return vec4f(one_way(y, at_next).rgb, 1.0);
}
