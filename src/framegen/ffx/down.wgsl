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

// ComputeOpticalFlowInputPyramid: the SDK runs the single-pass downsampler
// (SPD) with SpdReduce4 = the mean of a 2x2 quad, stored truncated into
// each mip. Here one dispatch per level does the same reduction from the
// level above (no wave operations needed).

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> finer: array<u32>;
@group(0) @binding(2) var<storage, read_write> coarser: array<u32>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let pos = vec2i(id.xy);
    // p.luma: this (coarser) level; p.src: the finer level it reads.
    if (any(pos >= p.luma)) {
        return;
    }
    var sum = 0u;
    for (var y = 0; y < 2; y++) {
        for (var x = 0; x < 2; x++) {
            let at = min(pos * 2 + vec2i(x, y), p.src - 1);
            sum += finer[at.y * p.src.x + at.x];
        }
    }
    coarser[pos.y * p.luma.x + pos.x] = sum / 4u;
}
