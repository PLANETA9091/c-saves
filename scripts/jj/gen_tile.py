#!/usr/bin/env python3
"""gen_tile.py X1 Z1 X2 Z2 [chunk coords, multiples of 128]

Generates one tile of the big JJThunder benchmark world on a CI runner:
boots Paper, marks all incomplete 16x16-chunk wave blocks inside the tile,
waits for the chunk count on disk to stabilize, stops (stop persists),
repeats until the tile is complete. Each iteration re-boots; completed
waves are skipped via region-file slot checks.

The paper.jar / datapack / world layout must already exist in the CWD
(server.properties with rcon enabled, level-name=jjworld,
jjworld/datapacks/JJThunder zip).
"""
import sys, time, os, subprocess

sys.path.insert(0, os.path.join(os.path.dirname(__file__)))
from rcon import rcon

X1, Z1, X2, Z2 = (int(x) for x in sys.argv[1:5])
BUDGET = int(sys.argv[5]) if len(sys.argv) > 5 else 3300
REG = "jjworld/dimensions/minecraft/overworld/region"
T0 = time.time()

assert X1 % 128 == 0 and Z1 % 128 == 0, "tile bounds must be multiples of 128 chunks"

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

def boot():
    proc = subprocess.Popen(
        ["java", "-Xms1G", "-Xmx8G", "-jar", "paper.jar", "nogui"],
        stdout=open("server.log", "w"), stderr=subprocess.STDOUT)
    for _ in range(360):
        try:
            if "Done (" in open("server.log").read():
                return proc
        except FileNotFoundError:
            pass
        time.sleep(1)
    proc.kill()
    raise RuntimeError("boot timeout")

def stop(proc):
    try:
        rcon("forceload remove all", timeout=120)
    except Exception:
        pass
    try:
        rcon("stop", timeout=60)
    except Exception:
        pass
    try:
        proc.wait(timeout=180)
    except Exception:
        proc.kill()
        proc.wait()

WAVES = [(X1 + i * 16, Z1 + j * 16) for i in range(8) for j in range(8)]
TARGET = 128 * 128

attempt = 0
while time.time() - T0 < BUDGET:
    attempt += 1
    pending = [w for w in WAVES if not wave_done(*w)]
    done = len(WAVES) - len(pending)
    print(f"[attempt {attempt}] tile chunks={tile_chunks()}/{TARGET} "
          f"waves done={done}/64 t={time.time()-T0:.0f}s", flush=True)
    if not pending:
        break
    proc = boot()
    for wx, wz in pending:
        x1, z1 = wx * 16, wz * 16
        try:
            rcon(f"forceload add {x1} {z1} {x1+255} {z1+255}", timeout=60)
        except Exception as e:
            print(f"  mark {wx},{wz} err {e}", flush=True)
        time.sleep(0.3)
    print(f"  marked {len(pending)} waves t={time.time()-T0:.0f}s", flush=True)
    # wait for generation: poll owned chunk count, stop when stable
    last, stable = tile_chunks(), 0
    while time.time() - T0 < BUDGET:
        time.sleep(15)
        cur = tile_chunks()
        stable = stable + 1 if cur >= last else 0
        last = max(last, cur)
        if stable >= 4 or cur >= TARGET:
            break
    print(f"  iteration done: chunks={tile_chunks()} t={time.time()-T0:.0f}s", flush=True)
    stop(proc)

final = tile_chunks()
print(f"TILE COMPLETE chunks={final}/{TARGET} elapsed={time.time()-T0:.0f}s", flush=True)
ok = final >= TARGET
print(f"TILE_RESULT {'OK' if ok else 'INCOMPLETE'} {final}/{TARGET}", flush=True)
sys.exit(0 if ok else 1)
