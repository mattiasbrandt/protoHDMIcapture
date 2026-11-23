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

// GenerateSceneChangeDetectionHistogram
// (ffx_opticalflow_generate_scd_histogram.h): a 256-bin luma histogram
// for each of 3x3 strata of level 0, scrambled over ten copies in
// workgroup memory against atomic contention.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> luma: array<u32>;
@group(0) @binding(2) var<storage, read_write> histogram: array<atomic<u32>>;

const LBASE: u32 = 10u;
const HISTOGRAMS_PER_DIM: u32 = 3u;

var<workgroup> scd_buffer: array<atomic<u32>, 2560>;

fn luma_at(pos: vec2i) -> u32 {
    if (any(pos < vec2i(0)) || any(pos >= p.src)) {
        return 0u;
    }
    return luma[pos.y * p.src.x + pos.x];
}

@compute @workgroup_size(32, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u, @builtin(local_invocation_index) local: u32) {
    let div = vec2u(p.src) / HISTOGRAMS_PER_DIM;
    let nx = id.z % HISTOGRAMS_PER_DIM;
    let ny = id.z / HISTOGRAMS_PER_DIM;
    let start = vec2u(div.x * nx, div.y * ny);
    let stop = start + div;

    let offset = local * LBASE;
    for (var i = 0u; i < LBASE; i++) {
        atomicStore(&scd_buffer[offset + i], 0u);
    }
    workgroupBarrier();

    var coord = vec2u(start.x + 4u * id.x, start.y + id.y);
    if (coord.x < stop.x) {
        let scramble = local % LBASE;
        for (; coord.y < stop.y; coord.y += 128u) {
            let c = vec2i(coord);
            let colour = vec4u(
                luma_at(c), luma_at(c + vec2i(1, 0)), luma_at(c + vec2i(2, 0)), luma_at(c + vec2i(3, 0))
            ) * LBASE;
            atomicAdd(&scd_buffer[colour.x + scramble], 1u);
            atomicAdd(&scd_buffer[colour.y + scramble], 1u);
            atomicAdd(&scd_buffer[colour.z + scramble], 1u);
            atomicAdd(&scd_buffer[colour.w + scramble], 1u);
        }
    }
    workgroupBarrier();

    var value = 0u;
    for (var i = 0u; i < LBASE; i++) {
        value += atomicLoad(&scd_buffer[offset + i]);
    }
    // 256 bins a histogram: the workgroup's size.
    atomicAdd(&histogram[256u * id.z + local], value);
}
