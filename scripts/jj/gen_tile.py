#!/usr/bin/env python3
"""gen_tile.py X1 Z1 X2 Z2 [budget_s] [seed_artifact_dir]

Generates one tile of the big JJThunder benchmark world on a CI runner.
Boots Paper ONCE, keeps it alive for the whole budget, and re-marks all
incomplete 16x16-chunk wave blocks every round (rounds interleave so the
chunk system always has work). Completed waves are skipped via region-file
slot checks, so seeding from a previous run's artifact (world dir with
region/*.mca copied in) resumes where that run stopped.

Requires: paper.jar, eula.txt, server.properties (rcon on),
jjworld/datapacks/ with the JJThunder zip, config/paper-global.yml with the
watchdog disabled — all in the CWD.
"""
import sys, time, os, subprocess

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rcon import rcon

X1, Z1, X2, Z2 = (int(x) for x in sys.argv[1:5])
BUDGET = int(sys.argv[5]) if len(sys.argv) > 5 else 7200
SEED = sys.argv[6] if len(sys.argv) > 6 else None
REG = "jjworld/dimensions/minecraft/overworld/region"
T0 = time.time()

assert X1 % 128 == 0 and Z1 % 128 == 0, "tile bounds must be multiples of 128 chunks"

if SEED and os.path.isdir(SEED):
    os.makedirs(REG, exist_ok=True)
    n = 0
    for f in os.listdir(SEED):
        if f.endswith(".mca"):
            subprocess.run(["cp", os.path.join(SEED, f), REG])
            n += 1
    print(f"seeded {n} region files from {SEED}", flush=True)

def slots(rx, rz):
    p = f"{REG}/r.{rx}.{rz}.mca"
    try:
        d = open(p, "rb").read(4096)
    except FileNotFoundError:
        return set()
    return {i for i in range(1024) if d[i*4:i*4+4] != b"\x00\x00\x00\x00"}

def tile_chunks():
    n = 0
    for rx in range(X1 >> 5, (X2 >> 5) + 1):
        for rz in range(Z1 >> 5, (Z2 >> 5) + 1):
            n += len(slots(rx, rz))
    return n

def wave_done(cx1, cz1):
    occ = slots(cx1 >> 5, cz1 >> 5)
    for cx in range(cx1, cx1 + 16):
        for cz in range(cz1, cz1 + 16):
            if (cx & 31) + (cz & 31) * 32 not in occ:
                return False
    return True

WAVES = [(X1 + i * 16, Z1 + j * 16) for i in range(8) for j in range(8)]
TARGET = 128 * 128

print(f"tile chunks on disk: {tile_chunks()}/{TARGET}", flush=True)
proc = subprocess.Popen(
    ["java", "-Xms1G", "-Xmx8G", "-jar", "paper.jar", "nogui"],
    stdout=open("server.log", "w"), stderr=subprocess.STDOUT)
booted = False
for _ in range(420):
    try:
        if "Done (" in open("server.log").read():
            booted = True
            break
    except FileNotFoundError:
        pass
    time.sleep(1)
if not booted:
    print("BOOT_FAIL", flush=True)
    proc.kill()
    sys.exit(1)
print(f"booted t={time.time()-T0:.0f}s", flush=True)

round_no = 0
while time.time() - T0 < BUDGET:
    round_no += 1
    pending = [w for w in WAVES if not wave_done(*w)]
    print(f"[round {round_no}] pending waves={len(pending)}/64 "
          f"chunks={tile_chunks()}/{TARGET} t={time.time()-T0:.0f}s", flush=True)
    if not pending:
        break
    for wx, wz in pending:
        x1, z1 = wx * 16, wz * 16
        try:
            rcon(f"forceload add {x1} {z1} {x1+255} {z1+255}", timeout=60)
        except Exception as e:
            print(f"  mark {wx},{wz} err {e}", flush=True)
            time.sleep(3)
        time.sleep(0.25)
    print(f"  marked {len(pending)} waves t={time.time()-T0:.0f}s", flush=True)
    # let the chunk system chew; report progress every 5 min
    deadline = min(time.time() + 1200, T0 + BUDGET)
    last = -1
    while time.time() < deadline:
        time.sleep(300)
        cur = tile_chunks()
        print(f"  progress chunks={cur}/{TARGET} t={time.time()-T0:.0f}s", flush=True)
        if cur >= TARGET:
            break
    if tile_chunks() >= TARGET:
        break

print("stopping (stop persists all chunks)", flush=True)
try:
    rcon("forceload remove all", timeout=120)
except Exception:
    pass
try:
    rcon("stop", timeout=90)
except Exception:
    pass
try:
    proc.wait(timeout=240)
except Exception:
    proc.kill()
final = tile_chunks()
print(f"TILE_RESULT {'OK' if final >= TARGET else 'INCOMPLETE'} {final}/{TARGET}", flush=True)
sys.exit(0 if final >= TARGET else 1)
