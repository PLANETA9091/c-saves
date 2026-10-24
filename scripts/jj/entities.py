#!/usr/bin/env python3
"""Spawn the 10k entity corpus over one persistent RCON connection."""
import sys, math, random
sys.path.insert(0, "/home/z/jj/scripts")
from rcon import rcon

random.seed(9091)
CX, CZ = 0, 0          # chunk center area (all generated: chunks -16..15)
R_BLOCKS = 120         # spread radius

def pos(i):
    # deterministic spiral-ish spread with jitter
    a = i * 2.399963  # golden angle
    r = R_BLOCKS * math.sqrt((i + 0.5) / 10000.0)
    x = CX * 16 + 8 + r * math.cos(a) + random.uniform(-1.5, 1.5)
    z = CZ * 16 + 8 + r * math.sin(a) + random.uniform(-1.5, 1.5)
    return x, z

kinds = (
    [("minecraft:villager", 2000, '{"Tags":["jjb"],"PersistenceRequired":1b}')] +
    [("minecraft:sheep", 4000, '{"Tags":["jjb"],"PersistenceRequired":1b}')] +
    [("minecraft:zombie", 2000, '{"Tags":["jjb"],"PersistenceRequired":1b}')] +
    [("minecraft:armor_stand", 2000, '{"Tags":["jjb"],"PersistenceRequired":1b,"ShowArms":1b}')]
)
cmds = []
idx = 0
for kind, n, nbt in kinds:
    for _ in range(n):
        x, z = pos(idx)
        y = 120
        cmds.append(f'summon {kind} {x:.2f} {y} {z:.2f} {nbt}')
        idx += 1

print("commands:", len(cmds), flush=True)

import socket, struct as _s
HOST, PORT, PW = "127.0.0.1", 25575, "jjbench2026"

def pkt(rid, ptype, payload):
    body = _s.pack("<ii", rid, ptype) + payload.encode() + b"\x00\x00"
    return _s.pack("<i", len(body)) + body

def recv_pkt(s):
    raw = b""
    while len(raw) < 4:
        c = s.recv(4 - len(raw))
        if not c: raise EOFError
        raw += c
    (ln,) = _s.unpack("<i", raw)
    d = b""
    while len(d) < ln:
        c = s.recv(ln - len(d))
        if not c: raise EOFError
        d += c
    rid, ptype = _s.unpack("<ii", d[:8])
    return rid, ptype, d[8:-2]

s = socket.create_connection((HOST, PORT), timeout=15)
s.sendall(pkt(1, 3, PW)); recv_pkt(s)
ok = 0
for i, c in enumerate(cmds):
    try:
        s.sendall(pkt(2, 2, c))
        _, pt, payload = recv_pkt(s)
        out = payload.decode("utf-8", "replace")
        if "Cannot" in out or "Unknown" in out or "Expected" in out:
            if ok < 3 or i % 1000 == 0:
                print("ERR", i, out[:120], flush=True)
        else:
            ok += 1
    except Exception as e:
        print("FAIL", i, repr(e)[:80], flush=True)
        try:
            s = socket.create_connection((HOST, PORT), timeout=15)
            s.sendall(pkt(1, 3, PW)); recv_pkt(s)
        except Exception:
            break
    if i % 2000 == 1999:
        print("progress", i + 1, flush=True)
print("summoned ok:", ok, flush=True)
print(rcon("save-all flush", timeout=120), flush=True)
