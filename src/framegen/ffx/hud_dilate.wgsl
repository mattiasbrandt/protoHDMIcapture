// openfg's ui_mask_dilate.comp (MIT; the notice is in hud_mask.wgsl), for
// protoHDMIcapture (#73): each block of the HUD mask becomes the largest of
// its 3x3 neighbourhood.

struct Hud {
    size: vec2i,
    blocks: vec2i,
    frames: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> h: Hud;
@group(0) @binding(1) var<storage, read> mask: array<u32>;
@group(0) @binding(2) var<storage, read_write> dilated: array<u32>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let b = vec2i(id.xy);
    if (any(b >= h.blocks)) {
        return;
    }
    var m = 0u;
    for (var y = -1; y < 2; y++) {
        for (var x = -1; x < 2; x++) {
            let q = clamp(b + vec2i(x, y), vec2i(0), h.blocks - 1);
            m = max(m, mask[q.y * h.blocks.x + q.x]);
        }
    }
    dilated[b.y * h.blocks.x + b.x] = m;
}
