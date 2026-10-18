#!/usr/bin/env python3
"""Lost frames from sample times (#34): the card models, the old counter
(`Leaky`, policy.rs TimeGaps as of 52a7090) and the new one (`Banded`),
graded on the ticket's matrix and on the MS2130's real 10 minute trace.

    python -I tools/sim/time_gaps.py [tools/fixtures/ms2130-windows/sample-times-600s.csv]

Times are in 100 ns units, as Media Foundation gives them. No dependencies.
"""
import math
import random
import sys
from collections import deque

P = 1e7 / 60


class Leaky:
    """policy.rs TimeGaps as of 52a7090: each step's lateness carried to the
    next, leaking 1/120 a frame."""
    LEAK = 1 / 120

    def __init__(s, fps=60.0):
        s.p = 1e7 / fps
        s.prev = None
        s.late = 0.0

    def lost(s, t):
        if s.prev is None:
            s.prev = t
            return 0
        s.late = s.late * (1 - s.LEAK) + ((t - s.prev) - s.p)
        s.prev = t
        n = max(0, round(s.late / s.p))
        s.late -= n * s.p
        return int(n)


class Banded:
    """Each sample's frame number on a line through the recent ones.

    - The line: frame number against time, its intercept by least squares
      over the last WINDOW samples, its slope (the card's period) from the
      turning of the times' phase: a lost frame moves the later times by a
      whole period and so leaves their phase alone, which keeps the slope
      free of the frame numbers counted. The phase's centre is the circular
      mean of each BLOCK samples, unwrapped against where the turning so far
      puts it; the turning is the median of the slopes between centres half
      the kept ones apart (a stray centre cannot tilt it). Until MIN_BLOCKS
      centres, the least-squares slope.
    - The band: a card's times keep within a band narrower than a period
      around the line (the MS2130's wander: 0.88 of one). A sample is the
      next frame unless its time lies past the middle of the gap above the
      band, a frame further for each period past it. The band is the 5th to
      95th percentile of the last BAND samples against the line as it is
      now (its slope pivots on a point up to 4 windows back).
    - The warm-up: until the line has WARM samples, a step counts only what
      wander cannot be (past WARM_STEP periods, rounded down past 0.9 of
      the wander); then the warm-up's frame numbers are found again, the
      nearest places on a line of either slope (the phase's, the least-
      squares), the set whose band is narrowest; taken, and the frames
      beyond the steps' count said, only if that band is TIGHTER than the
      steps' numbers give (a frame missed splits the band a period apart).
    - The phase is taken in periods of the slope as it stood at each
      block's start: in FPS's, a card 0.5 % off smears the MS2130's band
      over the whole turn within a cycle.
    - The slope stays within SLOPE of FPS's period: the MS2130's 23 short
      steps make a second line 4 % off FPS fit the same times, each long
      step a frame lost; a mode is opened only within 0.5 % of FPS.

    A loss is said at the sample after it, at once (in the warm-up, up to
    WARM samples late)."""
    WINDOW = 600       # samples the intercept is fitted over: 10 s at 60 fps
    BAND = 100         # samples the band is taken over: four MS2130 cycles
    WARM = 300         # samples before the line decides: 5 s
    WARM_STEP = 2.5    # periods a warm-up step must reach to count losses
    SLOPE = 0.006      # the slope's furthest from FPS's period
    BLOCK = 50          # samples a phase centre is taken over
    PHASE_BLOCKS = 120  # centres kept for the phase's turning: 100 s
    MIN_BLOCKS = 5      # centres before the turning is used: by the warm-up's end

    def __init__(s, fps=60.0):
        s.p = 1e7 / fps
        s.pts = deque()   # (frame number, time) of the last WINDOW samples
        s.k = None
        s.prev = None
        s.ref = (0, 0.0)  # (frame number, time) the sums are taken from
        s.sums = [0, 0.0, 0.0, 0.0, 0.0]  # n, k, t, kk, kt
        s.t0 = None
        s.acc = [0.0, 0.0, 0, 0.0]  # this block: cos, sin, samples, time sum
        s.centres = deque(maxlen=s.PHASE_BLOCKS)  # (mean time, unwrapped centre)
        s.turn = None  # the centres' turning a time unit, once known
        s.frame = s.p  # the period this block's phases are taken in

    # The least-squares sums, kept relative to `ref` for their precision.
    def _sum(s, k, t, sign):
        k -= s.ref[0]
        t -= s.ref[1]
        n, sk, st, skk, skt = s.sums
        s.sums = [n + sign, sk + sign * k, st + sign * t, skk + sign * k * k, skt + sign * k * t]

    def _rebase(s):
        s.ref = s.pts[0]
        s.sums = [0, 0.0, 0.0, 0.0, 0.0]
        for k, t in s.pts:
            s._sum(k, t, 1)

    def _add(s, k, t):
        s.pts.append((k, t))
        s._sum(k, t, 1)
        if len(s.pts) > s.WINDOW:
            s._sum(*s.pts.popleft(), -1)
        if s.pts[0][0] - s.ref[0] > 4 * s.WINDOW:
            s._rebase()

    def _phase(s, t):
        """This sample into the phase's blocks. A block's phases are taken
        in periods of the slope as it stood at the block's start (`frame`),
        not FPS's: off by 0.5 %, FPS's period smears the MS2130's 0.88-wide
        band over the whole turn within one cycle, and its circular mean
        then means nothing. Each centre is turned back into FPS's periods
        for keeping, and unwrapped against where the slope puts it."""
        if s.acc[2] == 0:
            s.frame = s._slope() if s.sums[0] >= 2 else s.p
        a = 2 * math.pi * (t - s.t0) / s.frame
        s.acc[0] += math.cos(a)
        s.acc[1] += math.sin(a)
        s.acc[2] += 1
        s.acc[3] += t
        if s.acc[2] == s.BLOCK:
            tm = s.acc[3] / s.BLOCK
            # In the frame's periods, then FPS's: phi_p = phi_f - (t - t0)(1/f - 1/p).
            c = math.atan2(s.acc[1], s.acc[0]) / (2 * math.pi) - (tm - s.t0) * (1 / s.frame - 1 / s.p)
            if s.centres:
                lt, lc = s.centres[-1]
                pred = lc + (1 / s.p - 1 / s.frame) * (tm - lt)
                c = pred + ((c - pred + 0.5) % 1.0 - 0.5)
            s.centres.append((tm, c))
            s.acc = [0.0, 0.0, 0, 0.0]
            s.turn = s._turn()

    def _turn(s):
        """The centres' turning a time unit: the median of the slopes
        between centres half the kept ones apart (Theil-Sen at one lag), so
        a stray centre cannot tilt it. None until MIN_BLOCKS."""
        n = len(s.centres)
        if n < s.MIN_BLOCKS:
            return None
        lag = n // 2
        c = s.centres
        slopes = sorted((c[j + lag][1] - c[j][1]) / (c[j + lag][0] - c[j][0]) for j in range(n - lag))
        return slopes[len(slopes) // 2]

    def _slope(s):
        if s.turn is not None:
            b = 1 / (1 / s.p - s.turn)
        else:
            n, sk, st, skk, skt = s.sums
            b = (n * skt - sk * st) / (n * skk - sk * sk)
        return min(max(b, s.p * (1 - s.SLOPE)), s.p * (1 + s.SLOPE))

    def _line(s):
        """The line's time at frame number `ref`'s (relative), and its slope."""
        n, sk, st, _, _ = s.sums
        b = s._slope()
        return (st - b * sk) / n, b

    def _relabel(s):
        """The warm-up's frame numbers again from a line fitted to them,
        refitted until they settle: the frames beyond the steps' count."""
        ref_k, ref_t = s.pts[0]
        ks = [k - ref_k for k, _ in s.pts]
        ts = [t - ref_t for _, t in s.pts]
        old = list(ks)
        # Both slopes: the phase's (a free one over a few seconds takes a
        # frame lost in them for drift: one period in 300 samples is 0.33 %),
        # and the least-squares (the phase's from a few blocks is shakiest
        # at the widest drift). The numbers whose band is the narrowest.
        kinds = (True, False) if s.turn is not None else (False,)
        cands = [s._labels(old, ts, phase) for phase in kinds]
        width, ks = min(((min(s._width(c, ts, ph) for ph in kinds), c) for c in cands), key=lambda x: x[0])
        extra = ks[-1] - old[-1]
        if extra <= 0:
            return 0
        # Taken only if they explain the times better: a frame lost and
        # missed leaves the warm-up's places against its line split a period
        # apart, a band wider than the wander's, which the right numbers
        # close; numbers made up do not.
        if width > min(s._width(old, ts, ph) for ph in kinds) - s.TIGHTER * s.p:
            return 0
        s.pts = deque((k + ref_k, t + ref_t) for k, t in zip(ks, ts))
        s._rebase()
        s.k = s.pts[-1][0]
        return extra

    TIGHTER = 0.05  # periods the warm-up's band must narrow by to re-label

    def _fit(s, ks, ts, phase):
        """A line through (ks, ts): the phase's slope or the least-squares
        one, within SLOPE; (time at frame 0, slope)."""
        n = len(ks)
        sk, st = sum(ks), sum(ts)
        if phase:
            b = 1 / (1 / s.p - s.turn)
        else:
            skk = sum(k * k for k in ks)
            skt = sum(k * t for k, t in zip(ks, ts))
            b = (n * skt - sk * st) / (n * skk - sk * sk)
        b = min(max(b, s.p * (1 - s.SLOPE)), s.p * (1 + s.SLOPE))
        return (st - b * sk) / n, b

    def _labels(s, ks, ts, phase):
        """Each time's frame number again, the nearest place on the line
        fitted with the last ones, until they settle."""
        for _ in range(8):
            a, b = s._fit(ks, ts, phase)
            base = round((ts[0] - a) / b)
            new = [0]
            for t in ts[1:]:
                new.append(max(round((t - a) / b) - base, new[-1] + 1))
            if new == ks:
                break
            ks = new
        return ks

    def _width(s, ks, ts, phase):
        """The 5th-95th percentile width of the places of `ts` against a line
        fitted to them with these frame numbers."""
        a, b = s._fit(ks, ts, phase)
        n = len(ks)
        band = sorted(t - (a + b * k) for k, t in zip(ks, ts))
        return band[-1 - n // 20] - band[n // 20]

    def lost(s, t):
        if s.k is None:
            s.k, s.prev, s.t0 = 0, t, t
            s.ref = (0, t)
            s._add(0, t)
            s._phase(t)
            return 0
        k = s.k + 1
        if len(s.pts) < s.WARM:
            # Only what wander cannot be: it adds up to 0.9 of a period to a
            # step (the MS2130's long one), so rounding would count one too
            # many there. The re-label at the warm-up's end adds what this
            # leaves; it cannot take back.
            step = (t - s.prev) / s.p
            if step >= s.WARM_STEP:
                k = s.k + 1 + math.floor(step - 0.9)
        else:
            a, b = s._line()
            line = lambda kk: a + b * (kk - s.ref[0])
            r = (t - s.ref[1]) - line(k)
            band = sorted((tt - s.ref[1]) - line(kk) for kk, tt in list(s.pts)[-s.BAND:])
            lo, hi = band[len(band) // 20], band[-1 - len(band) // 20]
            edge = hi + (b - (hi - lo)) / 2
            while r > edge:
                k += 1
                r -= b
        n = k - s.k - 1
        s.k, s.prev = k, t
        s._add(k, t)
        s._phase(t)
        if len(s.pts) == s.WARM:
            n += s._relabel()
        return n


def card(n, drift_pct, shape, rng):
    """The ticket's models. flat: on the grid; jit: 5 % of a period of noise
    per time; saw: 16.03 ms 24 times then 31.4 ms (mean scaled to the
    period). And ms24, the MS2130 as the real trace has it: 23 steps of
    16.028 ms, one of 31.285 (the medians of its clean cycles), mean
    scaled to the period. drift_pct > 0 is a slow card."""
    m = (24 * 160300 + 314000) / 25
    m24 = (23 * 160276.5 + 312849) / 24
    ph = rng.randrange(25)
    t = rng.uniform(0, 1e6)
    out = [t]
    for i in range(1, n):
        j = (i + ph) % 25
        if shape == 'saw':
            d = (314000 if j == 0 else 160300) / m * P
        elif shape == 'ms24':
            d = (312849 if (i + ph) % 24 == 0 else 160276.5) / m24 * P
        else:
            d = P
        t += d * (1 + drift_pct / 100)
        out.append(t)
    if shape == 'jit':
        out = [x + rng.gauss(0, P * 0.05) for x in out]
    return out


def drop(times, losses):
    """Each (index, frames) loses that many frames before times[index]."""
    ts = list(times)
    mean = (ts[-1] - ts[0]) / (len(ts) - 1)
    for k, b in losses:
        ts = ts[:k] + [x + b * mean for x in ts[k:]]
    return ts


def total(counter, times):
    c = counter()
    return sum(c.lost(t) for t in times)


def matrix(counter, seed=7, shapes=('flat', 'saw', 'jit')):
    rng = random.Random(seed)
    phantom = wrong = runs = 0
    for shape in shapes:
        for d in [-0.5, -0.3, -0.13, -0.05, 0, 0.05, 0.13, 0.3, 0.5]:
            for _ in range(3):
                base = card(5000, d, shape, rng)
                phantom += total(counter, base)
                for _ in range(8):
                    ks = sorted(rng.sample(range(200, 4800), rng.randint(1, 4)))
                    bs = [rng.randint(1, 3) for _ in ks]
                    runs += 1
                    wrong += total(counter, drop(base, list(zip(ks, bs)))) != sum(bs)
    return phantom, wrong, runs


def trace(path):
    """The fixture's device times, from its steps."""
    times, t = [], 0
    for line in open(path):
        if line.startswith('device_step'):
            times.append(0)
        elif line[:1].isdigit():
            t += int(line.split(',')[0])
            times.append(t)
    return times


def truth(times):
    """The frames lost over a trace by its cadence: the MS2130 delivers 24
    frames a cycle (23 short steps, one long), so the period is the mean of
    the cycles with no step over 1.5 periods but the closing long one; the
    frames sent are the span over it. (lost, its 2-sigma, the rate in Hz)"""
    steps = [b - a for a, b in zip(times, times[1:])]
    longs = [i for i, s in enumerate(steps) if s >= 1.7 * P]
    cyc = [sum(steps[a + 1:b + 1]) / 24 for a, b in zip(longs, longs[1:])
           if b - a == 24 and all(s < 1.5 * P for s in steps[a + 1:b]) and steps[b] < 2.2 * P]
    m = sum(cyc) / len(cyc)
    sd = (sum((c - m) ** 2 for c in cyc) / len(cyc)) ** 0.5
    span = times[-1] - times[0]
    return span / m + 1 - len(times), 2 * span / m ** 2 * sd / len(cyc) ** 0.5, 1e7 / m


if __name__ == '__main__':
    for c in (Leaky, Banded):
        print('%s: phantom losses %d, runs counted wrong %d of %d' % ((c.__name__,) + matrix(c)))
        print('  the MS2130 as traced (ms24): phantom losses %d, runs counted wrong %d of %d' % matrix(c, shapes=('ms24',)))
    if len(sys.argv) > 1:
        times = trace(sys.argv[1])
        lost, err, rate = truth(times)
        print('trace: %d samples; the card at %.5f Hz by its cadence; lost %.1f +- %.1f' % (len(times), rate, lost, err))
        for c in (Leaky, Banded):
            print('  %s counts %d' % (c.__name__, total(c, times)))
