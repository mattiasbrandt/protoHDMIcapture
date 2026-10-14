#!/usr/bin/env python3
"""Hardware-free checks of present-latency.py: run `python3 -I tools/test_present_latency.py`."""

import contextlib
import importlib.util
import io
import pathlib
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("present_latency", HERE / "present-latency.py")
pl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pl)

MS = 1_000_000
REAL_OFFSET = 1_790_000_000 * 10**9  # realtime - monotonic


def meta(lines):
    """Frames 16.7 ms apart: kernel stamp, dequeued 16 ms later."""
    out = ["# sequence kernel_mono_ns dequeue_mono_ns dequeue_real_ns"]
    for i in range(lines):
        kts = 1_000_000 * MS + i * 16_700_000
        dq = kts + 16 * MS
        out.append(f"{i} {kts} {dq} {dq + REAL_OFFSET}")
    return "\n".join(out) + "\n"


def shim(clock):
    out = [f"# clock_id {clock} (CLOCK_MONOTONIC is 1)", "# surface_id commit_mono_ns presented_mono_ns refresh_ns flags"]
    for i in range(1, 20):
        dq = 1_000_000 * MS + i * 16_700_000 + 16 * MS
        commit = dq + 1 * MS
        out.append(f"7 {commit} {commit + 23 * MS} 16680567 7")  # presented 40 ms after the kernel stamp
    return "\n".join(out) + "\n"


def run(meta_text, log_text):
    with tempfile.TemporaryDirectory() as d:
        m, l = pathlib.Path(d, "meta"), pathlib.Path(d, "log")
        m.write_text(meta_text)
        l.write_text(log_text)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            pl.main(["present-latency.py", str(m), str(l)])
        return buf.getvalue()


class ClockCheck(unittest.TestCase):
    def test_monotonic_shim_log_is_measured(self):
        out = run(meta(25), shim(1))
        self.assertIn("kernel -> presented     median  40.00", out)

    def test_other_clock_is_refused(self):
        with self.assertRaises(SystemExit) as e:
            run(meta(25), shim(0))
        self.assertIn("not CLOCK_MONOTONIC", str(e.exception))

    def test_missing_clock_is_refused(self):
        log = "\n".join(l for l in shim(1).splitlines() if not l.startswith("# clock_id")) + "\n"
        with self.assertRaises(SystemExit) as e:
            run(meta(25), log)
        self.assertIn("no wp_presentation clock_id", str(e.exception))

    def test_wayland_debug_other_clock_is_refused(self):
        log = (
            "[00:00:00.000000] wp_presentation#9.clock_id(0)\n"
            "[00:00:00.000000]  -> wp_presentation#9.feedback(wl_surface#3, new id wp_presentation_feedback#20)\n"
        )
        with self.assertRaises(SystemExit) as e:
            run(meta(25), log)
        self.assertIn("not CLOCK_MONOTONIC", str(e.exception))


if __name__ == "__main__":
    unittest.main()
