#!/usr/bin/env python3
"""TPS/MSPT sampler for the tps-bench CI job.

Each sample: query `paper tps` and `mspt`, append one line to the output
file. Every ROTATE_EVERY samples the forceload window rotates to the next
offset (unload + save the current 256 chunks; load the next window ->
disk read), and every 5th sample a timed `save-all flush` measures the
full save-cycle wall time.

Notes:
- Minecraft chat responses carry section-sign colour codes; they are
  stripped before writing (the verdict parser works on clean text).
- `forceload add <from> <to>` takes BLOCK coordinates: a 16x16-CHUNK
  window (cx1,cz1)-(cx2,cz2) must be sent as
  (cx1*16, cz1*16)-(cx2*16+15, cz2*16+15). Chunk coords silently marked
  2x2 chunks instead of 256.

Usage: tps_sample.py <out-file> <label> <samples> <interval-sec>
"""
import re
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from rcon import rcon

ROTATE_EVERY = 4
# four 16x16-CHUNK (256-chunk, the forceload cap) windows around spawn,
# in CHUNK coordinates; all pregenerated during the prep phase
WINDOWS = [
    (-8, -8, 7, 7),
    (4, 4, 19, 19),
    (-20, -20, -5, -5),
    (-20, 4, -5, 19),
]


def clean(s: str) -> str:
    s = re.sub(r"§.", "", s)
    return re.sub(r"\s+", " ", s.replace("\n", " ")).strip()


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
            t = rcon(cmd, timeout=30)
            row.append(clean(t))
        except Exception as e:
            row.append(f"{cmd} ERR {type(e).__name__}")
    if i % ROTATE_EVERY == ROTATE_EVERY - 1:
        cur = (cur + 1) % len(WINDOWS)
        x1, z1, x2, z2 = WINDOWS[cur]
        bx1, bz1, bx2, bz2 = x1 * 16, z1 * 16, x2 * 16 + 15, z2 * 16 + 15
        try:
            # time the FULL window switch: unload+save the old window, load
            # the new one (both run on the main thread with sync writes)
            t0 = time.time()
            rcon("forceload remove all", timeout=180)
            r = rcon(f"forceload add {bx1} {bz1} {bx2} {bz2}", timeout=300)
            row.append(
                f"rotate c{x1},{z1}->c{x2},{z2} {time.time()-t0:.2f}s: {clean(r)[:60]}"
            )
        except Exception as e:
            row.append(f"rotate ERR {type(e).__name__}")
    if i % 8 == 7:
        t0 = time.time()
        try:
            rcon("save-all flush", timeout=900)
            row.append(f"flush {time.time()-t0:.2f}s")
        except Exception as e:
            row.append(f"flush ERR {type(e).__name__}")
    with open(out, "a") as f:
        f.write(" | ".join(row) + "\n")
    sys.stdout.write(".")
    sys.stdout.flush()
    time.sleep(interval)
print("\nsampling done:", out, flush=True)
