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

// ComputeSCDHistogramsDivergence (ffx_opticalflow_compute_scd_divergence.h):
// each of the nine histograms, smoothed and shifted by -1, 0 and +1 bin,
// against the previous frame's, by symmetric Kullback-Leibler divergence;
// the smallest of the three shifts, averaged over the histograms, is the
// scene-change value, and over 0.45 sets bit 0 of the history.
//
// Changes for WGSL: the reductions the SDK leaves to wave lockstep below
// 32 threads take a barrier every step; and this pass writes the current
// filtered histograms to their own buffer (the host copies them to the
// previous ones afterwards, and clears the histograms) instead of
// overwriting buffers other workgroups of this dispatch may still read.

@group(0) @binding(0) var<storage, read_write> histogram: array<atomic<u32>>;
@group(0) @binding(1) var<storage, read> previous: array<f32>;
@group(0) @binding(2) var<storage, read_write> filtered_out: array<f32>;
@group(0) @binding(3) var<storage, read_write> scd: array<atomic<u32>>;

const FACTOR: f32 = 1000000.0;
const WHERE_TO_STOP: u32 = 26u; // 3 shifts x 9 histograms, less one
const HISTOGRAM_COUNT: f32 = 9.0;

var<workgroup> source_h: array<f32, 256>;
var<workgroup> filtered: array<f32, 256>;
var<workgroup> temp: array<f32, 256>;
var<workgroup> temp2: array<vec2f, 256>;

fn kernel(i: i32) -> f32 {
    switch (i) {
        case 0, 10: { return 0.0088122291; }
        case 1, 9: { return 0.027143577; }
        case 2, 8: { return 0.065114059; }
        case 3, 7: { return 0.12164907; }
        case 4, 6: { return 0.17699835; }
        default: { return 0.20056541; }
    }
}

@compute @workgroup_size(256, 1, 1)
fn main(@builtin(workgroup_id) group: vec3u, @builtin(local_invocation_index) local: u32) {
    let index = group.x * 256u + local;
    let shift = group.y;
    source_h[local] = f32(atomicLoad(&histogram[index]));
    workgroupBarrier();

    let read = i32(local) - 5;
    var val = 0.0;
    for (var k = 0; k < 11; k++) {
        val += kernel(k) * source_h[clamp(read + k, 0, 255)];
    }
    val += 1.0;

    if (shift == 0u) {
        if (local == 0u) { filtered[255] = 1.0; } else { filtered[local - 1u] = val; }
    } else if (shift == 1u) {
        filtered[local] = val;
    } else {
        if (local == 255u) { filtered[0] = 1.0; } else { filtered[local + 1u] = val; }
    }
    workgroupBarrier();

    temp[local] = filtered[local];
    workgroupBarrier();
    for (var s = 128u; s > 0u; s >>= 1u) {
        if (local < s) { temp[local] += temp[local + s]; }
        workgroupBarrier();
    }

    let current = filtered[local] / temp[0];
    let prev = previous[index];
    temp2[local] = vec2f(current * log(current / prev), prev * log(prev / current));
    workgroupBarrier();
    for (var s = 128u; s > 0u; s >>= 1u) {
        if (local < s) { temp2[local] += temp2[local + s]; }
        workgroupBarrier();
    }

    if (local == 0u) {
        let sum = temp2[0];
        let res = 1.0 - exp(-(abs(sum.x) + abs(sum.y)));
        let res_uint = u32((res / HISTOGRAM_COUNT) * FACTOR);
        atomicAdd(&scd[SCD_TEMP + shift], res_uint);
        let finished = atomicAdd(&scd[SCD_COMPLETED], 1u);
        if (finished == WHERE_TO_STOP) {
            let r0 = atomicLoad(&scd[SCD_TEMP + 0u]);
            let r1 = atomicLoad(&scd[SCD_TEMP + 1u]);
            let r2 = atomicLoad(&scd[SCD_TEMP + 2u]);
            let value = f32(min(r0, min(r1, r2))) / FACTOR;
            var history = atomicLoad(&scd[SCD_HISTORY]) << 1u;
            if (value > 0.45) {
                history |= 1u;
            }
            atomicStore(&scd[SCD_VALUE], bitcast<u32>(value));
            atomicStore(&scd[SCD_HISTORY], history);
            atomicStore(&scd[SCD_COMPLETED], 0u);
            atomicStore(&scd[SCD_TEMP + 0u], 0u);
            atomicStore(&scd[SCD_TEMP + 1u], 0u);
            atomicStore(&scd[SCD_TEMP + 2u], 0u);
        }
    }

    if (shift == 1u) {
        filtered_out[index] = current;
    }
}
