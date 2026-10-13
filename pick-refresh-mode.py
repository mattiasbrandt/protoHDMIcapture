#!/usr/bin/env python3
"""Pick the display mode closest to a target framerate, for view.sh.

Reads `hyprctl monitors -j` on stdin, finds the *focused* monitor (no output is
hardcoded — whichever monitor you launch on is the one considered), and among
its available modes at the SAME resolution picks the one whose refresh rate is
closest to the target fps.

Prints one pipe-separated line for the shell to consume:

    name|target-lua|restore-lua|current-hz|chosen-hz

where the two "lua" fields are ready to pass to `hyprctl eval`. NOTE: it must be
`hyprctl eval`, not `hyprctl keyword` — Omarchy configures Hyprland in Lua, and
the Lua parser rejects keyword with "can't work with non-legacy parsers", doing
nothing at all rather than erroring usefully.
Exits non-zero, printing nothing, if there is no sane choice to make.

Usage:  hyprctl monitors -j | pick-refresh-mode.py 60
"""

import json
import re
import sys

MODE_RE = re.compile(r"^(\d+)x(\d+)@([\d.]+)Hz$")


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: pick-refresh-mode.py <target-fps>", file=sys.stderr)
        return 2
    try:
        target = float(sys.argv[1])
    except ValueError:
        print(f"not a number: {sys.argv[1]}", file=sys.stderr)
        return 2

    try:
        monitors = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError) as exc:
        print(f"could not parse hyprctl output: {exc}", file=sys.stderr)
        return 1

    monitor = next((m for m in monitors if m.get("focused")), None)
    if monitor is None:
        print("no focused monitor reported", file=sys.stderr)
        return 1

    width, height = monitor["width"], monitor["height"]
    current = float(monitor["refreshRate"])

    best = None
    for mode in monitor.get("availableModes", []):
        match = MODE_RE.match(mode)
        if not match:
            continue
        mode_w, mode_h, rate = (
            int(match.group(1)),
            int(match.group(2)),
            float(match.group(3)),
        )
        # Only ever change refresh, never resolution — switching resolution
        # would rearrange a multi-monitor layout.
        if (mode_w, mode_h) != (width, height):
            continue
        if best is None or abs(rate - target) < abs(best - target):
            best = rate

    if best is None:
        print("no mode at the current resolution", file=sys.stderr)
        return 1

    # Preserve position, scale and transform. Omitting transform would
    # un-rotate a rotated display; omitting position would move it.
    position = f"{monitor['x']}x{monitor['y']}"
    scale = monitor.get("scale", 1)
    name = monitor["name"]
    geometry = f"{width}x{height}"
    transform = monitor.get("transform", 0)
    extra = f", transform = {transform}" if transform else ""

    def lua(rate: float) -> str:
        return (
            f'hl.monitor({{ output = "{name}", mode = "{geometry}@{rate:g}", '
            f'position = "{position}", scale = {scale}{extra} }})'
        )

    print(f"{name}|{lua(best)}|{lua(current)}|{current:g}|{best:g}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
