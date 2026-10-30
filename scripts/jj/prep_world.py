#!/usr/bin/env python3
"""Prep phase for tps-bench: pregenerate the four forceload windows,
spawn the persisted entity corpus, flush, stop the server.
Run with cwd = the prep server dir."""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
from rcon import rcon

WINDOWS = [
    (-8, -8, 7, 7),
    (4, 4, 19, 19),
    (-20, -20, -5, -5),
    (-20, 4, -5, 19),
]

for (x1, z1, x2, z2) in WINDOWS:
    t0 = time.time()
    out = rcon(f"forceload add {x1} {z1} {x2} {z2}", timeout=300)
    print(f"window {x1},{z1}: {out.strip()[:70]} ({time.time()-t0:.1f}s)", flush=True)

total = int(os.environ.get("ENTITIES", "10000"))
env = {**os.environ, "ENTITY_COUNT": str(total), "R_BLOCKS": "118"}
r = subprocess.run(
    ["python3", os.path.join(os.path.dirname(os.path.abspath(__file__)), "entities20k.py")],
    env=env, capture_output=True, text=True, timeout=900,
)
print(r.stdout[-1500:], flush=True)
if r.returncode != 0:
    print("entity spawn stderr:", r.stderr[-800:], flush=True)
print("flush:", rcon("save-all flush", timeout=600).strip()[:120], flush=True)
rcon("stop", timeout=30)
time.sleep(8)
print("prep done", flush=True)
