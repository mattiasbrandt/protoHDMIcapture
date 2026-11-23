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

// ComputeOpticalFlowAdvanced (ffx_opticalflow_compute_optical_flow_v5.h),
// the GLSL permutation (no msad4; FIX_TOP_LEFT_BIAS and
// LOCAL_SEARCH_FALLBACK on). A workgroup of 64 matches four 8x8 blocks of
// the current luma, one after the other, against a +-8 pixel window of the
// previous luma around the coarser level's vector: 256 candidates, four a
// thread. The vector says where the block was: previous = current + v.
//
// Changes for WGSL: the minimum and the block's own SAD are reduced with
// workgroup atomics instead of wave operations (any GPU, any wave size);
// the scene-change test is made uniform with workgroupUniformLoad.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> current: array<u32>;
@group(0) @binding(2) var<storage, read> previous: array<u32>;
@group(0) @binding(3) var<storage, read_write> flow: array<vec2i>;
@group(0) @binding(4) var<storage, read> scd: array<u32>;

const COMPARE: i32 = 8;
const RADIUS: i32 = 8;
const BUF_X: i32 = 6;  // (COMPARE + 2 * RADIUS) / 4 packed words
const BUF_Y: i32 = 24; // COMPARE + 2 * RADIUS rows
const THREADS: i32 = 64;

var<workgroup> pixels: array<array<u32, 2>, 8>;
var<workgroup> search_buffer: array<u32, 144>;
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

fn encode_search_coord(coord: vec2i) -> u32 {
    let a = vec2u(abs(coord - vec2i(8)));
    return (a.y << 12u) | (a.x << 8u) | (u32(coord.y) << 4u) | u32(coord.x);
}

fn decode_search_coord(bits: u32) -> vec2i {
    return vec2i(i32(bits & 0xfu) - RADIUS, i32((bits >> 4u) & 0xfu) - RADIUS);
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(workgroup_id) group_u: vec3u, @builtin(local_invocation_index) local_u: u32) {
    let group = vec2i(group_u.xy);
    let local = i32(local_u);
    // MapThreads.
    let search_id = vec2i(local & 3, (local >> 2u) & 15);
    let lane_block = ((local >> 1u) & 1) | (((local >> 5u) & 1) << 1u);
    let px = (group << vec2u(4u)) + search_id * vec2i(4, 1);

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
            // LoadSearchBuffer.
            let base = pixel_group + block_id * 8 + vector - vec2i(RADIUS);
            for (var id = local; id < BUF_X * BUF_Y; id += THREADS) {
                search_buffer[id] = second_packed(base + vec2i((id % BUF_X) * 4, id / BUF_X));
            }
            if (local == 0) {
                atomicStore(&wg_min, 0xffffffffu);
                atomicStore(&wg_block_sad, 0u);
            }
            workgroupBarrier();

            // CalculateQSads2.
            var sad = vec4u(0u);
            for (var dy = 0; dy < COMPARE; dy++) {
                let row = (search_id.y + dy) * BUF_X;
                let a0 = search_buffer[row + search_id.x];
                let a1 = search_buffer[row + search_id.x + 1];
                let a2 = search_buffer[row + search_id.x + 2];
                sad += qsad(a0, a1, pixels[dy][0]);
                sad += qsad(a1, a2, pixels[dy][1]);
            }
            // PrepareSadMap and SadMapMinReduction256: SAD above, the
            // candidate's coordinate (centre first on a tie) below.
            let x4 = search_id.x * 4;
            var best = (sad.x << 16u) | encode_search_coord(vec2i(x4 + 0, search_id.y));
            best = min(best, (sad.y << 16u) | encode_search_coord(vec2i(x4 + 1, search_id.y)));
            best = min(best, (sad.z << 16u) | encode_search_coord(vec2i(x4 + 2, search_id.y)));
            best = min(best, (sad.w << 16u) | encode_search_coord(vec2i(x4 + 3, search_id.y)));
            atomicMin(&wg_min, best);
            // BlockSad64: the block where it stands, unmoved.
            if (lane_block == block) {
                atomicAdd(&wg_block_sad, sad_4);
            }
            workgroupBarrier();

            let min_sad = atomicLoad(&wg_min);
            var new_vector = vector + decode_search_coord(min_sad);
            if (p.level == 0u && atomicLoad(&wg_block_sad) <= (min_sad >> 16u)) {
                new_vector = vec2i(0);
            }
            if (local == 0) {
                store_flow(of_group + block_id, new_vector);
            }
            workgroupBarrier();
        }
    }
}
