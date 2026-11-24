// Ported to WGSL for protoHDMIcapture (#73) from openfg,
// src/openfg/shaders/ui_mask.comp and ui_mask_dilate.comp, under its
// licence:
//
// MIT License
//
// Copyright (c) 2026 openfg contributors
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

// ffx's static-HUD mask (#73 step 4), openfg's static-UI block
// detector: an 8x8 block of frame pixels with at least four pixels that
// are the same in both frames and have high-contrast detail (HUD text, a
// minimap's edges, icons) is static. Unlike openfg's, the mask has a
// history: a block counts as HUD only once it has been static for
// `frames` pairs in a row (a still background is not a HUD the moment it
// stops). `main` writes the counts and the mask; `dilate` widens the mask
// by a block, so the anti-aliased halo of a glyph is covered too.

struct Hud {
    size: vec2i,
    blocks: vec2i,
    frames: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> h: Hud;
@group(0) @binding(1) var prev_frame: texture_2d<f32>;
@group(0) @binding(2) var next_frame: texture_2d<f32>;
@group(0) @binding(3) var<storage, read_write> counts: array<u32>;
@group(0) @binding(4) var<storage, read_write> mask: array<u32>;

fn luma(c: vec3f) -> f32 {
    return dot(c, vec3f(0.2126, 0.7152, 0.0722));
}

fn hit(p: vec2i) -> bool {
    let q = min(p, h.size - 1);
    let a = textureLoad(prev_frame, q, 0).rgb;
    let c = textureLoad(next_frame, q, 0).rgb;
    let la = luma(a);
    let lax = luma(textureLoad(prev_frame, min(q + vec2i(1, 0), h.size - 1), 0).rgb);
    let lay = luma(textureLoad(prev_frame, min(q + vec2i(0, 1), h.size - 1), 0).rgb);
    return length(a - c) < 0.012 && max(abs(la - lax), abs(la - lay)) > 0.12;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let b = vec2i(id.xy);
    if (any(b >= h.blocks)) {
        return;
    }
    let i = b.y * h.blocks.x + b.x;
    let origin = b * 8;
    // openfg's prefilter: most blocks have no static detail at all.
    var sparse = 0;
    for (var y = 0; y < 8; y += 2) {
        for (var x = 0; x < 8; x += 2) {
            if (hit(origin + vec2i(x, y))) {
                sparse++;
            }
        }
    }
    var hits = 0;
    if (sparse > 0) {
        for (var y = 0; y < 8; y++) {
            for (var x = 0; x < 8; x++) {
                if (hit(origin + vec2i(x, y))) {
                    hits++;
                }
            }
        }
    }
    var count = 0u;
    if (hits >= 4) {
        count = min(counts[i] + 1u, 255u);
    }
    counts[i] = count;
    mask[i] = select(0u, 1u, count >= h.frames);
}
