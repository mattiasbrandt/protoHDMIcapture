// Ported to WGSL for protoHDMIcapture (#68) from AMD FidelityFX SDK v1.1.4,
// sdk/include/FidelityFX/gpu/opticalflow/, under AMD's licence:
//
// This file is part of the FidelityFX SDK.
//
// Copyright (C) 2024 Advanced Micro Devices, Inc.
// 
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files(the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and /or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions :
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

// ScaleOpticalFlowAdvanced (ffx_opticalflow_scale_optical_flow_advanced_v5.h):
// each vector of the next finer level starts as the best of the four
// nearest coarse vectors (doubled), judged by the SAD of its 4x4 luma
// region at this level. Group 4x4x4: z is the candidate.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> current: array<u32>;
@group(0) @binding(2) var<storage, read> previous: array<u32>;
@group(0) @binding(3) var<storage, read> coarse: array<vec2i>;
@group(0) @binding(4) var<storage, read_write> next_level: array<vec2i>;
@group(0) @binding(5) var<storage, read> scd: array<u32>;

var<workgroup> nearest_vectors: array<array<array<vec2i, 4>, 4>, 4>;
var<workgroup> local_region: array<array<array<u32, 4>, 4>, 4>;
var<workgroup> sads: array<array<array<u32, 4>, 4>, 4>;
var<workgroup> wg_changed: u32;

fn first_packed(pos: vec2i) -> u32 {
    let w = p.luma.x;
    let at = vec2i(clamp(pos.x, 0, w - 4), clamp(pos.y, 0, p.luma.y - 1));
    let i = at.y * w + at.x;
    return packed_luma(w, pos.x, current[i], current[i + 1], current[i + 2], current[i + 3]);
}

fn second_packed(pos: vec2i) -> u32 {
    let w = p.luma.x;
    let at = vec2i(clamp(pos.x, 0, w - 4), clamp(pos.y, 0, p.luma.y - 1));
    let i = at.y * w + at.x;
    return packed_luma(w, pos.x, previous[i], previous[i + 1], previous[i + 2], previous[i + 3]);
}

fn load_coarse(pos: vec2i) -> vec2i {
    if (!in_flow(pos, p.of_size)) {
        return vec2i(0);
    }
    return coarse[pos.y * p.of_size.x + pos.x];
}

fn store_next(pos: vec2i, v: vec2i) {
    if (in_flow(pos, p.next_of)) {
        next_level[pos.y * p.next_of.x + pos.x] = v;
    }
}

@compute @workgroup_size(4, 4, 4)
fn main(@builtin(global_invocation_id) gid: vec3u, @builtin(local_invocation_id) lid: vec3u, @builtin(local_invocation_index) local: u32) {
    let g = vec2i(gid.xy);
    if (local == 0u) {
        wg_changed = scd[SCD_HISTORY] & 1u;
    }
    if (workgroupUniformLoad(&wg_changed) != 0u) {
        store_next(g, vec2i(0));
        return;
    }

    let z = i32(lid.z);
    let x_offset = (z % 2) - 1 + g.x % 2;
    let y_offset = (z / 2) - 1 + g.y % 2;
    let src = vec2i(g.x / 2 + x_offset, g.y / 2 + y_offset);
    let nearest = load_coarse(src);
    nearest_vectors[lid.z][lid.y][lid.x] = nearest * 2;

    // One row of the 4x4 region a candidate (WG_DEPTH = 4 rows).
    local_region[lid.z][lid.y][lid.x] = first_packed(vec2i(g.x * 4, g.y * 4 + z));
    workgroupBarrier();

    var sad = 0u;
    for (var n = 0; n < 4; n++) {
        let at = vec2i(g.x * 4, g.y * 4 + n) + nearest;
        sad += sad4(local_region[n][lid.y][lid.x], second_packed(at));
    }
    sads[lid.z][lid.y][lid.x] = sad;
    workgroupBarrier();

    if (lid.z == 0u) {
        var best_sad = 0xffffffffu;
        var best = 0u;
        for (var n = 0u; n < 4u; n++) {
            if (sads[n][lid.y][lid.x] < best_sad) {
                best_sad = sads[n][lid.y][lid.x];
                best = n;
            }
        }
        store_next(g, nearest_vectors[best][lid.y][lid.x]);
    }
}
