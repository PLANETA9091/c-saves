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

Current results on the corpus (v0.7.0):

```
region  : anvil 4.47 MiB -> c-saves 1.55 MiB  (x2.88 smaller)
entities: anvil 0.93 MiB -> c-saves 0.30 MiB  (x3.13 smaller)
TOTAL   : anvil 5.39 MiB -> c-saves 1.85 MiB  (x2.92 smaller)
verify: ok=946 bad=0
```

Entity slabs sit within ~2% of the theoretical zstd-19 concatenated floor
(301,927 B for the whole corpus as one frame).

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

## What v0.5.0 changed (profiling-driven)

Re-profiling the real corpus after v0.4.0 showed the entity store was the
weak spot (x1.21 vs Anvil while the concatenated z19 floor implied x3+):

1. **Verbatim whole-mode blobs** — the field-split path re-serialized entity
   NBT through the value model, which sorts compound keys alphabetically.
   Vanilla/Paper writers emit optional fields (Bukkit.*, Paper.*, Spigot.*)
   last, keeping identical entities byte-aligned across the corpus; the
   alphabetical order interleaves them and destroyed zstd matches:
   measured x24.6 (verbatim) vs x10.3 (re-serialized). Payloads above 1 KiB
   are now stored as one verbatim blob (field split kept for small POI-style
   payloads where cross-chunk structure dedup wins).
2. **Batched entity slabs** — whole-mode slab records grew from 512 KiB to
   4 MiB raw (`slab_max_raw_whole`) with a 1 MiB member cap: 218 entity
   chunks pack into 2 slabs, so each zstd frame sees the whole corpus
   window. Entity store: 803 KiB -> 310 KiB.
3. **No dictionary for whole-mode stores** — the trained dict cost 114 KiB
   on disk but saved only ~38 KiB of compressed bytes (net loss); batched
   4 MiB frames self-compress to the floor without it.
4. **SLAB v2 (kind-8) varint entry tables** — member entries were 24 fixed
   bytes (u32 id + u32 len + 16B hash); ids are assigned in walk order, so
   zigzag-delta varint id + varint len + 16B hash cuts the table to
   ~18-19 bytes/member and compresses better. Legacy kind-5 slabs parse
   unchanged; full rewrites upgrade them.
5. **Compressed recipe batches (kind-9)** — batch records now carry their
   (key, len) tables plus all recipes as ONE zstd-19 stream; entries are
   addressed through an in-memory batch index (`len=BATCHED` marker +
   `batch_idx`), rebuilt by scan on reopen. Legacy kind-7 batches still
   scan, load and rewrite. A stale-index-removal race (an in-loop flush
   re-batching a key whose old index entry was then deleted unconditionally)
   was caught by the corpus verification gate (bad=1) and fixed with a
   location check before removal.
6. **2 MiB section slabs** — region slab frames grew from 512 KiB to 2 MiB
   (x2.63 -> x2.74 on the corpus; bounded by the fixed 32 MiB slab cache so
   RAM is unchanged).

Synthetic bench (Tectonic-style wgen): compact container is x0.08 of Anvil
(was x0.11), 24/24 unit tests green.

## v0.5.1: CSEC narrow-data packing

Vanilla clamps linear palettes to 4 bits/entry, so a 2-entry palette wastes
4x (2 KiB of data per section where 512 B would do). CSEC now packs dense
streams at ceillog2(n) whenever that beats the clamp (palettes under 16
entries; flags2 bits 2/3 mark narrow block_states/biomes data), and the
decoder normalizes back to the vanilla width. The palette frequency-count
pass now also detects vanilla-clamped source arrays (it previously failed
its length check and silently lost the frequency ordering for all
small-palette sections). Raw data bytes across the corpus drop x1.47;
post-compression the corpus gains ~20 KiB (4-bit zero runs were already
cheap), but repeat-encode stability and dedup ordering improve for any
small-palette world.

## I/O speed benchmark (v0.5.0)

`native/examples/speed_bench.rs` drives the SAME committed JJThunder corpus
(`bench-corpus/dim`) through an in-memory transcription of vanilla Anvil I/O
— header lookup + zlib-6 inflate/deflate, exactly what
`RegionFile.getChunkDataInputStream` / `getChunkDataOutputStream` do — and
through the real c-saves Store (production pipeline: split, dedup, tiers,
slab packing, dictionaries). Both sides run page-cache-warm so the
comparison isolates the CPU data path; disk behavior (append-only writes,
pread/mmap reads) is identical for both and scales both sides equally.

Methodology notes:

* The Anvil baseline is deliberately GENEROUS: it is Rust flate2 (zlib),
  not Java's Inflater-over-JNI; if c-saves beats this baseline it beats
  vanilla for sure.
* Latency percentiles (p50/p90/p99/p99.9/max) are captured per operation —
  these are the server's stall points; they are recorded as CI artifacts on
  every push (`speed_stalls.txt`).
* Semantic verification (palette-permutation-safe) runs on every chunk after
  the full workload; the CI gate fails on any drift.

### What was slow and what fixed it

| stage | before | after | fix |
|---|---|---|---|
| join (read) | 323 us/chunk | ~6 us (p50 chunks) | fused CSEC-to-NBT emitter: no per-section Value trees (one HashMap per palette entry!), wire-format bytes emitted directly; repeat sections splice from a per-thread emit cache keyed by content hash |
| voxel data | 4096 x per-bit loop | memcpy | one-pass dense-to-nostroddle converter; dividing widths (1/2/4/8/16/32) are a byte copy (the vanilla 4-bit clamp case), the rest rolls a u64 window |
| section load | 2 preads + 2 mutex per blob | 1 pass | batched loader: one segs lock pass, per-slab dedup, single slab-cache lock |
| segment reads | Mutex per blob on the mmap | lock-free | OnceLock mmap per segment |
| recipe batch | O(n) scan per write (quadratic under autosave storms) | O(1) | HashMap index + running byte total; read-your-writes is an index lookup |
| hot tier | zstd-12 per section | zstd-3 | measured 2.1x faster imports (34.8 vs 16.8 MB/s), write p99.9 30ms -> 10.5ms, IDENTICAL packed size (the background pack recompresses at 19 anyway) |
| re-encode on rewrite | every section re-canonicalized | cache hit | per-thread encode cache keyed by the order-free structural hash; identity shortcut skips unpack/remap/pack when the palette is already canonical |

### Results (2-core sandbox, rounds=3)

* region read warm: p50 15 us (was 199), **1401 MB/s single-thread — 1.7x
  faster than vanilla zlib inflate** (821 MB/s), p90 47 us vs Anvil 150 us,
  p99 73 us vs 214 us; 29k chunks/s on 2 threads (Moonrise loads chunks with
  parallel IO workers). The read path also beats the Anvil baseline's
  latency tails: fewer and smaller stall points at every percentile.
* entity write: 747 MB/s fresh, 1677 MB/s steady — 6.8x / 15x faster than
  vanilla zlib-6.
* region write: p90/p99 tails beat Anvil (1.6 ms vs 2.3 ms at p90 on the
  autosave path); fresh-import throughput 34.8 MB/s vs Anvil's 53.8 — the
  one-time import is the only phase still behind, and its latency tail
  (p99.9 10.5 ms) stays far below the 50 ms/tick budget.
* size gate unchanged: x2.83 smaller than Anvil, semantic verify 946/946.

### Known next levers

* ~~tail direct splice~~ DONE in v0.5.0: the stored tail is spliced verbatim
  (only dense heightmaps transform back to LongArrays) — this alone took warm
  reads from 340 to 1401 MB/s.
* hot-slab batching on the write path (compress new sections as one frame
  instead of per-section zstd calls) for fresh-worldgen throughput.
* raw-scan split: hash section byte ranges without parsing the full chunk
  NBT (removes the parse floor on rewrites).

## v0.7.0: post-compression attribution, shift-ref codec, 8 MiB slab frames

A new attribution experiment (`examples/jj_exp3.rs`) compressed every
component class separately at zstd-19 and measured where the container bytes
actually live:

* dense `block_states` data streams: **1.05 MB of the 1.63 MB region
  container** (zstd already achieves 0.50 bits/voxel — BELOW the order-1
  conditional entropy of 1.09, i.e. the redundancy is long-range LZ structure,
  not local predictability; rANS/context coding and zstd dictionaries were
  measured and all lose: x0.75-1.12 vs plain z19 concat).
* biomes compress to 26 KB, palettes to 29 KB, heightmaps (delta-coded) to
  25 KB — all minor.

An earlier shift-match experiment reported 96.3% of dense sections matching a
vertical shift of a lower section; that number was an artifact (k=16 compared
empty slices — vacuously true). The true rate on this corpus is ~9%: JJThunder
mountains separate their dense (surface-transition) sections by 50-150 planes
of uniform stone, beyond the k<=15 window where a ref still pays for its
explicit top planes.

What shipped anyway (it is nearly free and pulls real weight on layered
terrain — ocean floors, beaches, terraces — common in survival worlds):

1. **Shift-ref sections (CSEC 0xC9)** — a section whose lower 4096-k*256
   voxels equal a lower section of the SAME chunk shifted up by k planes
   stores `[0xC9][target u16][k u8][f1'][f2'][n_extra][extras][top_kind
   uniform/packed/RLE][CSEC3 tail without block_states]`. The resolver
   rebuilds the canonical CSEC3 from the target's palette + the ref's top
   planes; resolution is a pure function of (member bytes, target bytes) and
   is cached under a mixed (member,target) key so content-keyed caches stay
   correct. Encode-side admission requires the ref to beat the member's
   POST-COMPRESSION contribution (mbr + 32 < 0.4 * full_len; the v0.6.1
   raw-length lesson applies); only first-occurrence content is converted
   (cross-chunk-shared members dedup for free, which always beats a local
   ref); candidates are deduped by content hash and pre-filtered with
   per-plane u64 fingerprints; sections with members < 512 B never
   participate. `CSAVES_NO_SHIFT=1` disables the pass.
2. **8 MiB slab frames** (was 4 MiB) — the wider zstd window lifts the real
   corpus from x2.76 to x2.88 region / x2.92 total with the slab cache budget
   unchanged (32 MiB); cold-miss decompression stays ~7 ms, inside the tick
   budget. `jj_profile` now respects the production default instead of its
   old 512 KiB override.
3. Import throughput recovered to ~26 MB/s on the 2-core CI sandbox (the
   naive shift pass cost 2x; member-size gating + per-chunk ShiftSec cache +
   fingerprint pre-filter brought it back above the 25 MB/s gate).
