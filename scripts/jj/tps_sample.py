#!/usr/bin/env python3
"""TPS/MSPT sampler for the tps-bench CI job.

Each sample: query `paper tps` and `mspt`, append one line to the output
file. Every ROTATE_EVERY samples the forceload window rotates to the next
offset (unloads the current 256 chunks -> save; loads the next window ->
disk read), and every 5th sample a timed `save-all flush` measures the
full save-cycle wall time.

Usage: tps_sample.py <out-file> <label> <samples> <interval-sec>
"""
import re
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from rcon import rcon

ROTATE_EVERY = 3
# four 16x16-chunk (256-chunk, the forceload cap) windows around spawn;
# all were pregenerated during the prep phase
WINDOWS = [
    (-8, -8, 7, 7),
    (4, 4, 19, 19),
    (-20, -20, -5, -5),
    (-20, 4, -5, 19),
]

out, label, n, interval = (
    sys.argv[1],
    sys.argv[2],
    int(sys.argv[3]),
    float(sys.argv[4]),
)

cur = 0
for i in range(n):
    row = [f"{label}", f"sample={i}"]
    for cmd in ("tps", "mspt"):
        try:
            t = rcon(cmd, timeout=20)
            row.append(re.sub(r"\s+", " ", t.replace("\n", " ")).strip())
        except Exception as e:
            row.append(f"{cmd} ERR {type(e).__name__}")
    if i % ROTATE_EVERY == ROTATE_EVERY - 1:
        cur = (cur + 1) % len(WINDOWS)
        x1, z1, x2, z2 = WINDOWS[cur]
        try:
            rcon("forceload remove all", timeout=30)
            t0 = time.time()
            r = rcon(f"forceload add {x1} {z1} {x2} {z2}", timeout=120)
            row.append(f"rotate {x1},{z1}->{x2},{z2} {time.time()-t0:.2f}s: {r.strip()[:60]}")
        except Exception as e:
            row.append(f"rotate ERR {type(e).__name__}")
    if i % 5 == 4:
        t0 = time.time()
        try:
            rcon("save-all flush", timeout=300)
            row.append(f"flush {time.time()-t0:.2f}s")
        except Exception as e:
            row.append(f"flush ERR {type(e).__name__}")
    with open(out, "a") as f:
        f.write(" | ".join(row) + "\n", )
    sys.stdout.write(".")
    sys.stdout.flush()
    time.sleep(interval)
print("\nsampling done:", out, flush=True)
