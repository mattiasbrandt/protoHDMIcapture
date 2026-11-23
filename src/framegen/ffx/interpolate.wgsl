// Ported to WGSL for protoHDMIcapture (#68) from afmf-linux,
// shaders/afmf_interpolate.comp, under its licence:
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

// The frame halfway between the previous and the current new frame, from
// the optical flow at level 0 (one vector per 8x8 block of the flow's luma,
// previous = current + v). After afmf-linux's afmf_interpolate.comp: the
// content at pixel y sits at current(y - v/2) and previous(y + v/2); both
// are sampled bilinearly and averaged, the flow bilinear between block
// centres; where the two warps disagree the current frame's own pixel
// takes over; a pixel equal in both frames on a moving block (a HUD) is
// kept as it is; motion beyond what the flow can track blends the frames
// unmoved. A scene change shows the current frame itself: the real frame.

struct Interp {
    // The frames' size in pixels.
    size: vec2f,
    // Frame pixels per flow block (8 x the flow's scale).
    block: f32,
    // Frame pixels per flow-luma pixel.
    scale: f32,
    of_size: vec2i,
    max_motion: f32,
    hud: f32,
}

@group(0) @binding(0) var<uniform> ip: Interp;
@group(0) @binding(1) var prev_frame: texture_2d<f32>;
@group(0) @binding(2) var next_frame: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
@group(0) @binding(4) var<storage, read> flow: array<vec2i>;
@group(0) @binding(5) var<storage, read> scd: array<u32>;

fn block_vector(b: vec2i) -> vec2f {
    let at = clamp(b, vec2i(0), ip.of_size - 1);
    return vec2f(flow[at.y * ip.of_size.x + at.x]);
}

fn flow_at(y: vec2f) -> vec2f {
    let b = y / ip.block - 0.5;
    let b0 = vec2i(floor(b));
    let f = b - vec2f(b0);
    let v00 = block_vector(b0);
    let v10 = block_vector(b0 + vec2i(1, 0));
    let v01 = block_vector(b0 + vec2i(0, 1));
    let v11 = block_vector(b0 + vec2i(1, 1));
    return mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) * ip.scale;
}

@fragment fn fs(in: VOut) -> @location(0) vec4f {
    let pix = vec2i(in.pos.xy);
    let y = in.pos.xy;
    let at_next = textureLoad(next_frame, pix, 0);
    if ((scd[SCD_HISTORY] & 1u) != 0u) {
        return at_next;
    }
    let inv = 1.0 / ip.size;
    let v = flow_at(y);
    let trusted = dot(v, v) <= ip.max_motion * ip.max_motion;
    if (trusted && ip.hud > 0.0 && dot(v, v) > 0.0) {
        let at_prev = textureLoad(prev_frame, pix, 0);
        let d = abs(at_next.rgb - at_prev.rgb);
        if (max(d.r, max(d.g, d.b)) <= ip.hud) {
            return at_next;
        }
    }
    if (!trusted) {
        return 0.5 * (textureSampleLevel(next_frame, smp, y * inv, 0.0) + textureSampleLevel(prev_frame, smp, y * inv, 0.0));
    }
    let half_v = 0.5 * v;
    let from_next = textureSampleLevel(next_frame, smp, (y - half_v) * inv, 0.0);
    let from_prev = textureSampleLevel(prev_frame, smp, (y + half_v) * inv, 0.0);
    let d = abs(from_next.rgb - from_prev.rgb);
    let level = 0.5 * (dot(from_next.rgb, vec3f(1.0 / 3.0)) + dot(from_prev.rgb, vec3f(1.0 / 3.0)));
    let disagreement = max(d.r, max(d.g, d.b)) / (level + 0.05);
    let unmoved = smoothstep(0.15, 0.6, disagreement);
    var colour = 0.5 * (from_next + from_prev);
    colour = mix(colour, at_next, unmoved);
    return vec4f(colour.rgb, 1.0);
}
