// Ported to WGSL for protoHDMIcapture (#68, widened for #73) from AMD FidelityFX SDK v1.1.4,
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

// ComputeOpticalFlowAdvanced as search.wgsl ports it, with the search
// window as a parameter (#73): `RADIUS` is set by the host before this
// source (16 for ffx: candidates -16..+15 on each axis, 1024 a
// block, where search.wgsl's +-8 has 256). The block's pixels are loaded
// as there (four 8x8 blocks a workgroup of 64); each thread now scores
// 4 x-offsets on several candidate rows. The minimum key is SAD above,
// then the candidate's distance from the window's centre (centre first on
// a tie, as FIX_TOP_LEFT_BIAS intends), then its coordinate.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> current: array<u32>;
@group(0) @binding(2) var<storage, read> previous: array<u32>;
@group(0) @binding(3) var<storage, read_write> flow: array<vec2i>;
@group(0) @binding(4) var<storage, read> scd: array<u32>;

const COMPARE: i32 = 8;
const SPAN: i32 = 2 * RADIUS;                // candidates a side
const BUF_X: i32 = (COMPARE + SPAN) / 4;     // packed words a row
const BUF_Y: i32 = COMPARE + SPAN;           // rows
const THREADS: i32 = 64;
const X_GROUPS: i32 = SPAN / 4;              // 4 x-offsets a thread
const ROW_STEP: i32 = THREADS / X_GROUPS;

var<workgroup> pixels: array<array<u32, 2>, 8>;
var<workgroup> search_buffer: array<u32, BUF_X * BUF_Y>;
var<workgroup> wg_min: atomic<u32>;
var<workgroup> wg_block_sad: atomic<u32>;
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

fn load_flow(pos: vec2i) -> vec2i {
    if (!in_flow(pos, p.of_size)) {
        return vec2i(0);
    }
    return flow[pos.y * p.of_size.x + pos.x];
}

fn store_flow(pos: vec2i, v: vec2i) {
    if (in_flow(pos, p.of_size)) {
        flow[pos.y * p.of_size.x + pos.x] = v;
    }
}

// SAD (at most 64 x 255, 14 bits) above, then the distance from the
// centre (6 bits), then y and x (5 bits each).
fn key(sad: u32, coord: vec2i) -> u32 {
    let a = abs(coord - vec2i(RADIUS));
    return (sad << 18u) | (u32(a.x + a.y) << 10u) | (u32(coord.y) << 5u) | u32(coord.x);
}

fn decode(bits: u32) -> vec2i {
    return vec2i(i32(bits & 0x1fu) - RADIUS, i32((bits >> 5u) & 0x1fu) - RADIUS);
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(workgroup_id) group_u: vec3u, @builtin(local_invocation_index) local_u: u32) {
    let group = vec2i(group_u.xy);
    let local = i32(local_u);
    // MapThreads, for loading the blocks' pixels.
    let search_id = vec2i(local & 3, (local >> 2u) & 15);
    let lane_block = ((local >> 1u) & 1) | (((local >> 5u) & 1) << 1u);
    let px = (group << vec2u(4u)) + search_id * vec2i(4, 1);
    // The candidates this thread scores: 4 x-offsets from x4, rows from y0.
    let x_group = local % X_GROUPS;
    let y0 = local / X_GROUPS;

    if (local == 0) {
        wg_changed = scd[SCD_HISTORY] & 1u;
    }
    if (workgroupUniformLoad(&wg_changed) != 0u) {
        if ((search_id.y & 7) == 0 && (search_id.x & 1) == 0) {
            store_flow(px >> vec2u(3u), vec2i(0));
        }
        return;
    }

    let use_prediction = p.level != p.levels - 1u;
    let packed = first_packed(px);
    let sad_4 = sad4(packed, second_packed(px));
    let of_group = group << vec2u(1u);
    let pixel_group = group << vec2u(4u);

    for (var by = 0; by < 2; by++) {
        for (var bx = 0; bx < 2; bx++) {
            let block_id = vec2i(bx, by);
            let block = by * 2 + bx;
            var vector = vec2i(0);
            if (use_prediction) {
                vector = load_flow(of_group + block_id);
            }
            if (lane_block == block) {
                pixels[search_id.y & 7][search_id.x & 1] = packed;
            }
            let base = pixel_group + block_id * 8 + vector - vec2i(RADIUS);
            for (var id = local; id < BUF_X * BUF_Y; id += THREADS) {
                search_buffer[id] = second_packed(base + vec2i((id % BUF_X) * 4, id / BUF_X));
            }
            if (local == 0) {
                atomicStore(&wg_min, 0xffffffffu);
                atomicStore(&wg_block_sad, 0u);
            }
            workgroupBarrier();

            var best = 0xffffffffu;
            for (var cy = y0; cy < SPAN; cy += ROW_STEP) {
                var sad = vec4u(0u);
                for (var dy = 0; dy < COMPARE; dy++) {
                    let row = (cy + dy) * BUF_X;
                    let a0 = search_buffer[row + x_group];
                    let a1 = search_buffer[row + x_group + 1];
                    let a2 = search_buffer[row + x_group + 2];
                    sad += qsad(a0, a1, pixels[dy][0]);
                    sad += qsad(a1, a2, pixels[dy][1]);
                }
                let x4 = x_group * 4;
                best = min(best, key(sad.x, vec2i(x4 + 0, cy)));
                best = min(best, key(sad.y, vec2i(x4 + 1, cy)));
                best = min(best, key(sad.z, vec2i(x4 + 2, cy)));
                best = min(best, key(sad.w, vec2i(x4 + 3, cy)));
            }
            atomicMin(&wg_min, best);
            if (lane_block == block) {
                atomicAdd(&wg_block_sad, sad_4);
            }
            workgroupBarrier();

            let min_key = atomicLoad(&wg_min);
            var new_vector = vector + decode(min_key);
            if (p.level == 0u && atomicLoad(&wg_block_sad) <= (min_key >> 18u)) {
                new_vector = vec2i(0);
            }
            if (local == 0) {
                store_flow(of_group + block_id, new_vector);
            }
            workgroupBarrier();
        }
    }
}
