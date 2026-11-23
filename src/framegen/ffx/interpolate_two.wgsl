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
// The static-HUD mask (hud_mask.wgsl, #73 step 4) keeps a still HUD
// still: inside its blocks the flow is zero (the two frames blended
// unmoved), feathered over one block so its edge is no seam. Outside, a
// pixel warps with its own motions; a motion whose sample would land on
// the mask is not trusted (it would drag the HUD into the background), so
// the pixel takes its other motions. The rule used to blend a pixel
// unmoved wherever its motion touched the mask, which cut a block-edged
// seam through a moving road beside a screen-locked car that the mask
// takes for HUD (#73, the driving clip).

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

// One mask block as 0 or 1 (clamped to the frame's blocks).
fn mask_at(b: vec2i) -> f32 {
    let at = clamp(b, vec2i(0), ip.mask_size - 1);
    return select(0.0, 1.0, hud_mask[at.y * ip.mask_size.x + at.x] != 0u);
}

// The mask feathered: bilinear between block centres, so a pixel inside a
// run of masked blocks is 1, one outside them 0, and the edge ramps over
// one block.
fn mask_weight(y: vec2f) -> f32 {
    let b = y / 8.0 - 0.5;
    let b0 = vec2i(floor(b));
    let f = b - vec2f(b0);
    let top = mix(mask_at(b0), mask_at(b0 + vec2i(1, 0)), f.x);
    let bottom = mix(mask_at(b0 + vec2i(0, 1)), mask_at(b0 + vec2i(1, 1)), f.x);
    return mix(top, bottom, f.y);
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

// A both-ends motion's weight falls by e per this much colour mismatch.
const MATCH_SIGMA: f32 = 0.06;
// A one-end motion's weight, against a perfect both-ends match's 1.
const ONE_SIDED: f32 = 0.05;

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
    // pixels; bilinear ones fall between), widening with the motion's
    // length (UnFlow's |v|²-scaled consistency bound): a long motion is not
    // rejected for a miss that is small against it.
    let tolerance = sqrt(4.0 * ip.scale * ip.scale + 0.02 * dot(u, u));
    // A moving sample taken from the mask would drag the HUD (ip.use_mask).
    let off_mask = ip.use_mask == 0u || dot(u, u) < 0.25 || !(masked(y - half_u) || masked(y + half_u));
    let trusted = dot(u, u) <= ip.max_motion * ip.max_motion && off_mask;
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
    // Inside the mask: zero flow, the two frames blended unmoved; at its
    // edge, ramped into the warped picture.
    var hud_w = 0.0;
    if (ip.use_mask != 0u) {
        hud_w = mask_weight(y);
    }
    let unmoved = vec4f((0.5 * (at_next + textureLoad(prev_frame, pix, 0))).rgb, 1.0);
    if (hud_w >= 1.0) {
        return unmoved;
    }
    return mix(warped(y, vn, at_next), unmoved, hud_w);
}

// The picture halfway from the motions (the mask aside).
fn warped(y: vec2f, vn: vec2f, at_next: vec4f) -> vec4f {
    var motions: array<vec2f, 4>;
    motions[0] = -nearest_n(y);
    motions[1] = nearest_p(y);
    motions[2] = -vn;
    motions[3] = bilinear_p(y);

    // Soft weights over the motions, not the best one alone: a motion
    // seen at both ends weighs by how well its two samples match; one seen
    // at one end only (a region uncovered or covered) weighs less, its one
    // sample. Neighbouring pixels then change their answer gradually, not
    // by a switch between motions (the shredding, #73).
    var sum = vec3f(0.0);
    var total = 0.0;
    for (var i = 0; i < 4; i++) {
        let t = attempt(y, motions[i]);
        let fit = exp(-t.mismatch / MATCH_SIGMA);
        if (t.next_agrees && t.prev_agrees) {
            sum += fit * 0.5 * (t.a.rgb + t.b.rgb);
            total += fit;
        } else if (t.next_agrees) {
            sum += ONE_SIDED * t.b.rgb;
            total += ONE_SIDED;
        } else if (t.prev_agrees) {
            sum += ONE_SIDED * t.a.rgb;
            total += ONE_SIDED;
        }
    }
    if (total > 0.0) {
        return vec4f(sum / total, 1.0);
    }
    return vec4f(one_way(y, at_next).rgb, 1.0);
}
