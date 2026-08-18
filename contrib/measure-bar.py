#!/usr/bin/env python3
"""Measure a cosmicbar binary against waybar on the live session.

    contrib/measure-bar.py target/release/cosmicbar mine 150

Starts the given binary (replacing any bar already started from the same path),
waits for it to settle, then samples CPU and RSS over the window. Any waybar
running beside it is measured over the same window, on the same desktop, with
the same monitors and the same modules on screen - the only comparison that
means anything.

CPU counts reaped children too, which is how a bar built out of `exec` scripts
shows its real cost: the forks are the work.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time

if len(sys.argv) != 4:
    sys.exit(__doc__)

BIN, LABEL, WINDOW = sys.argv[1], sys.argv[2], float(sys.argv[3])
BIN = os.path.abspath(BIN)
HZ = os.sysconf("SC_CLK_TCK")
LOG = f"/tmp/measure-bar-{LABEL}.log"
# Long enough for the first NVML init, wifi scan and BlueZ tree walk to be over,
# so the window measures a bar at rest rather than a bar starting up.
SETTLE = 12.0


def cpu_seconds(pid: int) -> float:
    fields = open(f"/proc/{pid}/stat").read().rsplit(") ", 1)[1].split()
    # utime, stime, cutime, cstime.
    return sum(int(fields[i]) for i in (11, 12, 13, 14)) / HZ


def rss_kb(pid: int) -> int:
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1])
    return 0


def pid_of(pattern: str, exact: bool = False) -> int | None:
    args = ["pgrep", "-x" if exact else "-f", pattern]
    out = subprocess.run(args, capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else None


def main() -> None:
    subprocess.run(["pkill", "-f", f"^{BIN}$"])
    time.sleep(1)
    display = os.environ.get("WAYLAND_DISPLAY", "wayland-1")
    stale = f"{os.environ.get('XDG_RUNTIME_DIR', '/tmp')}/cosmicbar-{display}.sock"
    if os.path.exists(stale):
        os.unlink(stale)

    # `debug` because the count of bar redraws is the interesting number beside
    # the CPU: a cheap bar is one that is not drawing.
    env = dict(os.environ, RUST_LOG="warn,cosmicbar=debug")
    with open(LOG, "w") as log:
        subprocess.Popen(
            [BIN], stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True
        )
    time.sleep(SETTLE)

    bar = pid_of(f"^{BIN}$")
    if bar is None:
        sys.exit(f"{LABEL}: bar did not start; see {LOG}")
    way = pid_of("waybar", exact=True)

    def redraws() -> int:
        return sum(1 for line in open(LOG) if " update " in line)

    before = redraws()
    start = {"bar": cpu_seconds(bar)}
    if way:
        start["waybar"] = cpu_seconds(way)
    t0 = time.time()
    time.sleep(WINDOW)
    elapsed = time.time() - t0

    messages = redraws() - before
    print(f"== {LABEL} over {elapsed:.0f}s")
    print(f"cosmicbar cpu   {100 * (cpu_seconds(bar) - start['bar']) / elapsed:.3f}%")
    print(f"cosmicbar rss   {rss_kb(bar)} kB")
    print(f"bar redraws     {messages} ({messages / elapsed * 60:.0f}/min)")
    if way:
        print(f"waybar cpu      {100 * (cpu_seconds(way) - start['waybar']) / elapsed:.3f}%")
        print(f"waybar rss      {rss_kb(way)} kB")
    else:
        print("waybar          not running; start it to compare")


if __name__ == "__main__":
    main()
