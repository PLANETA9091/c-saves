#!/usr/bin/env python3
"""Scatter realistic loot chests (block entities with Items NBT) around the
spawn area of the survival benchmark world.

Deterministic (seed 9092). Uses ONE persistent RCON connection. Auto-detects
the block-entity item field format on the first chest (lowercase {id,count,slot}
for 26.x data components vs legacy {Slot,Count,id}) and falls back if the
server rejects the modern form.

Usage: chests.py [n_chests=2000] [radius=250]
"""
import socket, struct, random, sys, time

HOST, PORT, PW = "127.0.0.1", 25575, "jjbench2026"
Y = 120  # spawn plane (chests float; block-entity bytes are identical)


def pkt(rid, ptype, payload: bytes) -> bytes:
    body = struct.pack("<ii", rid, ptype) + payload + b"\x00\x00"
    return struct.pack("<i", len(body)) + body


class Rcon:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT), timeout=30)
        self.s.sendall(pkt(1, 3, PW.encode()))
        rid, _, _ = self._recv()
        if rid == -1:
            raise RuntimeError("auth failed")

    def _recv(self):
        raw = b""
        while len(raw) < 4:
            c = self.s.recv(4 - len(raw))
            if not c:
                raise EOFError
            raw += c
        (ln,) = struct.unpack("<i", raw)
        data = b""
        while len(data) < ln:
            data += self.s.recv(ln - len(data))
        rid, ptype = struct.unpack("<ii", data[:8])
        return rid, ptype, data[8:-2]

    def cmd(self, c, timeout=30):
        self.s.settimeout(timeout)
        self.s.sendall(pkt(2, 2, c.encode()))
        out = []
        while True:
            rid, pt, payload = self._recv()
            out.append(payload.decode("utf-8", "replace"))
            if pt == 2:
                break
        return "".join(out)


# realistic survival loot: (item, min_stack, max_stack, weight)
COMMON = [
    ("minecraft:cobblestone", 12, 64, 20),
    ("minecraft:dirt", 8, 64, 8),
    ("minecraft:oak_log", 4, 32, 10),
    ("minecraft:oak_planks", 8, 48, 8),
    ("minecraft:wheat", 6, 48, 10),
    ("minecraft:bread", 1, 12, 10),
    ("minecraft:iron_ingot", 2, 40, 9),
    ("minecraft:coal", 5, 64, 10),
    ("minecraft:stick", 4, 64, 6),
    ("minecraft:torch", 6, 64, 8),
    ("minecraft:string", 1, 24, 5),
    ("minecraft:arrow", 4, 64, 5),
    ("minecraft:bone", 1, 20, 4),
    ("minecraft:gunpowder", 1, 12, 3),
    ("minecraft:rotten_flesh", 1, 16, 3),
    ("minecraft:leather", 1, 12, 3),
    ("minecraft: wheat_seeds", 1, 24, 0),  # typo guard: fixed below
]
COMMON = [(i.replace(" ", ""), a, b, w) for (i, a, b, w) in COMMON]
RARE = [
    ("minecraft:diamond", 1, 8, 2),
    ("minecraft:emerald", 1, 12, 2),
    ("minecraft:golden_apple", 1, 3, 1),
    ("minecraft:ender_pearl", 1, 6, 2),
    ("minecraft:book", 1, 8, 3),
    ("minecraft:crying_obsidian", 1, 6, 1),
    ("minecraft:netherite_scrap", 1, 4, 1),
]
TOOLS = [
    "minecraft:iron_pickaxe",
    "minecraft:iron_axe",
    "minecraft:iron_shovel",
    "minecraft:iron_sword",
    "minecraft:bow",
    "minecraft:fishing_rod",
    "minecraft:shield",
    "minecraft:bucket",
    "minecraft:shears",
    "minecraft:flint_and_steel",
    "minecraft:compass",
    "minecraft:clock",
    "minecraft:spyglass",
    "minecraft:lead",
    "minecraft:name_tag",
]

pool = [(i, a, b, w) for (i, a, b, w) in COMMON + RARE]
items_pool = [i for (i, a, b, w) in pool]
weights = [w for (i, a, b, w) in pool]


def make_loot(rng, modern: bool) -> str:
    n_items = rng.randint(6, 22)
    slots = rng.sample(range(27), min(n_items, 27))
    parts = []
    for slot in slots:
        if rng.random() < 0.08:
            iid = rng.choice(TOOLS)
            cnt = 1
        else:
            iid = rng.choices(items_pool, weights)[0]
            lo, hi = next((a, b) for (i, a, b, w) in pool if i == iid)
            cnt = rng.randint(lo, min(hi, 64))
        if modern:
            parts.append(f'{{slot:{slot}b,id:"{iid}",count:{cnt}}}')
        else:
            parts.append(f'{{Slot:{slot}b,id:"{iid}",Count:{cnt}b}}')
    return "[" + ",".join(parts) + "]"


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
    radius = int(sys.argv[2]) if len(sys.argv) > 2 else 250
    rng = random.Random(9092)
    r = Rcon()
    # format probe
    modern = True
    probe = f'setblock 0 {Y} 0 minecraft:chest{{Items:[{{slot:0b,id:"minecraft:stone",count:1}}]}}'
    if "Changed" not in r.cmd(probe):
        modern = False
        r.cmd(f"setblock 0 {Y} 0 minecraft:air")
        probe = f'setblock 0 {Y} 0 minecraft:chest{{Items:[{{Slot:0b,id:"minecraft:stone",Count:1b}}]}}'
        if "Changed" not in r.cmd(probe):
            print("FATAL: setblock chest NBT rejected in both formats")
            sys.exit(1)
    print(f"item field format: {'modern (slot/count)' if modern else 'legacy (Slot/Count)'}")
    r.cmd(f"setblock 0 {Y} 0 minecraft:air")

    t0 = time.time()
    ok = fail = 0
    for i in range(n):
        x = rng.randint(-radius, radius)
        z = rng.randint(-radius, radius)
        loot = make_loot(rng, modern)
        key = "slot" if modern else "Slot"
        nbt = f"{{Items:[{loot}]}}"
        cmd = f"setblock {x} {Y} {z} minecraft:chest{nbt}"
        try:
            resp = r.cmd(cmd)
            if "Changed" in resp:
                ok += 1
            else:
                fail += 1
                if fail <= 3:
                    print(f"rejected: {resp[:120]}")
        except Exception as e:
            fail += 1
            if fail <= 3:
                print(f"error: {e}")
        if (i + 1) % 250 == 0:
            print(f"  {i+1}/{n} ok={ok} fail={fail} {time.time()-t0:.0f}s", flush=True)
    r.cmd("save-all flush")
    print(f"chests: ok={ok} fail={fail} in {time.time()-t0:.0f}s")


if __name__ == "__main__":
    main()
