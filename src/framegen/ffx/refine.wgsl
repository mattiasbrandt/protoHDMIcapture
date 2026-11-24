// protoHDMIcapture (#73), ffx's finer flow: each 4x4 block of the
// level-0 luma takes the best of the 8x8 flow's vectors around it (its own
// block's and its eight neighbours'), then the best one-pixel step from
// that, each judged by the SAD of a 6x6 window (the 4x4 block and a
// one-pixel border) against the previous luma. An 8x8 block that straddles
// a motion edge holds one vector; its 4x4 quarters can take the one their
// own side moves with. Vectors as the search's: previous = current + v.

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> current: array<u32>;
@group(0) @binding(2) var<storage, read> previous: array<u32>;
@group(0) @binding(3) var<storage, read> coarse: array<vec2i>;
@group(0) @binding(4) var<storage, read_write> fine: array<vec2i>;

fn index(pos: vec2i) -> i32 {
    let at = clamp(pos, vec2i(0), p.luma - 1);
    return at.y * p.luma.x + at.x;
}

fn coarse_at(b: vec2i) -> vec2i {
    let at = clamp(b, vec2i(0), p.of_size - 1);
    return coarse[at.y * p.of_size.x + at.x];
}

fn window_sad(origin: vec2i, v: vec2i) -> u32 {
    var sad = 0;
    for (var y = -1; y < 5; y++) {
        for (var x = -1; x < 5; x++) {
            let q = origin + vec2i(x, y);
            sad += abs(i32(current[index(q)]) - i32(previous[index(q + v)]));
        }
    }
    return u32(sad);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3u) {
    let b = vec2i(id.xy);
    // next_of: this pass's output size, one vector per 4x4 block.
    if (any(b >= p.next_of)) {
        return;
    }
    let origin = b * 4;
    let own = b / 2;
    var best_v = coarse_at(own);
    var best = window_sad(origin, best_v);
    for (var dy = -1; dy < 2; dy++) {
        for (var dx = -1; dx < 2; dx++) {
            if (dx == 0 && dy == 0) {
                continue;
            }
            let v = coarse_at(own + vec2i(dx, dy));
            let sad = window_sad(origin, v);
            if (sad < best) {
                best = sad;
                best_v = v;
            }
        }
    }
    let start = best_v;
    for (var dy = -1; dy < 2; dy++) {
        for (var dx = -1; dx < 2; dx++) {
            if (dx == 0 && dy == 0) {
                continue;
            }
            let v = start + vec2i(dx, dy);
            let sad = window_sad(origin, v);
            if (sad < best) {
                best = sad;
                best_v = v;
            }
        }
    }
    fine[b.y * p.next_of.x + b.x] = best_v;
}
