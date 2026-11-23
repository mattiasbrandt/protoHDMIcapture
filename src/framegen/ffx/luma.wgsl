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

// PrepareLuma (ffx_opticalflow_prepare_luma.h), SDR only: Rec. 709 luma of
// the frame as it is stored (BackbufferTransferFunction 0), times 255,
// truncated. At half resolution each luma pixel is the mean of a 2x2
// quad of frame pixels (afmf-linux's half-resolution flow).

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var frame: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> luma: array<u32>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let pos = vec2i(id.xy);
    if (any(pos >= p.luma)) {
        return;
    }
    let size = vec2i(textureDimensions(frame));
    let s = i32(p.scale);
    var rgb = vec3f(0.0);
    for (var y = 0; y < s; y++) {
        for (var x = 0; x < s; x++) {
            let at = min(pos * s + vec2i(x, y), size - 1);
            rgb += textureLoad(frame, at, 0).rgb;
        }
    }
    rgb /= f32(s * s);
    let y = dot(rgb, vec3f(0.2126, 0.7152, 0.0722));
    luma[pos.y * p.luma.x + pos.x] = u32(clamp(y, 0.0, 1.0) * 255.0);
}
