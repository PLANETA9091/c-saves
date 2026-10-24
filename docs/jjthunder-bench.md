# JJThunder To The Max — real-world benchmark

The synthetic `bench_size` world models Tectonic-style terrain. This benchmark
additionally runs against a **real** JJThunder To The Max world: a datapack
that generates mountains up to y=2032, so every chunk carries **131-132
sections** (vs 24 in a vanilla world) and 96% of sections are single-block
uniform stone/air/deepslate. This is the worst case for naive storage and the
best case for section dedup.

## Corpus

`bench-corpus/dim` holds a subset of a real Paper 26.2 + JJThunder 0.9.0 world
(seed 90919091):

* `region/r.0.0.mca`, `region/r.0.-1.mca` — 728 chunks, 95 434 sections
* `entities/r.*.mca` — 218 entity chunks holding ~10 000 summoned entities
  (2 000 villagers, 4 000 sheep, 2 000 zombies, 2 000 armor stands + natural
  spawns), persistence-required so they survive restarts

## Running

```
cd native
cargo run --release --example jj_profile -- ../bench-corpus/dim
```

The profile reports, byte-exactly:

1. per-component raw weights (block_states data, palettes, biomes, heightmaps…),
2. anvil vs CRS2 container size after the production pipeline
   (split → CSEC-v3 encode → dedup → dict → hot tier → pack pass → cold tier),
3. full semantic verification of every chunk (must be `bad=0`).

Current results on the corpus (v0.4.0):

```
region  : anvil 4.47 MiB -> c-saves 1.73 MiB  (x2.58 smaller)
entities: anvil 0.93 MiB -> c-saves 0.77 MiB  (x1.21 smaller)
TOTAL   : anvil 5.39 MiB -> c-saves 2.50 MiB  (x2.16 smaller)
verify: ok=946 bad=0
```

On the full 1024-chunk world (13 MiB anvil): region x2.38, total x2.24.

## Reproducing the world from scratch

Requires ~4 GB RAM, Java 25, and the datapack from
<https://modrinth.com/mod/jjthunder-to-the-max> (file
`JJThunder_To_The_Max_26.2_v0.9.0.zip`).

1. Boot Paper 26.2 with the zip in `world/datapacks/` (create the folder
   before first boot). First boot generates the world and searches a spawn
   point (~3 min with JJThunder terrain).
2. Disable the watchdog (`config/paper-global.yml` → `watchdog.enable: false`):
   console `forceload add` synchronously generates chunks on the server thread
   and would otherwise trip the 60 s watchdog.
3. Pregenerate in waves of 16×16 chunks (`forceload add X1*16 Z1*16
   X2*16+15 Z2*16+15`), waiting until `world/.../region` stops growing, then
   `forceload remove all`. A 32×32-chunk grid takes ~4 minutes on 2 cores.
4. Spawn the entity corpus over a persistent RCON connection (see
   `scripts/jj/entities.py`): 10k `summon <mob> x 120 z
   {PersistenceRequired:1b,Tags:["jjb"]}` commands in a golden-angle spiral,
   then `save-all flush` and `stop`.
5. Run `jj_profile` against the dimension folder
   (`world/dimensions/minecraft/overworld`).

## Why the codec is shaped by this world

Profiling the real corpus drove every 0.4.0 change:

* **Compound palettes** — 26.x palette entries are `{Name, Properties}`
  compounds, not strings; the old codec silently fell back to raw NBT for
  100% of real sections (synthetic benches never caught it because wgen
  emits string palettes). CSEC v3 encodes both forms.
* **Vanilla 4-bit clamp** — linear palettes pack at `max(4, ceillog2(n))`
  bits, so 9 980 sections carried 256-long data arrays that the old
  `bits_for(n)` check rejected. Another 13 MiB of raw NBT fallback.
* **Starlight keys** — Paper writes `starlight.skylight_state` /
  `starlight.blocklight_state` Ints into sections; now first-class CSEC
  fields (1 byte each).
* **Frequency-canonical palettes** — the dominant block gets index 0, so
  dense data streams become zero runs; the order is a pure function of
  content, so identical sections still dedup (permutation-invariant).
* **Locality-ordered slab packing** — sections of one chunk are packed into
  the same slab in Y order (measured -17% slab bytes vs map-order packing).
* **Auto-pack watermark** — fresh imports/worldgen never produce dead bytes,
  which used to mean slab packing and cold-tier recompression never ran.
  Now a pack pass fires after imports and whenever the hot tier grows past
  4 MiB.
* **Mode-aware dictionary** — section slabs compress better WITHOUT the
  zstd dictionary (canonical CSEC bytes); entity payload slabs compress
  better WITH it (raw NBT boilerplate).
