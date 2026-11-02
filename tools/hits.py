"""Hook call rates from a diag build's events.log.

Usage: python3 -I tools/hits.py <events.log> [<events.log> ...]

The diagnostic build writes `<tick> diag hits <site>=<total> ...` every 5 s (when a count changed; ticks at
19.2 MHz). For each site this prints the total, the mean rate over the logged span and the highest rate between
two consecutive lines, in calls per second.
"""
import re
import sys
from pathlib import Path

TICK_HZ = 19_200_000
LINE = re.compile(r"^(\d+) diag hits ((?:\s*\w+=\d+)+)\s*$")


def samples(text):
    """(tick, {site: total}) for every hits line, in file order (only the last boot's)."""
    out = []
    for line in text.splitlines():
        if "plugin=memory-guard/" in line:
            out = []  # a new boot starts the counts again
            continue
        m = LINE.match(line)
        if m:
            counts = {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", m.group(2))}
            out.append((int(m.group(1)), counts))
    return out


def rates(points):
    """{site: (total, mean_per_s, peak_per_s)} from consecutive samples."""
    if len(points) < 2:
        return {}
    first_tick, first = points[0]
    last_tick, last = points[-1]
    span = (last_tick - first_tick) / TICK_HZ
    out = {}
    for site in last:
        peak = 0.0
        for (t0, c0), (t1, c1) in zip(points, points[1:]):
            dt = (t1 - t0) / TICK_HZ
            if dt > 0:
                peak = max(peak, (c1.get(site, 0) - c0.get(site, 0)) / dt)
        mean = (last.get(site, 0) - first.get(site, 0)) / span if span > 0 else 0.0
        out[site] = (last.get(site, 0), mean, peak)
    return out


def main(paths):
    for path in paths:
        points = samples(Path(path).read_text(errors="replace"))
        span = (points[-1][0] - points[0][0]) / TICK_HZ if len(points) > 1 else 0
        print(f"{path}: {len(points)} samples over {span:.0f} s")
        for site, (total, mean, peak) in rates(points).items():
            print(f"  {site:18} total={total:9d} mean={mean:9.0f}/s peak={peak:9.0f}/s")


if __name__ == "__main__":
    main(sys.argv[1:])
