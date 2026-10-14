#!/usr/bin/env python3
"""Kernel timestamp -> presented, for any Wayland player, from outside.

Inputs:
  META   `hdmipoc meta --out FILE`: one line per captured frame (sequence,
         kernel timestamp, dequeued; CLOCK_MONOTONIC ns, plus CLOCK_REALTIME
         ns of the dequeue to tie the two clocks together).
  LOG    one of:
         - the player's stderr with WAYLAND_DEBUG=client, when the player
           (or Mesa's Vulkan WSI under Fifo) asks wp_presentation for
           feedback right before each commit. More than one may ask (mpv
           asks for some frames too); the wp_presentation object asking
           most often is taken. WAYLAND_DEBUG stamps are wall-clock time
           of day in UTC (checked: 13:27 logged at 15:27 CEST); the META
           file's two clocks turn them into CLOCK_MONOTONIC.
         - $PRESENT_SHIM_LOG from tools/present-shim.so (LD_PRELOAD), for
           players that ask for no feedback themselves (EGL apps,
           waylandsink): feedback on every commit, times already on
           CLOCK_MONOTONIC. The surface that commits most is the picture.

Each present is matched to the newest frame the card had finished
(metadata dequeued) before the request: the player cannot show a frame it
does not have yet, so this is a LOWER bound on the player's latency; a
player holding an older frame is slower than this says. Only a frame's
first present counts; presents of the same newest frame again are redraws.

Usage: present-latency.py META LOG [--window SECONDS]
"""

import bisect
import datetime
import re
import statistics
import sys

REQ = re.compile(
    r"^\[(\d\d):(\d\d):(\d\d)\.(\d{6})\] (?:\{[^}]*\})?\s+-> "
    r"wp_presentation#(\d+)\.feedback\(wl_surface#\d+, new id wp_presentation_feedback#(\d+)\)"
)
CLOCK = re.compile(r"wp_presentation#\d+\.clock_id\((\d+)\)")
SHIM_CLOCK = re.compile(r"^# clock_id (\d+)")
CLOCK_MONOTONIC = 1  # Linux; the meta file's capture times are on it

EVT = re.compile(
    r"^\[[\d:.]+\] (?:\{[^}]*\} )?"
    r"wp_presentation_feedback#(\d+)\.(presented|discarded)\((.*)\)"
)


def pct(xs, q):
    return xs[round((len(xs) - 1) * q)]


def describe(xs):
    xs = sorted(xs)
    return (
        f"median {statistics.median(xs):6.2f}  p99 {pct(xs, 0.99):6.2f}  "
        f"min {xs[0]:6.2f}  p1 {pct(xs, 0.01):6.2f}  max {xs[-1]:6.2f} ms  (n={len(xs)})"
    )


def read_meta(path):
    frames, offsets = [], []  # (dequeue_mono_ns, kernel_ns, sequence); realtime - monotonic
    with open(path) as f:
        for line in f:
            fields = line.split()
            if line.startswith("#") or len(fields) != 4:  # a header, or a line still being written
                continue
            seq, kts, mono, real = map(int, fields)
            if kts == 0:  # empty buffers after the video stream stops
                continue
            frames.append((mono, kts, seq))
            offsets.append(real - mono)
    if not frames:
        sys.exit("no frames in the meta file")
    frames.sort()
    return frames, statistics.median(offsets), (max(offsets) - min(offsets)) / 1e6


def is_shim(path):
    with open(path, errors="replace") as f:
        return any(line.startswith(("# surface_id", "# clock_id", "# the compositor")) for line in f)


def require_monotonic(clocks, where):
    """Presented times are only comparable with capture times on CLOCK_MONOTONIC."""
    if not clocks:
        sys.exit(f"{where}: no wp_presentation clock_id, so presented times cannot be trusted")
    other = sorted(c for c in clocks if c != CLOCK_MONOTONIC)
    if other:
        sys.exit(f"{where}: presentation clock {other} is not CLOCK_MONOTONIC ({CLOCK_MONOTONIC}); "
                 "the latencies would compare different clocks")


def read_shim(path):
    """[(request_mono_ns, presented_mono_ns or None)] for the busiest surface."""
    rows, commits, clocks = [], {}, set()
    with open(path) as f:
        for line in f:
            if line.startswith("#"):
                print(line.strip())
                m = SHIM_CLOCK.match(line)
                if m:
                    clocks.add(int(m.group(1)))
                continue
            p = line.split()
            if len(p) == 3 and p[2] == "discarded":
                rows.append((p[0], int(p[1]), None))
            elif len(p) == 5:
                rows.append((p[0], int(p[1]), int(p[2])))
            else:
                continue
            commits[p[0]] = commits.get(p[0], 0) + 1
    require_monotonic(clocks, path)
    if not rows:
        sys.exit("no commits in the shim log")
    surface = max(commits, key=commits.get)
    print(f"presents per surface: {commits}; using wl_surface#{surface}")
    return sorted((req, at) for s, req, at in rows if s == surface)


def read_wayland_debug(path, day, offset):
    """[(request_mono_ns, presented_mono_ns or None)] for the busiest requester."""
    asks, clocks = {}, set()
    with open(path, errors="replace") as f:
        for line in f:
            m = REQ.match(line)
            if m:
                asks[m.group(5)] = asks.get(m.group(5), 0) + 1
            m = CLOCK.search(line)
            if m:
                clocks.add(int(m.group(1)))
    # Every wp_presentation object announces the compositor's one clock.
    require_monotonic(clocks, path)
    if not asks:
        sys.exit("no wp_presentation feedback requests in the Wayland log; use present-shim.so")
    chosen = max(asks, key=asks.get)
    print(f"feedback requests per wp_presentation object: {asks}; using #{chosen}")
    pending, out = {}, []  # feedback id -> request monotonic ns
    with open(path, errors="replace") as f:
        for line in f:
            m = REQ.match(line)
            if m:
                h, mi, s, us, obj, fid = m.groups()
                if obj != chosen:
                    pending.pop(fid, None)
                    continue
                wall = datetime.datetime.combine(
                    day, datetime.time(int(h), int(mi), int(s), int(us)), tzinfo=datetime.timezone.utc
                )
                pending[fid] = int(wall.timestamp() * 1e9) - offset
                continue
            m = EVT.match(line)
            if not m:
                continue
            fid, kind, args = m.groups()
            req = pending.pop(fid, None)
            if req is None:
                continue
            if kind == "discarded":
                out.append((req, None))
                continue
            hi, lo, nsec = (int(a) for a in args.split(",")[:3])
            out.append((req, ((hi << 32) | lo) * 1_000_000_000 + nsec))
    return out


def report(frames, offset_spread, presents, window):
    dq = [f[0] for f in frames]
    to_present, kernel_to_present, windows = [], [], []
    presented = discarded = no_frame = repeats = 0
    last = -1  # index of the newest frame the previous present could show
    for req, at in presents:
        if at is None:
            discarded += 1
            continue
        i = bisect.bisect_right(dq, req) - 1
        if i < 0:
            no_frame += 1
            continue
        if i == last:
            repeats += 1
            continue
        last = i
        mono, kts, _ = frames[i]
        presented += 1
        to_present.append((at - mono) / 1e6)
        kernel_to_present.append((at - kts) / 1e6)
        windows.append((at, (at - kts) / 1e6))

    gaps = sum(1 for a, b in zip(frames, frames[1:]) if b[2] - a[2] != 1)
    print(f"clock offset spread over the run: {offset_spread:.3f} ms (must be well under 1)")
    print(f"frames {len(frames)}, sequence gaps {gaps}; presents of a new frame {presented}, "
          f"redraws of the same frame {repeats}, discarded {discarded}, before any frame {no_frame}")
    if not to_present:
        sys.exit("no presents matched")
    print(f"kernel -> presented     {describe(kernel_to_present)}")
    print(f"frame done -> presented {describe(to_present)}")
    start = windows[0][0]
    bucket, edge = [], start + window * 1e9
    for at, k in windows:
        while at >= edge:
            if bucket:
                print(f"  {(edge - start) / 1e9:6.0f} s  kernel -> presented median "
                      f"{statistics.median(bucket):6.2f}  p99 {pct(sorted(bucket), 0.99):6.2f} ms")
            bucket, edge = [], edge + window * 1e9
        bucket.append(k)


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    window = float(argv[argv.index("--window") + 1]) if "--window" in argv else 10.0
    frames, offset, spread = read_meta(argv[1])
    if is_shim(argv[2]):
        presents = read_shim(argv[2])
    else:
        day = datetime.datetime.fromtimestamp((frames[0][0] + offset) / 1e9, datetime.timezone.utc).date()
        presents = read_wayland_debug(argv[2], day, offset)
    report(frames, spread, presents, window)


if __name__ == "__main__":
    main(sys.argv)
