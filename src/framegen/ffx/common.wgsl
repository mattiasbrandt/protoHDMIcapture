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

// Shared by every pass: the parameters and the packed-luma helpers of
// ffx_opticalflow_common.h and ffx_opticalflow_callbacks_*.h.

// One pass's view of the pyramid (set per level by the host).
struct Params {
    // This level's luma, in pixels.
    luma: vec2i,
    // This level's flow: one vector per 8x8 block of its luma.
    of_size: vec2i,
    // The next finer level's flow (the scale pass writes it).
    next_of: vec2i,
    // Level 0's luma: the flow's input resolution (the SDK's DisplaySize).
    src: vec2i,
    level: u32,
    levels: u32,
    // The luma pass: frame pixels per luma pixel (1 or 2).
    scale: u32,
    _pad: u32,
}

// The scene-change slots (SCD_OUTPUT_*), after three accumulators.
const SCD_TEMP: u32 = 0u;
const SCD_VALUE: u32 = 3u;
const SCD_HISTORY: u32 = 4u;
const SCD_COMPLETED: u32 = 5u;

// GetPackedLuma: four luma bytes from x, the edge pixel repeated past the
// image's left and right.
fn packed_luma(width: i32, x: i32, l0: u32, l1: u32, l2: u32, l3: u32) -> u32 {
    var v = l0 | (l1 << 8u) | (l2 << 16u) | (l3 << 24u);
    if (x < 0) {
        let fill = v & 0xffu;
        if (x <= -1) { v = (v << 8u) | fill; }
        if (x <= -2) { v = (v << 8u) | fill; }
        if (x <= -3) { v = (v << 8u) | fill; }
    } else if (x > width - 4) {
        let fill = v & 0xff000000u;
        if (x >= width - 3) { v = (v >> 8u) | fill; }
        if (x >= width - 2) { v = (v >> 8u) | fill; }
        if (x >= width - 1) { v = (v >> 8u) | fill; }
    }
    return v;
}

// Sad: the sum of absolute differences of four packed bytes.
fn sad4(a: u32, b: u32) -> u32 {
    let x = vec4i(vec4u(a, a >> 8u, a >> 16u, a >> 24u) & vec4u(0xffu));
    let y = vec4i(vec4u(b, b >> 8u, b >> 16u, b >> 24u) & vec4u(0xffu));
    let d = abs(x - y);
    return u32(d.x + d.y + d.z + d.w);
}

// QSad: b against a0:a1 shifted by 0, 1, 2 and 3 bytes.
fn qsad(a0_in: u32, a1_in: u32, b: u32) -> vec4u {
    var a0 = a0_in;
    var a1 = a1_in;
    var s: vec4u;
    s.x = sad4(a0, b);
    a0 = (a0 >> 8u) | ((a1 & 0xffu) << 24u);
    a1 = a1 >> 8u;
    s.y = sad4(a0, b);
    a0 = (a0 >> 8u) | ((a1 & 0xffu) << 24u);
    a1 = a1 >> 8u;
    s.z = sad4(a0, b);
    a0 = (a0 >> 8u) | ((a1 & 0xffu) << 24u);
    s.w = sad4(a0, b);
    return s;
}

// A flow vector, or zero outside the level (an image load out of bounds
// reads zero in the SDK's backends).
fn in_flow(pos: vec2i, size: vec2i) -> bool {
    return all(pos >= vec2i(0)) && all(pos < size);
}
