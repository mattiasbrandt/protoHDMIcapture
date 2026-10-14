#!/usr/bin/env python3
"""Kernel timestamp -> presented, for any Vulkan player on Wayland, from outside.

Inputs:
  meta   `hdmipoc meta --out FILE`: one line per captured frame (sequence,
         kernel timestamp, dequeued; CLOCK_MONOTONIC ns, plus CLOCK_REALTIME
         ns of the dequeue to tie the two clocks together).
  wl     the player's stderr with WAYLAND_DEBUG=client.

Feedback is asked for right before each commit: by Mesa's Vulkan WSI on its
own queue under Fifo (mpv), by the player itself otherwise (the PoC). More
than one may ask (mpv asks for some frames too); the wp_presentation object
with the most requests is taken as the one asking every present. Each such
request is matched to the newest frame the card had
finished (metadata dequeued) before the request: the player cannot show a
frame it does not have yet, so this is a LOWER bound on the player's
latency; a player holding an older frame is slower than this says.

The WAYLAND_DEBUG time is the wall-clock time of day in UTC (checked
against file times: 13:27 logged at 15:27 CEST); it is turned into
CLOCK_MONOTONIC with the offset measured in the meta file.

Usage: present-latency.py META WL [--window SECONDS]
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


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    window = float(argv[argv.index("--window") + 1]) if "--window" in argv else 10.0

    frames = []  # (dequeue_mono_ns, kernel_ns, sequence)
    offsets = []  # realtime - monotonic, ns
    with open(argv[1]) as f:
        for line in f:
            if line.startswith("#"):
                continue
            fields = line.split()
            if len(fields) != 4:  # a line still being written
                continue
            seq, kts, mono, real = map(int, fields)
            if kts == 0:  # empty buffers after the video stream stops
                continue
            frames.append((mono, kts, seq))
            offsets.append(real - mono)
    if not frames:
        sys.exit("no frames in the meta file")
    frames.sort()
    dq = [f[0] for f in frames]
    offset = statistics.median(offsets)
    spread = (max(offsets) - min(offsets)) / 1e6
    # The UTC date of the run, for the time-of-day stamps.
    day = datetime.datetime.fromtimestamp((frames[0][0] + offset) / 1e9, datetime.timezone.utc).date()

    # Who asks for feedback on every present: the object asking most often.
    asks = {}
    with open(argv[2], errors="replace") as f:
        for line in f:
            m = REQ.match(line)
            if m:
                asks[m.group(5)] = asks.get(m.group(5), 0) + 1
    if not asks:
        sys.exit("no wp_presentation feedback requests in the Wayland log")
    chosen = max(asks, key=asks.get)
    print(f"feedback requests per wp_presentation object: {asks}; using #{chosen}")

    pending = {}  # feedback id -> request monotonic ns
    to_present, kernel_to_present = [], []
    windows = []
    presented = discarded = no_frame = repeats = 0
    last = -1  # index of the newest frame the previous present could show
    gaps = sum(1 for a, b in zip(frames, frames[1:]) if b[2] - a[2] != 1)
    with open(argv[2], errors="replace") as f:
        for line in f:
            m = REQ.match(line)
            if m:
                h, mi, s, us, obj, fid = m.groups()
                if obj != chosen:
                    pending.pop(fid, None)
                    continue
                wall = datetime.datetime.combine(day, datetime.time(int(h), int(mi), int(s), int(us)),
                                             tzinfo=datetime.timezone.utc)
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
                discarded += 1
                continue
            hi, lo, nsec = (int(a) for a in args.split(",")[:3])
            at = ((hi << 32) | lo) * 1_000_000_000 + nsec
            i = bisect.bisect_right(dq, req) - 1
            if i < 0:
                no_frame += 1
                continue
            # The same newest frame as the last present: a redraw (OSD, a
            # refresh with no new frame), not a new frame reaching the screen.
            if i == last:
                repeats += 1
                continue
            last = i
            mono, kts, _ = frames[i]
            presented += 1
            to_present.append((at - mono) / 1e6)
            kernel_to_present.append((at - kts) / 1e6)
            windows.append((at, (at - kts) / 1e6))

    print(f"clock offset spread over the run: {spread:.3f} ms (must be well under 1)")
    print(f"frames {len(frames)}, sequence gaps {gaps}; presents of a new frame {presented}, "
          f"redraws of the same frame {repeats}, discarded {discarded}, before any frame {no_frame}")
    if not to_present:
        sys.exit("no presents matched")
    print(f"kernel -> presented   {describe(kernel_to_present)}")
    print(f"frame done -> presented {describe(to_present)}")
    start = windows[0][0]
    bucket, edge = [], start + window * 1e9
    for at, k in windows:
        while at >= edge:
            if bucket:
                print(f"  {(edge - start) / 1e9:6.0f} s  kernel -> presented median {statistics.median(bucket):6.2f}  "
                      f"p99 {pct(sorted(bucket), 0.99):6.2f} ms")
            bucket, edge = [], edge + window * 1e9
        bucket.append(k)


if __name__ == "__main__":
    main(sys.argv)
