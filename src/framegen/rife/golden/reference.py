"""Reference output of RIFE v4.6 (rife-ncnn-vulkan's converted model) run by
ncnn's own CPU runtime, with rife.Warp ported from rife-ncnn-vulkan's
src/warp.cpp (its CPU forward, including the clamp-then-alpha quirk).
Writes in0.rgb, in1.rgb (u8 HWC), out.f32 (f32 CHW, 0..1 scale, timestep 0.5)."""
import sys, numpy as np, ncnn
model, outdir, W, H = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])

class Warp(ncnn.Layer):
    def __init__(self):
        super().__init__(); self.one_blob_only = False; self.support_inplace = False
    def forward(self, bottom_blobs, top_blobs, opt):
        img = np.array(bottom_blobs[0]); flow = np.array(bottom_blobs[1])
        c, h, w = img.shape
        ys, xs = np.mgrid[0:h, 0:w].astype(np.float32)
        sx = xs + flow[0]; sy = ys + flow[1]
        x0 = np.floor(sx).astype(np.int64); y0 = np.floor(sy).astype(np.int64)
        x1 = x0 + 1; y1 = y0 + 1
        x0 = np.clip(x0, 0, w - 1); y0 = np.clip(y0, 0, h - 1)
        x1 = np.clip(x1, 0, w - 1); y1 = np.clip(y1, 0, h - 1)
        a = (sx - x0).astype(np.float32); b = (sy - y0).astype(np.float32)
        v0 = img[:, y0, x0]; v1 = img[:, y0, x1]; v2 = img[:, y1, x0]; v3 = img[:, y1, x1]
        v4 = v0 * (1 - a) + v1 * a; v5 = v2 * (1 - a) + v3 * a
        out = (v4 * (1 - b) + v5 * b).astype(np.float32)
        t = top_blobs[0]; t.create(w, h, c)
        np.array(t, copy=False)[...] = out
        return 0

_alive = []
def warp_creator():
    l = Warp(); _alive.append(l); return l
def warp_destroyer(l):
    pass

net = ncnn.Net()
net.opt.use_vulkan_compute = False
net.register_custom_layer("rife.Warp", warp_creator, warp_destroyer)
import os
assert os.path.isfile(model + "/flownet.param") and os.path.isfile(model + "/flownet.bin")
assert net.load_param(model + "/flownet.param") == 0
assert net.load_model(model + "/flownet.bin") == 0

rng = np.random.default_rng(74)
yy, xx = np.mgrid[0:H, 0:W]
def picture(shift):
    # Gradients, a textured square that moves, and a little noise.
    r = (xx * 255 // max(W - 1, 1)); g = (yy * 255 // max(H - 1, 1)); b = ((xx + yy) * 4) % 256
    img = np.stack([r, g, b], -1).astype(np.float32)
    x0 = 20 + shift; y0 = 16 + shift // 2
    sq = (xx >= x0) & (xx < x0 + 24) & (yy >= y0) & (yy < y0 + 20)
    tex = ((xx - x0) // 4 + (yy - y0) // 4) % 2 * 180 + 40
    for c in range(3): img[..., c] = np.where(sq, tex + 10 * c, img[..., c])
    return img.astype(np.uint8)
a = picture(0); bimg = picture(8)
noise = rng.integers(-3, 4, size=a.shape)
a = np.clip(a.astype(int) + noise, 0, 255).astype(np.uint8)
bimg = np.clip(bimg.astype(int) + noise, 0, 255).astype(np.uint8)
a.tofile(outdir + "/in0.rgb"); bimg.tofile(outdir + "/in1.rgb")
t0 = np.ascontiguousarray(a.transpose(2, 0, 1).astype(np.float32) / 255.0)
t1 = np.ascontiguousarray(bimg.transpose(2, 0, 1).astype(np.float32) / 255.0)
ts = np.full((1, H, W), 0.5, np.float32)
ex = net.create_extractor()
assert t0.shape == (3, H, W) and ts.shape == (1, H, W)
assert ex.input("in0", ncnn.Mat(t0)) == 0 and ex.input("in1", ncnn.Mat(t1)) == 0 and ex.input("in2", ncnn.Mat(ts)) == 0
ret, out = ex.extract("out0")
assert ret == 0, ret
o = np.array(out).astype(np.float32)
print(o.shape, o.min(), o.max(), o.mean(), flush=True)
o.tofile(outdir + "/out.f32")
