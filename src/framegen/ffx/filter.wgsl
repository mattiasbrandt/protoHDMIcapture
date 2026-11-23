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

// FilterOpticalFlow (ffx_opticalflow_filter_optical_flow_v5.h): each
// vector becomes the one of its 3x3 neighbourhood closest to all the
// others (a vector median); outside the level reads zero.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> searched: array<vec2i>;
@group(0) @binding(2) var<storage, read_write> filtered: array<vec2i>;

fn load(pos: vec2i) -> vec2i {
    if (!in_flow(pos, p.of_size)) {
        return vec2i(0);
    }
    return searched[pos.y * p.of_size.x + pos.x];
}

@compute @workgroup_size(16, 4, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let pos = vec2i(id.xy);
    if (!in_flow(pos, p.of_size)) {
        return;
    }
    var mv: array<vec2i, 9>;
    var idx = 0;
    for (var xx = -1; xx < 2; xx++) {
        for (var yy = -1; yy < 2; yy++) {
            mv[idx] = load(pos + vec2i(xx, yy));
            idx++;
        }
    }
    var ret = 0xffffffffu;
    for (var i = 0; i < 9; i++) {
        var tmp = 0u;
        for (var j = 0; j < 9; j++) {
            let d = mv[i] - mv[j];
            tmp += u32(d.x * d.x) + u32(d.y * d.y);
        }
        ret = min((tmp << 4u) | u32(i), ret);
    }
    filtered[pos.y * p.of_size.x + pos.x] = mv[ret & 0xfu];
}
