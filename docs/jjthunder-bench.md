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

## v0.7.1: compact_min 4 MiB -> 64 KiB — small stores kept 70% dead bytes

The survival bench exposed a production bug the JJ corpus never showed: after
the pack pass moves blobs into slab frames, the source segment keeps its dead
bytes until a full rewrite, and `compact_min` (4 MiB default) EXEMPTED every
segment smaller than that from rewrites. Any entity/POI store whose segments
stay in the hundreds-of-KiB range — i.e. most survival worlds' entity stores —
kept ~70% garbage permanently (survival entities measured x0.64 vs Anvil,
WORSE than vanilla while the slab frame itself sat at the zstd-19 floor).

With `compact_min: 64 KiB` a full rewrite of a dead segment costs ~1 ms, so
the churn guard is pointless at the old scale; the survival entity store now
settles at the compression floor:

* survival corpus: entities x0.64 -> **x1.94**, TOTAL x3.34 -> **x4.00**
* synthetic farm-entity probe: 283 KB -> 82 KB (z19 concat floor 77 KB)
* JJThunder corpus unchanged (x2.92 total; its stores were already above the
  old threshold), tests 36/36, verify clean, import 25.7 MB/s, warm reads
  1.6x anvil.

## v0.9.0: light-uniform codec (CSEC v4 0xC8) + bigworld pack-drain fix

The 512x512 real-survival attribution (`bigworld-survival`) measured the raw
component weights of a 261,542-chunk JJThunder world (11.5 GB raw NBT):

* block_states data 5.15 GB (44.7%), **SkyLight 1.64 GB + BlockLight 2.01 GB
  (31.8%!)**, sec_other 1.86 GB, Heightmaps 335 MB, biomes 316 MB.

Three fixes shipped against that attribution:

1. **CSEC v4 (0xC8)** — the flags1/flags2 bit budget was exhausted, so a
   third flag byte is introduced: `[0xC8][f1][f2][f3][y]`. f3 bit0 =
   BlockLight all-0 (previously stored RAW — 2048 B per underground section,
   the single biggest waste in deep mountain worlds). SkyLight all-0 now sets
   the reserved f1 bit5 (decoder already understood it; the encoder never
   set it). Streams without v4 features stay byte-identical v3 (0xC7), so
   dedup hashes are stable. Shift-ref members whose tail encodes as v4 use
   the new 0xCA member magic (`[0xCA][t u16][k u8][f1'][f2'][f3']...`).
   Uniform forms require the standard 2048-byte light length (safe guard).
2. **bigworld pack-drain fix** — the pack waves ran
   `request_pack(); loop { compact_step() }`, but `pack_segment` CLEARS
   pack_requested at entry and `pack_fallback` re-bases the auto-pack
   watermark after every call, so the loop stopped after ONE segment: the
   2 GB hot tier stayed per-section z3 (container 1629 MiB instead of the
   packed ~x3). The drain now re-requests a pack before every compact step
   until no hot blobs remain.
3. **Palette attribution fix** — `fastnbt::to_bytes` requires a compound
   root; bare Lists (block_states palettes) failed and measured 0 bytes,
   silently dumping ~1.86 GB of palette weight into `sec_other`. Both
   profilers wrap non-compound roots and strip the 5-byte wrapper.

`big_profile` additionally gained a full-key integrity scan (every written
region key is read back) with a self-heal re-import loop (max 2 rounds) that
isolates pack-loss vs write-loss, and drains/steps/waves/profile are printed
to stdout so the CI log carries the full pack story. The workflow now
uploads artifacts `if: always()`.

## v0.9.1 addendum: correctness round found by the autosave fast path

The unchanged-write fast path (hash match against the last write → O(1),
where Anvil pays inflate+deflate) exposed two latent corruption bugs and one
operational hazard:

1. **scan() refcount rebuild**: the reopen pass resolved BATCHED recipes
   with `load_recipe`, which cannot follow the kind-9 batch index (BATCHED
   is a sentinel, not a length) — every batched recipe's refcount was
   rebuilt as 0. The first full-rewrite cycle then superseded recipes down
   to refs=0 and evicted LIVE blobs that other, still-unrewritten recipes
   referenced → dangling ids → unreadable chunks. Masked for months because
   the same rewrites re-created the blobs; any chunk NOT rewritten after a
   restart would break. Resolved through `recipe_at`.
2. **read-your-writes**: a BATCHED rewrite removes its batch_idx entry at
   supersede time, so reads hitting the stale committed pointer returned
   None until the next flush. The pending recipe batch now wins.
3. **strip is per-call**: the background import strips derived light; the
   live server's saves must NOT (relit light is stored, never recomputed).
   The first implementation used a process-global flag that would have
   leaked the import's strip into runtime saves — a relight-on-every-load
   loop. Now `split(..., strip)` / `Store::write_chunk_import`.

The fast path is SELF-HEALING: on a hash match it probes `chunk_intact()`
(recipe resolves + every referenced blob is live — pure map lookups via the
probe cache) and falls through to the full write on any damage.

Drain speed: `pack_segment` victims come from the blobs map sorted by blob
id (ids allocate in section write order ≈ chunk locality — same-chunk
sections still land in one slab frame) instead of a per-segment full
chunks-map sort + per-key `recipe_at`. The 262k-chunk world's pack pass was
O(segments × chunks) batch decompresses (~40+ min); now O(blobs log blobs).

## v0.9.2 — root cause of the 11,819-missing integrity loss (dict file GC)

The bigworld-survival run on 4eec7a4 drained the 261,573-chunk world to
956 MiB but the integrity scan found **11,819 region keys unreadable
(4.5%)** and died in heal round 1 against the job timeout. The loss class:

**`gc_dicts` deleted dictionary FILES.** Shared zstd dictionaries are
versioned (`dicts/dict-vN.bin`); the GC kept a version alive only when some
blob in the blobs map still carried its id. But compressed data OUTSIDE the
blobs map also references dict versions: **kind-2 recipe inline tails**
(`Tail::Inline { dict, .. }` — tails smaller than `tail_blob_min`, compressed
on the hot tier WITH the then-current dictionary). That reference lives in
the recipe and is invisible to blob-only accounting. Sequence on the big
world: dict v1 trained early (dict_train=40) → chunks written under v1 →
later versions trained → pack waves re-tiered every v1 blob to plain slabs
or v2/v3 colds → `gc_dicts` saw no blob on v1 → `fs::remove_file(dict-1.bin)`
→ every recipe whose inline tail was compressed under v1 failed
`decompress(v1, ..)` → `read_chunk` → None. 4.5% of chunks = the share
written between dict version switches with sub-512-byte tails.

Fix (three layers):
1. **Dict files are immutable.** `gc_dicts` only trims the in-RAM map; the
   disk copies (≤ ~112 KiB per version, ≤ 255 versions) are the source of
   truth. Worst-case disk cost ~28 MB, in practice a handful of versions.
2. **`decompress` lazy-loads from disk** on a RAM cache miss — reads survive
   any trim. This also closed a same-thread **read→write self-deadlock**: a
   `match` on `dicts.read().get(..)` holds the read guard across the arms,
   and the lazy-reload arm takes `dicts.write()` — the guard is now scoped
   out before the reload.
3. **Forensics**: `Store::read_fail_reason(key)` walks the read path and
   names the first blocking stage (recipe / blobs map / segment file /
   blob range / dict version absent in RAM AND on disk); the bigworld
   integrity scan prints it for a sample of every missing set, plus the
   surviving dict file list. `big_profile` reports the integrity phase wall
   time. A bounded-drain guard fails fast if the maintenance loop stops
   converging instead of eating the CI job.

Regression test `dict_gc_keeps_files_inline_tails_readable` reproduces the
exact loss: all-unique-section chunks (a rewrite must strand dead blobs so
the dead-ratio drop_seg compaction fires) + two dict versions + drain; on
the pre-fix code it fails with "dict-1.bin must never be deleted".

Workflow: `real_exp` (class attribution) moved to a parallel `attrib` job
(it re-imports the whole world a second time — the sequential layout made
the bench job exceed its 350-minute budget), and both CI jobs now build
with a Rust cache (~10 min saved per run).

## v0.9.3 — roll hole, probe-cache deadlock, live Paper 26.3 tps-bench

### The 23,380-missing loss class: writer-segment roll hole

Forensics (`read_fail_reason`) on run 41020241542 named it: every sampled
missing chunk = "blob ... segment 1 removed from segs", plus a storm of
"LEAK: slice fail ... rawlen=0" while packing segs 25-30 — batch records
decompressing to ZERO bytes. Root cause in `append()`: the writer roll
seeked to `written` while the append buffer held unflushed records that
belong at `written - buf.len()` (the file-position invariant). The roll
wrote them a buffer-length too far: a hole of zeros over the segment tail
(up to BUF_CAP = 1 MiB per roll) with the buffer duplicated after it.
Batch records in the hole decode rawlen=0; blobs in the hole decompress to
garbage; the pack skips them (continue) and the drop_seg removal turns
them into permanent dangling references. The JJ corpus (< 64 MiB) never
rolls a segment with a non-empty buffer — only the 262k world rolled 60+
times per run, losing a timing-dependent 11,819-23,380 chunks.

Fixes: roll seeks `written - buf.len()`; `Cfg.seg_roll`
(CSAVES_SEG_ROLL, default 64 MiB) so tests force rolls; the pack drop
path is poison-guarded — any unreadable victim (blob read fail,
batch_idx miss, hdr/body/decompress/slice fail, unknown recipe body)
keeps the segment (`pack_broken`, never considered by compaction again,
loud logs). Regression test `seg_roll_with_pending_buffer_keeps_every_record`
(seg_roll 192 KiB < BUF_CAP forces non-empty buffers at every roll;
fails on pre-fix code). Bigworld after the fix: **261,573/261,573
integrity, heal_rounds 0, verify bad=0, TOTAL x2.21** (region x2.20,
entities x8.48; vs stripped raw ~x7.7; vanilla terrain = dense sections
at the z19 floor, dense:rle = 2.29M:250k).

### Probe-cache self-deadlock (the 20-minute speed-bench "cancellations")

`chunk_intact`'s probe-cache lookup was an `if let` scrutinee; edition
2021 keeps the scrutinee temporary (the MutexGuard) alive through the
else branch, so the miss path re-locked probe_cache against itself:
parking_lot spin (~9 s) + park (~5 s), forever. Requires a committed
BATCHED key whose payload hash matches last_written — exactly the speed
bench's G2 phase and Paper's unchanged-chunk autosave; import/G phases
never hit it, so it shipped in v0.9.1 unnoticed (every speed-bench since
died at the 20-min job timeout). Fixed by binding the lookup to a
standalone statement; verified with a standalone repro + marker bisect.

### Concurrent supersede during pack (production firing in phase B)

The module runs compaction concurrently with Paper's save thread: the
pack's recs snapshot held dead pointers (supersede removes the batch_idx
entry, the chunks map flips at flush). Moving them tripped the poison
("batch_idx miss xN ... seg-00011 PARTIAL — segment KEPT") or worse,
re-queued the OLD recipe over the newer pending one (RecipeBatch is
last-write-wins → silent chunk-state rollback). The loop re-checks the
live chunks pointer and skips superseded keys. Regression test
`concurrent_supersede_during_pack_keeps_newest_state` (writer thread +
compaction waves); after the fix the live phase-B console is clean
(zero LEAK/PARTIAL lines).

### Live Paper 26.3 tps-bench (the TPS milestone)

module job builds `libc_saves.so` (cdylib name: hyphens → underscores;
module.json `main` must match — CRUSSTY silently skips a plugin whose
entry does not resolve), packages it with module.json; the tps job runs
phase A (plain Paper) and phase B (CRUSSTY launcher + the module:
SavesShim natives, Moonrise RFS bridges, auto-import) on one prepared
1024-chunk world + 10k persisted entities. Measurement notes that cost
us a few runs: `forceload add` takes BLOCK coords (chunk coords marked
2x2 chunks instead of 256); chat §-codes must be stripped before
parsing; Paper's `mspt` answers "◴ avg/min/max" for 5s/10s/1m (no
p95/p99); with 10k ticking entities `save-all flush` never converges on
a 2-core runner (both engines censored at any RCON timeout — reported,
not gated). Result (8 samples/phase): TPS 19.96 vs 19.95, MSPT avg 5.78
vs 5.96 ms, MSPT max 12.11 vs 10.41 ms (c-saves better), rotate 491.6 s
vs 493.2 s, module attached, import 897+193 chunks 0 failed, zero
integrity events.

## 0.9.4 — GLOB natural-choice root cause: shift-ref permutations vs the rebuilt palette order

### The hunt

The GLOB form (0xCB global state registry) shipped opt-in in 0.9.3 because
the NATURAL per-slab size choice corrupted 32 of 946 JJ-corpus chunks
(x3.08 but verify bad=32), while the forced path was test-green. Three
instruments closed the gap:

* `Store::slab_invariant_scan` — cross-checks every slab entry in the blob
  map against the ON-DISK slab it points at (hash in the entry table,
  slab_off/raw_len equal to the table's cursor walk). Result: the map and
  the disk tables were CONSISTENT at every drain step and after sync —
  the store layers were innocent.
* `probe_glob` — the exact jj_profile pipeline with per-step invariant
  scans. First version read all chunks between steps and stayed clean at
  bad=0: the reads populate the slab/emit caches and MASK the bug.
  Read-free scans + one final verify reproduce bad=32 exactly
  (first bad chunks (3,-20), (0,-19)).
* `probe_bad2` — surgical diff of the first bad chunks: the palette came
  back SHIFTED (src [stone, diorite, coal_ore] → got [andesite, stone,
  diorite, coal_ore]) and the members were **0xC9 shift-refs**, not 0xCB.

### Root cause

A 0xC9 shift-ref stores its top planes as **target-canonical palette
indices** (computed at write time against the target's canonical order).
`resolve_one` expands them against the target's palette AS DECODED FROM
ITS CURRENT BYTES. `glob_to_section` — the 0xCB → canonical rebuild —
sorted the rebuilt palette by count ASCENDING, while the canonical
encoder (`enc_pal_data_canon`) sorts count DESCENDING (dominant first).
Every shift whose target was glob-rebuilt therefore decoded through a
ROTATED palette: index 0 resolved to the RAREST state instead of the
dominant one. 32 chunks on the corpus had a glob-rebuilt shift target;
the other 914 did not (targets canonical, or RLE — `section_to_glob`
refuses non-dense members, which is also why the synthetic GLOB test
never caught it: strata-free random sections have no shifts AND its
targets stayed dense-canonical under FORCE... the real trigger needs a
shift member plus a glob-rebuilt dense target in one chunk, exactly the
JJ terrain shape).

### Fix + verification

`glob_to_section` now reproduces the exact canonical rule (count DESC,
ties (plain, name, props) ASC — the gid tiebreak never fires because
(plain, name, props) ties imply identical states). With that, a glob
roundtrip is byte-identical for encoder-produced sections, so:

* shift permutations stay valid regardless of which slabs globbed;
* dedup hashes of glob-roundtripped sections match fresh encodes again
  (the rebuild is a pure function of content).

Results on the committed corpus (728 region + 218 entity chunks):

| mode | container | verify |
| --- | --- | --- |
| natural GLOB (now default) | x3.08 | 946/946 |
| FORCE_GLOB=1 (all-dense globbed) | x3.08 | 946/946 |
| CSAVES_GLOB=0 (opt-out) | x2.99 | 946/946 |

47/47 unit tests (new: `glob_rebuild_keeps_shiftref_target_order`,
which FAILS on pre-fix code — 3-state target with strictly descending
counts, k=1 shift member whose top plane sits at canonical index 0,
semantic verify post-pack and post-reopen). Speed bench with GLOB on:
warm region read 1318 MB/s p50 16.9 us, import 46.5 MB/s, steady region
write 88 MB/s, entity write 4.9 GB/s — all CI gates pass.

### GLOB default ON

`glob_enabled()` is now on unless `CSAVES_GLOB=0`. The known limit stays:
sections convert while the registry holds ≤ 256 states (1-byte gid
streams); a bigger world just stops converting NEW sections — existing
glob slabs keep reading through the persisted registry.

### v0.9.5 — GLOB2 (0xCC) gid-stream forms

Follow-up levers on the GLOB architecture (u16 registries + RLE'd gid
streams), plus two bugs the work flushed out:

* **wide-palette unpack** — the u8 unpack wrapped 9..16-bit voxel indices
  (`as u8`), silently corrupting the canonical encoder's frequency counts,
  the canonical remap and the section identity for palettes > 256 states
  with indices >= 256. Vanilla terrain never triggers it; modded worlds
  could. All unpack paths are width-correct now.
* **transpose decode** — the first 0xCC build decoded gform 2/5 payloads
  without un-transposing. `verify ok=944 bad=2` on this corpus (chunks
  (0,9)/(1,9)): identical per-state histograms, permuted positions — the
  0xC9 shift-ref signature (expansion against a rebuilt target palette).
  The transpose permutation is an involution; the rebuild applies it
  again. Regression test pins the gform-2-strictly-wins shape.

GID-RLE admission divisor on this corpus (default via `CSAVES_GID_RLE_DIV`):

| div | container | verify |
| --- | --- | --- |
| 4 (canonical-style 4x) | x2.99 | 946/946 |
| 8 (default) | x3.07 | 946/946 |
| 16 | x3.08 | 946/946 |
| 0 (gid RLE off = legacy 0xCB only) | x3.08 | 946/946 |

The JJ corpus is strata-dominated: its runny sections never reach the glob
pass (canonical RLE beats the packed stream outright), and its dense
sections are noise — so gid RLE has nothing to bite on HERE. The lever's
real target is moderate-strata vanilla terrain (survival bigworld), where
sections fail the canonical 4x-vs-packed-width test but clear the wider
4096-byte gid window. The bigworld-survival CI bench measures it.

### v0.9.8-v0.9.11 verdict — warm-read regression found and fixed; scan split

CI artifact archaeology (`scripts/ci_art.py`) surfaced a hidden regression
the 0.4x speed gate had been waving through via its 100 MB/s absolute
backstop: warm region read measured **0.25x same-run Anvil** (260 vs
1035 MB/s, p90 549 µs — a collapsed tail, not a slow mean). Per-stage
counters attributed 92% of sectload to per-read GLOB canonical rebuilds;
the v0.9.8 content-hash `CanonCache` turned the conversion into a
once-per-member pure function (hosted-runner warm read x1.3 aggregate vs
Anvil, p90 x2.3, gate raised to 0.8x).

v0.9.11 then replaced the import split's full-tree parse + HashMap tail
re-serialization with a zero-copy header-arithmetic walk (`scan_split`).
The walk's own diagnostics caught a cursor-drift bug in array skipping
(element count added as bytes), which had silently degraded scan coverage
to 72% of chunks; after the fix the corpus splits **100%** through the
scan path (full-parse fallback never runs).

Same-corpus, same-machine numbers (v0.9.10 -> v0.9.11):

| metric | v0.9.10 | v0.9.11 |
| --- | --- | --- |
| import ST | 36.9 MB/s | 41.5-42.3 MB/s |
| import p90 | 3496 µs | 2873 µs |
| scan coverage | — (full parse) | 728/728 |
| warm region read | 1217 MB/s | 1200-1261 MB/s |
| container (bench_size) | 2.96 MiB (x0.08) | 2.96 MiB (x0.08) |

Equivalence pins: `enc_fp` fp=6d240d7166cdda99 (12 130 members, byte-exact),
`verify ok=946 bad=0`, tests 53/53.

0.9.11.1 diagnostics follow-up: the deepest-walk beacon became an exact
failure-position store (SCAN_FAIL_POS at the failing site, no per-call
atomic on the hot path), `scan_fail` in the split-stage JSON carries
`{why, pos}`, and the JSON key quoting bug is fixed. `probe_keys` (new
example) cross-checks every corpus chunk against fastnbt: 728/728 scan
walks succeed, 0 failures. Independent re-run of speed_bench after the
rebase: import ST 44.3 MB/s (p50 240 µs), warm read 1349.8 MB/s, steady
write 82.8 MB/s, dirty10 612.9 MB/s, container x7.51 smaller than Anvil
sector-padded. The remaining `scan_root fail` lines in speed_bench logs
originate in the G2 dirty phase, which deliberately flips a payload byte —
scan_root rejecting corrupted NBT into the full-parse fallback is the
designed behavior, not a regression.

0.9.12 encoder-to-shift facts reuse: build timers split the shift build
(id 4.8 / unp 60.0 / key 2.8 / fp 11.6 ms) and showed the unpack+fill —
which the encoder already performs with the same comparator and the same
counts — was 74% of it. The encoder now hands the voxel stream and the
canonical order to the shift pass (moved, zero copy); the ShiftSec build
keeps only id interning + ident fill + plane fingerprints. Shift build
81.3 -> 24.3 ms, shift stage 141 -> 62.6 µs/chunk, scan 641 -> 561
µs/chunk, import ST 43.2-44.3 -> 47.1-48.1 MB/s (3-repeat confirmed;
first post-rebuild run reads low — measurement noise, p99.9 bimodal).
Match decisions and container bytes identical; enc_fp unchanged.

0.9.13 typed borrowed sections parse (lite): the sections-only fastnbt parse
inside scan_split materialized a full Value tree — a HashMap per palette
entry plus two Strings per property, 125.1 µs/chunk of allocations that the
encoder drops immediately. The lite path deserializes the same bytes into
borrowed structs (Cow strings via a custom serde visitor, BTreeMap props,
borrowed light slices): 2.4x faster parse, scanp 125.1 -> 62.5 µs/chunk,
scan 590.7 -> 479.0 µs, import ST 47.1-48.1 -> 53.5-56.3 MB/s (p50
240 -> 112 µs; 3-repeat confirmed), MT import 69.5 -> 81-82, dirty10 736.6.
Both currencies share SecView, so encode/shift bytes are pinned identical:
probe_lite asserts hash + encoded-byte parity over 42218 corpus sections
(0 mismatches), enc_fp fp 6d240d7166cdda99 members 12130 unchanged,
53/53 tests, verify 946/946 bad=0, container bytes unchanged. Any section
shape outside the 7 KNOWN keys fails the typed parse and the chunk takes
the unchanged Value path — the lite path cannot alter behavior, only speed.
0.9.14 shift build interns identities straight from the lite palette: the
0.9.13 build still materialized a full SecView per built section (per-entry
props Vecs + light-byte copies, ~8 µs x 2977 builds) only to feed
intern_ids, and that allocation churn was the unp_ms 6.2 -> 30 inflation
across bench phases. identity_from_lit / intern_ids_lit now serialize
identity bytes directly from the typed PalLit palette (BTreeMap props
iterate in exactly the canonical sorted order the view builders sort to —
same bytes, same chunk-global ids) and shift_sec_from_facts_lite builds
the ShiftSec with no view at all; the SecView is materialized lazily at
first winner-assembly use (vw_get! + view_ready mask — real builds mark
their slot, meta-cache hits stay not-ready and keep being skipped by the
assembly exactly as before). The rare lite full build and the Value path
keep the eager view. Results (3-repeat confirmed): import ST 53.5-56.3 ->
56.9-58.5 MB/s (+60% over 0.9.10, p50 112.9-115.9 µs), import MT 79.2-84.1,
warm read 1260.2-1302.3 MB/s, shift build 22.6-23.4 ms with unp_ms stable
at 5.9-6.2 across all phases. Match decisions byte-identical (dense_match=198,
full=327, ksum=3041, cand=447213); enc_fp fp 6d240d7166cdda99 members 12130
unchanged, probe_lite 42218 sections 0 mismatches, 53/53 tests, verify
728/728 + 218/218 + 1280/1280, bench_size x0.08 vs anvil compact, container
1.66 MiB (x7.51).

0.9.15 interleaved plane fingerprints: the stage timers left the shift
build at 14.6 ms of which the fp pass was the biggest slice (fp_ms 11.7 of
22.6-23.4) — the per-plane loop hashed each 256-entry FNV chain
sequentially, 16 latency-bound serial mul chains (~3.5 cycles per link,
~1 ns/step over 4096 entries). plane_fps (now the single fp source for all
three ShiftSec builders) interleaves the four chains of each 4-plane group
so they overlap: fp_ms 11.7 -> 3.6 (x3.25), shift build 22.6-23.4 ->
14.6 ms, import ST 56.9-58.5 -> 59.3-59.4 MB/s (3-repeat confirmed;
+58% over 0.9.10), import MT 83.0-85.3, warm read parity 1238-1266.
Per-plane hash values IDENTICAL (pure scheduling) — the diagnostic pins
are unchanged: cand=447213, plane0=327, full=327, ksum=3041,
dense_match=198. Gates: enc_fp fp 6d240d7166cdda99 members 12130 bytes
9971843 (byte-exact), probe_lite 42218 sections 0 mismatches, 53/53
tests, verify 728/728 + 218/218 + 1280/1280, bench_size x0.08 vs anvil
compact, container 1.66 MiB (x7.51).

0.9.16 fused unpack+counts+bounds + no-straddle pack fast paths: the
encoder core made THREE full passes over the same 4096 voxels per section
(unpack, sanity any(), counts loop) and packed through generic
variable-shift loops. unpack_counts_vox fuses all three into ONE pass with
width-specialized inner loops (bits 8 = to_le_bytes byte stream, bits 4 =
constant nibble shifts; vanilla no-straddle packing never crosses a long,
so per-long extraction is independent), dense_pack_mapped takes bits 4/8
(and wide bits 16) no-straddle byte paths. Per dense section: enc
16.3 -> 10.3 us, unp 3.8 -> 2.9 (counts fused in), pal 2.5 -> 0.07 (counts
moved out), scan 465 -> 351 us/chunk, shift 61.3 -> 48.8 (warm facts.v
cache side effect). Results (3-repeat confirmed): import ST 59.3-59.4 ->
70.6-72.6 MB/s (+91% over 0.9.10), import MT 96.9-105.2 MB/s @ 2 threads,
warm read 1321.9, p90 2300 -> 1786-1798 us. All outputs byte-identical:
enc_fp fp 6d240d7166cdda99 members 12130 bytes 9971843 unchanged, probe_lite
42218 sections 0 mismatches, 53/53 tests, verify 728/728 + 218/218 +
1280/1280, bench_size x0.08 vs anvil compact, container 1.66 MiB (x7.51),
match pins unchanged (plane0=327, full=327, dense_match=198).

0.9.17 direct sections parser (fastnbt off the hot path) + group-split
pack + lazy dense: the split stage's remaining bulk after 0.9.16 was
scanp 65 us/chunk (fastnbt serde over a wrap-buffer COPY of the sections
range), pack 5.3 us/data-array (a single or->shift->or accumulator chain
spanning all 4096 voxels), and an unconditional dense pack whose bytes
were discarded for the ~10% of data arrays where RLE wins. Three levers:
(1) parse_sections_direct walks the sections list IN PLACE and emits the
identical Vec<SecLite> — no wrap buffer, no serde machinery; safety is
one-sided (any shape doubt -> None -> the exact v0.9.13 fastnbt wrap
path), with fastnbt's int-tag coercion mirrored where proven (any NBT
int tag feeds i8/i32 via serde num visitors) and a refusal-reason
histogram (direct.whys) tracking the ~9% of chunks still taking the wrap
path — diagnostics only, outputs identical; scanp 65.4 -> 34.6 us/chunk,
import p50 113.7 -> 76-78 us. (2) pack_groups: 64 values x b bits =
exactly b longs, so the stream shards into independent 64-value groups —
and since even per-group state stays chained across groups (the renamer
cannot break an acc->acc data dependency through the reset), TWO groups
are packed in lockstep with fully independent (acc, n) state: 2x ILP on
the pack chain; byte-identical by construction, pinned by enc_fp.
(3) lazy dense: the RLE admission rule only consumes dense.len(), which
is known without packing ((entries*out_bits+7)/8 for the packer,
data.len()*8 for identity) — RLE winners skip the dense pack entirely.
Results (3-repeat confirmed): import ST 70.6-72.6 -> 82.6-83.8 MB/s
(+122% over 0.9.10), import MT 117.3-121.6 MB/s @ 2 threads, warm read
1257.5 (parity). All outputs byte-identical: enc_fp fp 6d240d7166cdda99
members 12130 bytes 9971843 unchanged, probe_lite 42218 sections 0
mismatches, 53/53 tests, verify 728/728 + 218/218 + 1280/1280,
bench_size x0.08 vs anvil compact, container 1.66 MiB (x7.51), match
pins unchanged (plane0=327, full=327, ksum=3041, cand=447213,
dense_match=198).

0.9.18 dir_bytes header fix — direct parser covers 100% of the corpus,
fb 66 -> 0: the v0.9.17 in-place parser refused 66/728 chunks (9%) into
the fastnbt wrap-parse. The direct.whys refusal histogram (new: wired
into every refusal class) showed 14 unknown-field + 49 Y-context + 3
uncounted refusals; an offline NBT replay of the first refusing payload
traced the walk to dir_bytes, which consumed len bytes but never ADDED
the 4-byte ByteArray length header — every SkyLight/BlockLight-bearing
section drifted 4 bytes left, the misread tail usually ends 0x00, the
compound "ends" early, and the cascade of garbage fields (tag 0x99,
39321-byte "names") tripped the refusal. The fastnbt fallback silently
papered over it: outputs were identical (every parity gate stayed green
across 0.9.17), only the path was slower. Fix: end = p + 4 + len.
direct ok=728 fb=0 whys all zero, scanp 35.3 -> 22.3 us/chunk (-37%:
zero fastnbt wrap-parses remain on the corpus), import ST 81-84 ->
87-89 MB/s (p50 77.6 -> 76.5 us), +132% over 0.9.10's 37.5. All pins
unchanged.

0.9.19 ensure_blobs: O(n) in-batch dedup, fingerprint hasher, reused
zstd CCtx. ensure_blobs_batch carried three invisible per-chunk costs:
a linear misses.iter().any() duplicate scan (O(n^2), ~8.4k tuple
compares per ~130-section chunk), SipHash on already-fingerprint keys
in the blobs/ids maps (~520 map ops per chunk; Hash IS a 128-bit
content hash and ids are sequential u32 — the bucket hasher needs
uniformity only, full-key compare still guards every probe), and a
fresh zstd CCtx per slab frame. Now: HashSet dedup + blobs/ids on
RawBuild (one multiply per 64-bit half) + a per-thread reusable
bulk::Compressor cached by level (byte-identical; recipe_batch
8.7 -> 5.8 us/chunk side effect). ensure_blobs 87.0 -> 79.8 us/chunk,
import p50 76.5 -> 72.4 us, import MT 120 -> 125.8 MB/s @ 2 threads.

0.9.20 one ids lock per slab group: flush_hot_slab_group acquired
ids.write() per member (130 RwLock round-trips per chunk on top of the
single blobs guard); one guard now covers the loop. Neutral on the
2-core sandbox (compute-bound), removes real cross-thread lock traffic
for multi-core import hosts. Also probed and DISCARDED in this window
(after differential testing against the accumulator, both byte-exact
on the corpus and 0-mismatch on random inputs bits 1..16): 4-way
group lockstep pack (+5.6% pack time — four interleaved remap load
streams thrash L1) and cursor-gather / closed-form packing (-4.5%
import — the per-value remap load and branch structure dominate, the
acc chain was already hidden by the 2-way lockstep). The 2-way group
packer stands.

Cumulative: import ST 37.5 (0.9.10) -> 87-89 MB/s (+132%), p50
113.7 -> 72.9 us/chunk, scanp 65.4 -> 22.3, ensure_blobs 87.0 -> 79.8,
warm read ~1530 MB/s, container and every byte-level pin unchanged
throughout.

0.9.21 bounded SIMD RLE admission: the new per-width packb timers
disproved the vanilla-width passthrough hypothesis (bits 5-7 repack is
only 5% of dense-pack cost; bits 2/3/4 are 85%) and exposed the real
pack-stage item: the RLE admission scans. The transposed-stream
transition count ran for every section — 17.7 ms of the 45.0 ms pack
stage, more than the dense pack itself (13.7 ms). Three byte-exact
cuts: (1) bounded scans — cap = dense_len/8 exactly separates
rejection from survival, the AVX2 walk aborts at cap, noise-heavy
sections exit within the first row pairs (b-scan 1168 -> 460 ns/call);
(2) vectorized column boundaries — the 255 boundary pairs regrouped by
z become 15 contiguous 16-element window compares + 15 scalar cross-x;
(3) UNI skip — uniform raw stream transposes to the same single value,
la == lb proven, b-scan skipped (0 fires on this layered corpus, free
insurance for uniform survival worlds). Import ST 87-91 -> 98-104
MB/s (median ~100.5, +12-15%); enc stage 94.4 -> 75.6 ms, pack stage
45.0 -> 30.1 ms per region pass. Gates: tests 56/56 (3 new
differential/contract tests), enc_fp fp=6d240d7166cdda99 members=12130
bytes=9971843, verify 728+218+1280 bad=0, bench_size x0.08, survival
ok=1280.

Cumulative: import ST 37.5 (0.9.10) -> 98-104 MB/s (+163-177%), p50
113.7 -> ~74 us/chunk, scanp 65.4 -> 21.4, ensure_blobs 87.0 -> 79.8,
warm read ~1530 MB/s, container and every byte-level pin unchanged
throughout.

0.9.22 ensure_blobs single-pass slab insert: flush_hot_slab_group
patched slab_off in a second blobs.get_mut walk (~130 hash probes per
chunk for values known at insert time). One pass; ensure_blobs 79.3 ->
76.6 us/chunk, import ST 98-104 -> 103-104 MB/s (warm, median 103.3).
enc_fp pin unchanged, tests 56/56, verify 728+218+1280 bad=0.

Cumulative: import ST 37.5 (0.9.10) -> 103-104 MB/s (+175-177%).

0.9.23 probe window (all REVERTED, tree back at 0.9.22; negative results
recorded so nobody re-digs these holes):
- vanilla-width passthrough for bits 5-7: the new per-width packb timers
  show bits 5/6/7 repacks are 0.66 ms of the 13.7 ms dense-pack budget
  (5%) — the format-flag + reader work cannot pay for itself. Bits 2/3/4
  are 85% of dense-pack cost but they are ALREADY memcpy passthrough when
  the canonical order is identity.
- LUT unpack fast paths (bits 1/2/4, pal_n == 1<<bits): isolated
  micro-bench said 19.8 -> 0.5 us per 4096-voxel bits=1 section, but the
  corpus disagreed — with all arms on, unp doubled (2.57 -> 5.22 us/call,
  import 106 -> 90 MB/s); arm-bisection pinned the damage on the bits=2/4
  LUT arms (+34.6 ms of unp per region pass) while the bits=1 arm was a
  noise-level -1.9 ms. The corpus microdata (run-layered two-state
  sections, predictor-friendly scalar loop) defeats the LUT+memcpy
  structure; a bits=1 arm kept alone measured 99.7-102.9 MB/s vs
  106.2 baseline — inside the machine noise band, and not worth carrying
  unexplained residual. Full revert; the differential test and the
  old-vs-new unp_bench harness were removed with it.
- lesson for the next unp attack: the win must come from restructuring
  the pass itself (fused unpack-remap-transpose without the 16 KB
  materialization, or SIMD on the long stream), not from re-decoding the
  same fields through a table.

0.9.24 — hot tier drops to zstd fast level -3 (import +13%, drained
container byte-identical). The 0.9.23 probes closed the pack-side micro
holes, so the next honest lever was the write path's compression budget:
ensure_blobs was 56.7 ms of the 162.9 ms import wall (25%) and its bulk is
the hot-tier zstd call over each whole-chunk slab frame. The hot tier is
transient material — the pack pass decompresses it and re-compresses at
cold_level 19 from the same content, so the hot level never reaches the
final container. Measured (back-to-back, same machine, warm-up enforced —
the 0.9.22-era lesson that first runs after a rebuild are page-cache cold):

  hot_level:    3       2       1      -3      -5      -7
  import MB/s:  96.5   99.8   106.4   109.1   107.3   100.1
  ensure us/ch: 79.2   68.6    60.8    50.1    49.5    49.7
  p50 us/chunk: 75.6   72.9    71.5    65.8    65.9    86.0

-3 shipped as the default (CSAVES_HOT_LEVEL still overrides). A later
same-session run measured import ST 113.0 / MT 173.0 MB/s. Gates: jj
verify 946/946 TOTAL x3.36, 728-chunk drained container x7.51 (identical
across every level), survival x4.01 ok=1280, bench_size compact x0.08,
enc_fp 6d240d7166cdda99 / 12130 / 9971843 unchanged, tests 56/56.

- also recorded: the u8-pipeline WIP (byte-per-voxel unpack + AVX2 bits4
  kernel + u8-twin scans/packs, ~860 lines) measured as a REGRESSION
  back-to-back (import 95.7-99.9 vs 103.7, unp 2532 -> 3226 ns/call, enc
  +12%, shift +20%) despite winning the isolated probe (8.9 vs 10.5 us on
  synthetic bits4 data) — the real corpus bits4 call runs 9.4 us under the
  kernel vs ~6.8 us under the plain u32 scalar loop. Parked on branch
  wip-u8-pipeline with the full differential-test set; the lesson mirrors
  0.9.23's: corpus microdata beats isolated micro-benches, and
  per-stage timers on the real corpus are the only honest referee.

0.9.24 CI verdict + probe window (tree back at 0.9.24; negative results
recorded so nobody re-digs these holes):
- CI (b250095): 7/7 green, import ST 107.8 / MT 203.0 MB/s, jj TOTAL x3.36
  verify 946/946. The previous runner generation (5d32583 artifact) measured
  166.9/336.8 but its anvil controls are 1.35-1.47x faster (read 1197 vs 817,
  write 92.6 vs 68.9) — cross-runner deltas are noise; normalized, ensure
  improved -27% and split is unchanged (also proven locally: the split stage
  is byte-identical L3 vs L-3, scan 165.8 vs 164.4 ms).
- ensure_blobs fully decomposed (temporary ensx timers, reverted): 53.3
  us/chunk = slab compress 28.2 (53% — ~486 MB/s over the 13.7 KB frame,
  the zstd fast-level codec floor; the -3/-5/-7 plateau was this, not a
  non-zstd term) + tail block 9.7 (18% — 1614 per-tail small-buffer zstd
  calls at 4.3 us each) + dedup/build/append/inserts ~7.6 + ~7.5 glue. No
  hidden non-compression fish; the next ensure-class candidate is tail
  batching (~+4-5% ceiling).
- AVX2 bits4 unpack kernel: FINAL NEGATIVE, two-stage proof. Stage 1 (real
  corpus probe, wip-u8-pipeline branch): the kernel's separate byte
  histogram is 8.6 us of the 9.0 us call — unpack-only 3.7 us, memcpy floor
  0.16 us. Stage 2 (hist-free kernel transplanted into the u32 path on
  main, 58/58 tests, enc_fp exact): in-bench unp REGRESSED 35.2 -> 42.3 ms
  (8.7 vs 6.8 us/call) — the fused counts+push pass (~4 us) is the shared
  floor of BOTH formulations; splitting extract from counts adds a pass
  that costs more than the SIMD extract saves. Full revert. A future win
  needs a fully-fused SIMD counts design (pshufb/SAD accumulators, ~+5%
  class) — noted, not dug. Branch wip-u8-pipeline keeps the full u8
  pipeline + probes for reference.

## 0.9.25 — pack is the wall; parallelize it

The 0.9.24 bench hides the real distribution: import 0.21 s, pack 10.2 s —
the storage-conversion pipeline spends ~98% of its wall in zstd-19 slab
compressions (2.5-4 MB/s per core; 8.58 s on the 4-core CI runner). The
first pack census (new SPEED pack-census line): comp 7.2-7.5 s cumulative,
build 0.62 s, recipes 0.03 s, victim reads 15 ms — compressions are the
pass.

flush_slab_cands restructured into three phases: build all group variant
streams serially (GLOB conversion order preserved exactly — registry gids
and streams byte-identical), compress every variant through a scoped
work-stealing pool, then append/update in the serial order. Per-task
timing proves real overlap: wave 1 ran the 8.34 MB canon (3.32 s) and the
12.10 MB glob (4.86 s) concurrently (8.18 s serial); the whole pack fell
10.2 -> 8.0-8.2 s on 2 cores. CI 4-core expectation: 8.58 s -> ~4.5-5.5 s.
The drained container's packed-slab segment is byte-identical; the tail
segments were ALREADY non-deterministic on 0.9.24 (self-comparison:
1443173 vs 1443174 bytes) — the container-fp line (new) makes this
visible per run instead of a false identity claim.

MT write path got its first exclusive-lock census (SPEED mt-lock-census):
append 2.5-2.8 ms, slabins 1.3, bump 1.4, lastw 0.3 — all ~1-2% of wall,
NOT the story; idof (per-hash blobs.read() re-acquisition inside recipe
encode) measured 10.8-12.3 ms (660 ns per contended acquisition). Fixed:
one guard per recipe encode; supersede de-nested out of the recipe_batch
Mutex. MT recipe stage 20-30 -> 6.8 us/chunk, but the MT wall band stayed
148-181 MB/s — the remaining MT ceiling is split-stage cache/DRAM
contention (222 -> 264 us/chunk cumulative), not locks. Deferred-encode
(moving the 47% enc machinery to pack) was scoped and parked: recipes
reference ids (mode 2/6), so an id-remap pack conversion is designable,
but hot-tier raw storage changes dedup semantics, slab sizes and the
shift-pass input — a multi-day project, documented for a future tick.

## 0.9.26 — zstd-MT inside the big pack streams (intra-frame nbWorkers)

The 0.9.25 CI verdict re-measured the wall: pack 7.45 s with comp = 6.63 s
of it, and the comp wall is the LONGEST single stream (12.1 MB GLOB
variant, 4.86 s serial z19) — parallel-across-streams cannot divide one
stream. Phase B now rank-selects the k = cores/2 longest variant streams
(deterministic: lens are content-determined, ties break by group order)
and compresses them with zstd nbWorkers = cores/k inside ONE standard
frame; the rest of the bigs stay ST in the scoped pool and all tasks run
LONGEST-FIRST. The auto budget is a deliberate fix over the obvious
cores/big_cnt split: with 6 big streams on 2 cores that formula gives 0
workers (MT silently dead) and on 4 cores it under-workers every pole;
concentrating on the poles is what binds the makespan (LPT).

Measurements. Local 2-core sandbox (work-saturated: wall ≈ work/cores,
so parallelism of any form is ~neutral): baseline MT-off pack 8.39 s
(comp 7.62 s, mtjobs:0) vs auto-MT 8.36 s (mtjobs:4, 22.2 MB intra-frame)
vs explicit-2 8.12 s (mtjobs:6, 32.5 MB — oversubscribed, no better).
Explicit-2 DID prove the pole shrinks: mtns 11.37 s aggregate in a 7.37 s
wall with the ST streams overlapping. The pack win is expected on
pole-bound 4+ core hosts: CI 6.63 s comp wall = 4.86 s pole + overlapped
rest -> pole/1.85 ≈ 2.6 s, work/4 ≈ 3.7 s floor -> pack ~4-5 s expected
(-35-45%). MT frames decode with the stock single-threaded decoder,
deterministic per (input, level, workers); drained container fp moves
(job boundaries) — the 0.9.24/25 tail-segment nondeterminism precedent,
now also for the packed-slab segment's frame bytes. Sizes: 1.65 vs
1.66 MiB (x7.53 vs x7.51) — within noise.

libzstd gained ZSTD_MULTITHREAD (zstdmt feature): ST paths (hot tier,
recipe flush, small streams) byte-identical — enc_fp
6d240d7166cdda99/12130/9971843 EXACT re-verified. Gates: 57/57 tests
(new pack_mt_deterministic_and_wire_compatible), jj verify 946/946
x3.36, survival x4.01 ok=1280, bench_size x0.08, verify 728+218 bad=0.
Census fields mtjobs/mtns/mtbytes report the intra-frame share; knobs
CSAVES_PACK_MT_MIN (1 MiB default, 0 = off) and CSAVES_PACK_MT_WORKERS
(explicit, all bigs — may oversubscribe).

## 0.9.27 — cold tier 19 -> 12: pack 6.8x; auto-MT gated to 4+ cores

The 0.9.26 CI run (42022752248, green 7/7) returned a NEGATIVE pack
verdict: 7.45 -> 7.87 s (+5.6%). Root truth: the private-repo CI runner
is 2-core, and a 2-core pack is WORK-BOUND (wall ~= work/cores; comp
6.63 s ~= 13 core-s of z19 over 2 cores). No parallelism shortens a
work-bound wall; intra-frame MT only added job overhead + scheduling
contention. The auto-MT budget now engages only at cores >= 4 (where
work/4 ~ 3.3-3.7 s < the 4.86 s pole -> pole-bound, MT is the right
division); 2-core hosts run the pure ST pool (0.9.25 behavior); explicit
CSAVES_PACK_MT_WORKERS still overrides anywhere.

The big pack lever turned out to be LESS WORK, not more cores. Cold-level
sweep (JJ corpus, MT off, warmup-first, same machine):

  cold 19: pack 8.39 s (comp 7.62 s), container 1.66 MiB (x7.51)
  cold 15: pack 2.53 s (comp 1.77 s), container 1.86 MiB (x6.70)
  cold 12: pack 1.23 s (comp 0.52 s), container 1.88 MiB (x6.62)

Default cold_level 19 -> 12. Pack wall 6.8x smaller; comp phase 7.62 ->
0.52 s (below 12 the wall is the BUILD phase — variant stream assembly +
GLOB conversion — so lower levels stop mattering). Size cost +13% on the
noisiest corpus only; the size gates did not move: survival x4.01
identical, bench_size compact x0.08 identical (gate 0.15), jj x3.36 at
+0.6% (1.60 -> 1.61 MiB, 946/946 verify). Production frame: pack = the
background drain rate competing with tick threads for cores; 6.8x less
maintenance CPU shrinks the 2 GB hot-tier incident class by the same
factor. CSAVES_COLD_LEVEL=19 remains the size-obsessed opt-out. Gates:
57/57 tests, enc_fp 6d240d7166cdda99/12130/9971843 EXACT, survival
x4.01 ok=1280, bench_size x0.08, verify 728+218 bad=0, import 113.7 MB/s
(warm read 1577.9) — import/read untouched, container x6.62.

## 0.9.28 — palette-memo in section_to_glob: pack build phase 4x

At z12 (0.9.27) the pack wall moved from compression to the BUILD phase:
build 632 ms vs comp 511 ms of the 1.23 s pack. Decomposition by reading
section_to_glob found the fish: the voxel loop called
reg.get_or_insert(st.clone()) PER VOXEL — 4096 full-state hash lookups +
4096 String clones per section, for a palette holding at most n distinct
states. The fix is a lazy per-palette-index memo (pgid: Vec<u32> seeded
with u32::MAX): states still register on their first REFERENCING voxel in
the same voxel order (gid assignment order and registry bytes unchanged;
unreferenced palette entries still never register), so the hash cost
drops from 4096 to n lookups and n clones per section.

Measured (JJ corpus, local 2-core, z12): pack 1.23 -> 0.76 s (-38%);
pack-census build 631.6 -> 156.8 ms (4.0x); comp unchanged (511 ->
515 ms); container 1.88 MiB (x6.61 vs x6.62 — within the +-46 B
run-to-run tail noise). Import path untouched (section_to_glob is
pack-only); enc_fp 6d240d7166cdda99/12130/9971843 EXACT re-verified.
Gates: 57/57 tests, verify 728+218 and 946/946 bad=0 (jj_profile).

Sanity note recorded: across this sandbox's recycle the jj_profile
container grew 1.61 -> 1.83 MiB with IDENTICAL blobs/dedup/enc-modes and
a degenerate 2239 B trained dict (was ~100 KB) — present with AND without
the memo (old code measures 1.83 too), so it is an environment-level
dict-training anomaly to chase separately, not a memo regression. CI
remains the honest size judge (stable runner image).

## 0.9.29 — cold tier z12 -> z6: the memo moved the knee

With the build phase 4x cheaper (0.9.28 memo), compression is no longer
hidden behind variant assembly, so the cold-level sweep was RE-RUN
(JJ corpus, local 2-core, warmup-first `3 2` -> `1 2`, MT off):

| cold | pack | comp | build | container |
| ---- | ---- | ---- | ----- | --------- |
| 12   | 0.79 s | 538 ms | 157 ms | 1.88 MiB (x6.61) |
| 8    | 0.56 s | 297 ms | 185 ms | 1.89 MiB (x6.59) |
| 6    | 0.45 s | 213 ms | 156 ms | 1.94 MiB (x6.42) |
| 4    | 0.29 s |  60 ms | 158 ms | 2.15 MiB (x5.81) |

Default 12 -> 6: -43% pack for +3% container. Size gates byte-flat at
z6: bench_size compact x0.10 (3.58 vs 3.57 MiB, gate 0.15), jj TOTAL
unchanged (1.83 MiB x2.95 on this env), survival TOTAL x3.12 vs x3.13
at z12 — level-flat on THIS env; the historical x4.01 vs today's x3.13
survival spread across sandboxes is the same env-level dict-training
anomaly documented above (z12 itself no longer reproduces x4.01 here),
not a level cost. verify 946/946 + survival ok=1280 bad=0; enc_fp
6d240d7166cdda99/12130/9971843 EXACT.

Below z6 the wall is the BUILD phase again (z4: comp 60 ms vs build
158 ms), so z4's extra -16% pack buys +11% storage — wrong trade for a
storage engine. z6 = the knee. CSAVES_COLD_LEVEL=19 opt-out unchanged.
Auto-MT note: local 2-core default run shows mtjobs:0 (the cores>=4
gate from 0.9.27 working as designed).

CI verdict v0.9.28 (a9467f5, 2-core runner) received before this push:
pack 1.43 -> 0.53 s (-63%), pack-census build 803 -> 97.6 ms (8.2x),
import 174.7 MB/s ST / 335.9 MT (CI best), warm read 1983.9 MB/s,
cold read 794.9 MB/s, container x6.61 flat, compact x0.10 — the memo
delivered on CI with zero size movement.

## 0.9.30 — dict training: the arrival lottery, measured and closed

Root cause chain of the "degenerate dictionary" anomaly: the old pool was
`Vec` truncated to the FIRST 2*dict_train arrivals -> which 400 payloads
those are depends on fs::read_dir order / spawn direction -> the COVER
trainer's output swings 4.6-53 KB across 200-sample subsets of the SAME
982-sample stream (offline study, examples/dict_sel.rs) -> arrival order
decided dict quality. Full stream in hash order: stable. Numbers logged on
this env: 1662 B / 48656 B / 82873 B / 114688 B / 5302 B / 6450 B dicts
from the same corpus under different selection policies; container did NOT
move with any of them (pack Phase-B variant streams compress dict-less —
the dict only feeds ensure/hot-frame/inline-tail paths).

Fixes shipped: content-hash bucket reservoir (dedupe by content, keep
largest per bucket, train on hash-ordered prefix capped at 32 samples /
256 KiB = 35 ms trainer wall cost, pack parity with 0.9.29 at 0.45 s);
retrain gate fresh>=dict_train/4 or +25% buckets; cold tier dict-less
(pack-path consistency, wire-compatible via per-blob dict_id stamps);
regression test pins byte-identical dicts across arrival rotations;
trainer telemetry (size/ms/samples/bytes) in the trained log line.

A/B honesty: bench_size compact 3.58 vs 3.80 MiB and jj 1.61 vs 1.83 MiB
reproduce on UNCHANGED 0.9.29 code across sandbox states — environment
drift, NOT dict effects (git-stash A/B today). CI's stable runner remains
the size judge. Parked follow-up (the real size lever): dictionary into
pack Phase-B variant streams — wire change, dict_id per stream, decode
paths; multi-hour project.

## 0.9.31 — CI-caught chunk loss under concurrent supersede+pack (z6 timings)

CI on 0.9.29 (d4a6299) failed `concurrent_supersede_during_pack_keeps_
newest_state` with "chunk 19 lost" — the FIRST failure of this test ever,
and only at z6 pack speed on the 2-core runner (locally 30/30 green).
Log forensics: `LEAK: batch_idx miss` on seg-6 correctly poisoned that
segment, then seg-1/seg-7 FULL rewrites ran — one of them dropped a
segment holding a recipe that arrived AFTER the rewrite's snapshot.

Mechanism: the writer thread's 192 KiB recipe-batch auto-flush lands new
batch recipes into the CURRENT writer segment; when that segment is the
one being compacted, a flush between the packer's snapshot and the segment
drop adds pointers the snapshot never saw. The per-key pointer re-check
only catches STALE pointers, not new ones. The drop then deleted another
thread's live bytes.

Fix: fixpoint re-snapshot — the move loop repeats until a full pass finds
no recipe pointing at worst_id (after seal() no new appends target the
segment, so only the one in-flight flush can add; each pass is bounded
work). Poison escape: a pass that hits mark_broken() leaves its key
pointing at worst_id forever — break out and let the PARTIAL/KEPT guard
handle it (also prevents a spin the old code never had). Second CI failure
(unchanged_write_skip left:81 right:1) was the process-global
ST_WRITE_SKIP counter bumped by parallel tests mid-assert — assertions now
use the new per-store write_skips counter.

Gates: 58/58 x3 suite runs, race test 30/30, jj verify 946/946, survival
ok=1280, enc_fp EXACT, pack 0.47 s (parity), container x6.42.

## v0.9.32: split-path profiling pass (import per-chunk budget)

Warm import-MT run on the dim corpus (728 chunks, 2 threads) decomposes
per chunk as: split ~225 µs (scan-root walk 48 incl. ~40 heightmap work,
sections lite-parse 21, encode 105, shift 48-51, tail 3), ensure_blobs
~60 µs (hot-slab zstd — real work), recipe ~6 µs. The split stage timers
are stable to ±1 µs across runs; wall clock on this 2-core sandbox swings
±15% and must not be used for A/B verdicts at this granularity.

Shipped: heightmap dense-form memo (xxh3_128 of the raw 296-byte
LongArray -> dense bytes, thread-local, pure-function cache; scan stage
232.8 -> 222.8 µs/chunk) and once-resolved env toggles on the split path.
enc_fp EXACT (6d240d7166cdda99 / 12130 / 9971843).

Negatives kept on record: byte-pair nibble unpack regression (LLVM already
vectorizes the constant-shift loop; reverted), and the split-minus-scan
delta identified as nested-timer overhead (~70 Instant::now() sites inside
the outer WST_SPLIT wrap on a virtualized host), not work. Next levers by
size: shift-pass internals (identity/candidate build ~21 µs/chunk of the
pass), hot-slab zstd inside ensure_blobs, SWAR nibble histogram in
unpack_counts_vox.

## 0.9.33 — pack build goes parallel: the 3-stage GLOB split + cold z4

Build sub-census (new PK_B_GLOB_NS/PK_B_TAB_NS in pack-census): of the
serial Phase A wall, `bglob 145.4 ms` = 95% — the GLOB conversion of every
member — and `btab 12.4 ms` = entry tables + raw concatenation. Phase A was
`groups.into_iter().map()`: strictly serial while the Phase B compress pool
waited. The conversion cannot be naively parallelized because the registry
gid assignment ORDER is the wire format. Split (chunknbt):

- `section_glob_pre` (pure): magic/f1/f2 gates, palette parse, dense
  unpack, bounds, first-reference order — all parallel
- `section_glob_resolve` (serial): palette refs -> gids via get_or_insert
  in first-reference order — the EXACT mutation sequence of the old
  per-voxel memo loop (including: the violating >65535 state registers,
  later refs of that member do not; per-member len() <= 65536 gate)
- `section_glob_finish` (pure): gid map + bounded RLE admission scans +
  winner materialization + header + tail — all parallel

`section_to_glob` = the composition; all glob unit tests pass unchanged and
the conversion bytes are identical by construction (container x5.81 on dim
= the plain z4 sweep's bytes). Ported the v0.9.21b bounded-scan lever to
the gid admission scans (early exit once `total > len/div` — decisions
identical because the exact surplus length is only ever used to fail the
same admission rule) and skipped the scans entirely at div=0. Fused the
bounds pass into the first-reference scan (one voxel pass instead of two).
Once-resolved env toggles on the pack path (GID_RLE_DIV was read PER
SECTION, GLOB_DEBUG twice, FORCE_GLOB per group).

Cold 6 -> 4: with build parallel, z4's comp 60 ms no longer hides behind
build 160 ms — z4 pack 0.30 s vs z6 0.44 s (-32%) for +10% container
(compact x0.10 -> x0.13 vs gate 0.15); z2 measured pack-flat (0.29 s, comp
40 ms) — z4 is the knee.

Dim corpus 2-core (warmup-first): pack 0.48 -> 0.24 s (-50%), build 152 ->
95 ms, comp 207 -> 57 ms. Import untouched: enc_fp EXACT
(6d240d7166cdda99 / 12130 / 9971843). Gates: 58/58 tests, verify 728+218
bad=0, survival ok=1280 bad=0, bench_size compact x0.13.

Next levers by size: import split is now the bigger wall (211 ms vs pack
240 ms) — shift-pass internals (48-51 us/chunk) and the encode stage
(105 us/chunk, AVX2-verdict-negative territory), then stage1 String
interning on the glob parse path, then z2 (comp 40 ms) only if container
headroom allows.

## 0.9.34 — cold back to z6 (CI verdict): the real-world corpora own the level

0.9.33's cold 6 -> 4 passed every local gate and broke both real-world ratio
floors on CI: jj-bench TOTAL x2.59 (floor 2.6), survival-bench TOTAL x2.50
(floor 3.0). Attribution: entities x3.03 -> x2.96 (noise), region x2.83 ->
x2.52 — the whole miss is the cold level inside the region streams. Survival
section data (ore noise) is where cold level bites; the dim corpus is
GLOB-flat and hides it (its compact gate x0.10 -> x0.13 stayed green).
z5 interpolates below the survival floor as well, so z6 is the fastest level
that clears both gates with margin (survival +4%, jj +10%). The 3-stage GLOB
split and the parallel build stand unchanged. The level decision now carries
the recorded lesson: it is a storage parameter owned by the survival/jj
ratio floors, never re-decided on the local speed corpus alone.

CI at z4 measured pack 0.19 s; z6 should land near 0.3 s — still ~25x under
the 0.9.26 wall, and the import scan stage (189 us/chunk, 73% of the split
wall) is now the biggest wall in the engine. Next levers by size: the scan
stage, then shift internals (48-51 us/chunk), then z2 only if both ratio
floors gain headroom from a codec-side compactness win.

## 0.9.35 — concurrent supersede+pack loss fix (CI verdict)

v0.9.34's CI test job exposed a REAL chunk-loss race the concurrent
supersede test was built to catch: the supersede path eagerly removed the
key's batch_idx entry before the new pending recipe was queued, so reads in
the window resolved to None ("chunk 16 lost") and a concurrent segment
rewrite poisoned on batch_idx misses (8 in the CI log). Supersede now queues
the new pending FIRST, resolves the committed version through the captured
location (load_batched, no batch_idx re-consult), and the batch_idx entry
lives until the flush overwrites it; the rewrite loop skips keys with a
newer pending entry (check-and-push is atomic under the batch lock) and
poisons out when a stuck pending would never drain. The two glob form test
failures were env-race flakes (set_var vs the once-resolved OnceLocks from
0.9.32/33) — all 14 test env writes are now override statics. Import path
byte-identical: enc_fp 6d240d7166cdda99 / 12130 / 9971843 unchanged; 58/58
tests x8 at 8 test threads.

## 0.9.37 — the shift pass stops hashing: fingerprint dedup without SipHash

New probes first (split_stage_json): scanr = scan_root total, encl =
encode_section_list_lite total, tailb = build_scan_tail; shiftc =
candidate-loop counters, shiftb = per-build timers, shiftt = tail
materialization + pass total. Same-instrumentation before/after on the
dim corpus (2-core, warmup-first, measure `1 2`):

| block | 0.9.36 | 0.9.37 | note |
|---|---|---|---|
| scan | 219.6 | 207.3 µs/chunk | whole scan_split |
| scanr | 24.7 | 25.0 | root walk (secb 21.7 fused parse) |
| encl | 138.4 | 137.1 | encode list (enc 105.7 = unp 48.4 + pack 41.5) |
| shift | 49.2 | 38.0 | builds 18.0 + matching 6.7 + tail 9.4 |
| import ST | 112.9 | 117.5 MB/s | p50 60.2 -> 58.4 us |
| import MT | 180.4 | ~186 MB/s | 2 threads, noisy shared host |

The matching-loop cost was the per-i candidate-dedup HashMap<Hash, ()>
(~0.5M SipHash inserts per import at ~25 ns + growth reallocs), not the
447k plane-fingerprint compares (99.93% reject at plane0, ~2 us/chunk).
Fix: u64 bloom bit + reused scratch Vec (capacity == CANDIDATES, exact
scan only on same-slot collisions). The identity-intern table, the
ref-materialization maps and the per-thread enc cache switched to the
RawBuildC bucket hasher (fingerprint/id keys; full-key Eq unchanged).

Documented negative: nibble unpack kernel rewrites (byte-plane extract +
4-way interleaved sub-counters) measured 2.57 -> 3.78 us/call (branchy
worst-tracking) and 2.57 -> 3.38 (predicted-not-taken checks) — both
reverted. Second independent confirmation of the 0.9.32 verdict: LLVM
already schedules the constant-shift nibble loop better than the
byte-pair form.

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843),
pack 0.42 s at z6, container 1.94 MiB x6.42, bench_size compact x0.11
(gate 0.15), survival verify ok=1280 bad=0, verify 728+218 bad=0.

## 0.9.36 — one walk, not two: the root walk owns the section parse

Method note first: the container-fp line is NOT a stable pin on this
sandbox — four different values across runs of near-identical code
(member bytes 1728105..1728193), including a run with the change fully
disabled. The import-level enc_fp is the honest pin (6d240d7166cdda99 /
12130 / 9971843 re-verified EXACT with the fused path on).

Per-chunk scan budget after the fusion (dim corpus, 2-core, warmup-first,
measure round 1 2):

| block | µs/chunk | note |
|---|---|---|
| enc (misses) | 104.3 | 12146 structurally unique sections x 6.3 µs |
| shift | 48.8 | build ~20.6 + match ~28 |
| sections walk (secb) | 22.1 | the fused direct walk (was skip 6 + parse 21) |
| hash_lite (sh) | 16.9 | 95434 sections x 129 ns |
| tail + misc | ~20 | build_scan_tail copy + SecBytes pushes |
| scanp (retry parse) | 0.03 | was 21.1 — the second walk is gone |
| root hmap / skip | 0.07 / 2.3 | post-memo / generic keys |

The enc-cache verdict: misses are NOT capacity — 12146 per round at both
32 MiB and 256 MiB caps; they are the unique sections (83288/95434 hit =
87% within one import; rounds are separate processes). Encode kernel next:
unp 2.58 + pack 2.20 µs/call = 88 µs/chunk is the biggest single block
left (documented-negative territory — probe before transplant).

New telemetry: the `"root"` block in split_stage_json
(secb/hmap/skip ns-n pairs) decomposes scan_root per branch; hdr is the
remainder. Net measured: scan 222.8 -> 216.8 µs/chunk, import ST 112.5 ->
114.8 MB/s, import work −2.7% — small but structural, and the walk now
owns the parse.

## 0.9.39 — the hot tier stops pretending: raw slab frames + the tail joins the frame

New `stages-ensure` telemetry (kept in `ensure_stage_json` / SPEED
`stages-ensure`) decomposed the ensure stage on 0.9.38 (58.3 us/chunk,
21% of the ST import wall; 2-core shared sandbox, warmup `3 2` -> `1 2`):

| block | us/chunk | note |
|---|---|---|
| slab flush | 34.9 | 160 frames; sub-timers: comp 30.7, append 4.4, build 2.6 |
| tail blob | 10.5 | 728 standalone ensure_blob calls, every chunk |
| dict_sample | 3.6 | sampling misses at flush time |
| per-blob fallback | 1.8 | 568 below-threshold batches (mostly map hits) |
| batch map phase | 1.6 | blobs.read + in-batch seen set |

The smoking gun: `slab_comp` = 139 us PER ~34 KiB frame, and a standalone
probe (`zstd::bulk::compress` loop) measured **155-220 us for a 34 KiB
frame at EVERY level from -3 to 3** — the cost is zstd frame machinery
(window/huffman/literal-table setup), not match-finding; level changes
moved nothing, a raw memcpy moved ~0. The hot tier is transient material
the pack pass re-stores at the cold level with a 4-8 MiB window — a
per-chunk zstd frame is work the drained container never sees.

Shipped (both changes output-pinned, drained containers byte-identical
modulo the documented ±19-byte tail-order jitter):

1. **Raw hot slab frames** — `flush_hot_slab_group` stores the SLAB V2
   stream verbatim behind a `dict_id=0xFF` sentinel (dict ids 0..=254;
   slabs never carried a dict; old containers can never contain it). One
   hook at the top of `decompress()` covers slab cache, read_blob, batch
   loads, scan rebuild and pack re-group; the slab header is patched in
   place in front of the stream (one full-frame memcpy saved). Undrained
   containers honestly hold raw member bytes until pack drains them.
2. **Tail joins the frame** — a blob-eligible tail (>= `tail_blob_min`)
   rides the hot-slab batch as its LAST member: no standalone
   compress/record/append per tail, the batch map phase dedups it for
   free. Below the slab threshold it rides the per-blob fallback; tiny
   tails stay recipe-inline (n=0 on this corpus — every tail >= 512 B).

Measured: import ST 115.5 -> **130.2 MB/s (+12.7%)**, wall 0.205 -> 0.182 s
(2.39x the same-run Anvil write); import MT 189.8 -> 201.2 MB/s (the MT
band bounces ±25% on this co-tenant host — ST is the stable metric).
Ensure 58.3 -> 31.1 us/chunk (-47%): slab flush 158.9 -> 67.7 us
(comp 139.3 -> 6.4 us = the sentinel copy, tail_blob 10.5 -> 0). Warm
read 1541 -> 1459 MB/s and cold read 547 -> 557 MB/s — both inside the
host noise band; warm read of a raw member is a memcpy from the slab
cache, cold reads move more bytes but skip the decompress.

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843), jj
verify 946/946 bad=0, verify 728+218 bad=0, survival TOTAL x3.12 ok=1280
bad=0 (documented post-0.9.34 z6 baseline; the historical x4.01 spread
across sandboxes is documented in the 0.9.29 section), bench_size compact
x0.11 (gate 0.15), container 1.94 MiB x6.42 unchanged.

Next honest levers after this: cross-chunk slab batching (needs deferred
recipe encode — the id-window problem), dict_sample gating (+1.3% class,
ratio-risky), thread-local scratch in the split stage (+1.5-2% class).

## 0.9.38 — the encode loop closes: three probe verdicts, one presize

In-bench probes on the last unprobed encode corners (all output-pinned,
enc_fp EXACT throughout; 2-core shared sandbox, warmup `3 2` -> `1 2`):

| probe | result | verdict |
|---|---|---|
| RLE-A transitions fused into unpack_counts_vox | unp 2593 -> 3906 ns/call (+50.6%), scan 204.7 -> 231.0 us/chunk, import 119.4 -> 108.1 MB/s | **NEGATIVE, reverted** — the extract+counts loop is IPC-bound (~4 uops/voxel); ~3 register uops (cmp+add+mov prev) cost +50%, the saved bounded SIMD walk is 305 ns/call. Third negative of the class (0.9.24 AVX2 bits4, 0.9.37 nibble). |
| hash_lite fold into the direct walk (two-level per-key scheme) | floor = the data hash itself: one xxh3 update over the 8 KB LE longs (~270 ns/block_states section) is mandatory dedup work; fold saves only dispatch/palette re-walk ~25-30 ns/section | **PARKED, sized** — +2%-class for a three-implementation hash rewrite (feed / feed_lite / walker must agree; probe_sec pins cross-currency equality). |
| pack kernel SWAR/pdep headroom | packb: bits2 2091 ns, bits3 4293, bits4 4869 per 4096-voxel call = ~3.1 cycles/voxel vs ~2-2.5 scalar IPC bound | **PARKED** — ~1.3x projected on 14 us/chunk; the 0.9.17 group-split already broke the accumulator chain, remap-gather dominates. |

Shipped: section_encode_view presizes `out` from the known extents (dense <=
input longs since out_bits <= in_bits; + light arrays + 1.1 KB palette
headroom) — the old 5-byte Vec grew ~9x per section inside the enc timer.
pack 2199 -> 2078-2167 ns/call across runs; import flat within host noise
(ST 117.3-119.4 band; MT bounces +/-25% run-to-run on this co-tenant host —
the MT-ensure timer measured 54.3M vs 82.3M ns on identical bytes across two
builds, so MT deltas below that band are not attributable).

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843), verify
728+218 bad=0, pack 0.41 s, container 1.94 MiB x6.42, warm read 1490.2 MB/s.

## 0.9.40 - ensure stage: splice memmove + SipHash dict sampling

Fresh per-frame telemetry on flush_hot_slab_group after the 0.9.39 raw-frame
change (same machine, warmup 3 2 -> measure 1 2, ST):

| timer | 0.9.39 | 0.9.40 | note |
|---|---|---|---|
| slab_comp | 6.2 us/frame | 0.05 | splice(0..0) header memmove -> reserved header |
| dict_sample | 17.6 | 9.2 | SipHash DefaultHasher -> xxh3_64 over the same truncated bytes |
| ensure total | 30.4 us/chunk | 28.6 | |
| import ST | 120.8-128.4 (med 125.7) | 126.8-131.3 (med 131.0) | +3-4%, A/B 3 rounds each side |

dict bucket keys shift with the hash change (which samples train the hot
dict); the determinism contract (same content set -> same dict) holds, and
member raws / enc_fp / drained container are untouched: enc_fp EXACT
(6d240d7166cdda99/12130/9971843), 58/58 tests, verify 728+218 bad=0, MT 195.8
(in the +-25% band). Next honest levers unchanged: cross-chunk slab batching
(deferred recipe encode), thread-local scratch in the split stage (+1.5-2%
class), dict_sample is now at its data-hash floor.

## 0.9.46 - strip-canonical contract legends; the erosion lever (huge-world win survives real sessions)

0.9.45's raw-only contract flips on the first byte-different re-save. A real
Paper session relights and re-saves contracted chunks (fresh section light,
bumped InhabitedTime/lastUpdate, HashMap-key-order drift) the moment a player
merely walks past — one relight session over the 728-chunk real corpus
eroded 728/728 contracts back to full storage (1.45 MiB, x0.32). The mode-7
contract now carries a SECOND hash — the strip-canonical legend
(`chunknbt::canonical_contract_hash`): order-free structural digest minus
per-section SkyLight/BlockLight/starlight bags, isLightOn and the root
lastUpdate/LastUpdate/InhabitedTime counters — everything a pristine chunk
regenerates anyway. Wire: `[mode 7][raw 16B][canon 16B]` = 33 B; 17 B
raw-only contracts still parse (old containers behave as before); the O(1)
raw fast path is untouched; canon only arbitrates byte-different re-saves.
`CSAVES_PRISTINE_CANON=0` restores raw-only formation.

| gate | 0.9.45 flag OFF | 0.9.46 flag OFF | 0.9.46 pristine + canon ON |
|---|---|---|---|
| bench compact (64x5) | 1.24 MiB x0.08 (64x4) | **1.24 MiB x0.08 (identical)** | n/a (edited corpus) |
| erosion A/B (728 ch, one relight session) | n/a | n/a | **pristine=728 stored=0, compact 0.02 MiB x0.0054 vs anvil (-99.5%)** vs raw-only pristine=0 stored=728, 1.45 MiB x0.32 |
| pure-pregen 96x96 (9,216 ch) | 0.28 MiB x0.01, 26 B/chunk | unchanged | **0.43 MiB x0.01 (42 B/chunk all-in; 33 B legend)** |
| bench_size B2 relight session | n/a | n/a | **318 relit re-saves -> 318 contracts kept (assert: zero flips allowed)** |
| D verify | 152 pristine sampled | 1957 chunks semantic-equal | pristine sample 152, gate ratio 0.01 (<= 0.02) |
| tests | 63/63 | **66/66 x3** (+relight-keeps-contract; +raw-only opt-out fallback; +canon strip-context isolation) | same binaries |
| enc_fp | EXACT 6d240d7166cdda99/12130/9971843 | **EXACT** | pin |

Honest costs and side measurements: canonical formation ~1.6-1.8 s per 728
corpus chunks (~2.3 ms/chunk, full parse + order-free structural digest on a
2-core sandbox) — rides the background import drain; canon re-save check
costs the same class once per byte-different re-save. keys_probe documents
that the import REBUILD form differs from the raw payload under the
canonical hash for 204/728 corpus chunks (CSEC canonical palette order vs
vanilla order in rebuilt sections) — irrelevant for contracts (Paper
REGENERATES contracted chunks; the re-save comes from Paper's own
serializer), pinned in pristine_canon_probe's drift diagnostic.

## 0.9.45 - pristine IMPORT contracts; the huge-world lever measured (-99% class)

`CSAVES_PRISTINE=import|2` (or `CSAVES_PRISTINE_IMPORT=1`): the import/strip
path (background Anvil drain = how huge pregen worlds enter) forms recipe
mode 7 regeneration contracts too. Whole-mode stores stay exempt.

| gate | 0.9.44 flag OFF | 0.9.45 flag OFF | 0.9.45 pristine pure-pregen / import |
|---|---|---|---|
| bench compact (64x4) | 1.24 MiB x0.08 | 1.24 MiB x0.08 (identical) | n/a (edited corpus) |
| pure-pregen 96x96 (9,216 ch, 0 edits) | not measured | n/a | **0.28 MiB x0.01 vs anvil (-99.2%), 26 B/chunk, blobs=0** |
| real-corpus import (728 ch, anvil 4.47 MiB) | 1.17 MiB x0.26 (import exempt) | unchanged | **0.01 MiB x0.002 (-99.8%), pristine=728 stored=0** |
| D verify | 1661 chunks, 0 contracts | 1661 chunks | 152 pristine sampled (0 stored to verify) |
| tests | 62/62 | **63/63** (+import-contract flip/persist; live-only exemption pinned) | same binaries |
| enc_fp | 6d240d7166cdda99/12130/9971843 | **EXACT** | pin |
| jj / survival | x3.71 ok=946 / x4.28 ok=1280 | unchanged (CI flag-off) | n/a |

CI size-bench gains the pristine pure-pregen gate: `CSAVES_PRISTINE=1
bench_size 96 0`, compact ratio <= 0.02 AND pristine sample >= 100 (measured
locally: 0.01 / 152). Erosion model documented in README/research §9: flip
on any hash-differing re-save bounds loss-risk to chunks the server loads;
operator asserts pure-worldgen import. Probe:
`native/examples/pristine_import_probe.rs` (imports from a disposable copy —
import_dir drains its source).

## 0.9.44 - pristine-chunk elision ships (opt-in); the semantic era begins

CSAVES_PRISTINE=1 (region stores only): first-ever write = recipe mode 7
regeneration contract (payload hash, zero blobs, 17 B). Read = absent ->
Paper regenerates; different-content write supersedes via the normal
pipeline (flip-on-mismatch); identical re-save stays a contract across
restarts. Import/strip/whole-mode paths exempt; verify gains pristine=N.

| gate | 0.9.43 | 0.9.44 flag OFF | 0.9.44 flag ON (bench 64x4) |
|---|---|---|---|
| bench compact | 1.24 MiB x0.08 | 1.24 MiB x0.08 (identical) | **0.59 MiB x0.04** |
| bench fresh | 5.08 MiB | 5.08 MiB (identical) | **0.11 MiB x0.01 vs anvil** |
| D verify | 1661 chunks 25.5 s | 1661 chunks (0 contracts) | 1623 + 38 contracts, 20.5 s |
| jj_profile | x3.71, ok=946 bad=0 | pristine=0, unchanged | n/a (import path exempt) |
| survival | x4.28, ok=1280 bad=0 | unchanged | n/a (CI runs flag-off) |
| tests | 62/62 | **62/62** (2 new pristine tests) | same binaries |
| enc_fp | 6d240d7166cdda99/12130/9971843 | **EXACT** | pin |

Honest residual risk: pre-first-save mutations ride inside the contract
hash and revert on regeneration (operator-accepted, documented; shim-side
isNewChunk/InhabitedTime marker = future tightening).

## 0.9.43 - the LZMA2 encoder goes EXTREME; PPMd solid + export framing measured dead

One encode-only flag: `lzma2_compress` -> preset 6 | PRESET_EXTREME (deeper
match search). Decode side untouched (EXTREME = encoder search only, dict
stays preset-6 8 MiB) - every 0xFE stream ever written decodes as before,
both directions. Gate sweep (2-core sandbox, CI-equivalent commands):

| gate | 0.9.42 | 0.9.43 (6\|EXTREME) | floor |
|---|---|---|---|
| bench_size 64x4 compact | 1.35 MiB x0.08 | **1.24 MiB x0.08 (-8.1%)** | <0.15 |
| region slab bulk (exact) | 1,306,138 B | **1,190,263 B (-8.87%)** | - |
| jj_profile container | x3.36 smaller | **x3.71 smaller** | >=2.6 |
| survival 16x12 | x4.01 smaller | **x4.28 smaller** | >=3.0 |
| D verify (cold reads, 1661 chunks) | 26.8 s | 27.5 s | - |
| C compaction (pack wall) | 8.1 s | 22.5 s | background/watermark |
| enc_fp | 6d240d7166cdda99/12130/9971843 | **EXACT** | pin |
| tests | 60/60 | **60/60** | all |

Offline ladder on the real member stream (12,130 members, 6.29 MiB group,
`examples/lzma_preset_probe.rs`): p6 940,873 B; window lever EMPTY (p9/dict
64 MiB = +0.00% - the preset-6 dict already covers the 8 MiB slab cap); every
EXTREME variant identical 915,488 B = -2.70% (search depth is the whole win).
Bench-corpus material gains more (-8.9%) than noise-heavy JJ members (-2.7%):
repetitive terrain feeds the deeper search superlinearly.

Research tick 3 closures (docs/compression-research.md section 7): PPMd
solid-window (global-CM proxy) +6.1% WORSE than LZMA2-6 at order-16 on the
24,809,708 B corpus - second independent negative for the slab-CM frontier;
brotli -11 solid = -3.2% only, so archive export is ~3%, not a feature;
recipe batches / cold blobs / dicts are speed-pinned or negligible. The
codec era is measured-closed; what remains is semantic (pristine-chunk
elision, product decision pending).

## 0.9.42 - LZMA2 cold codec graduates to default; per-section CM measured negative

The 0.9.41 opt-in flipped to the default after the full gate sweep (2-core
sandbox, same commands as CI):

| gate | 0.9.41 default (z6) | 0.9.42 default (LZMA2-6) | floor |
|---|---|---|---|
| bench_size 64x4 compact | 1.70 MiB x0.11 | **1.35 MiB x0.08** | <0.15 |
| jj_profile container | 1.65 MiB x3.26 | **1.53 MiB x3.53** | >=2.6 |
| survival 16x12 | 1.36 MiB x3.12 | **1.04 MiB x4.05** | >=3.0 |
| D verify (cold reads, 1661 chunks) | 28.2 s | **25.8 s** | - |
| C compaction (pack wall) | 1.0 s | 8.4 s | background/watermark |
| import ST | 126 MB/s | 132 MB/s | >=25 |

`CSAVES_COLD_CODEC=zstd` opts back out; the per-record 0xFE-vs-dict_id stamp
makes both containers decode either way. enc_fp EXACT
(6d240d7166cdda99/12130/9971843). 60/60 tests (new zstd opt-out roundtrip
mirror; the MT determinism test pins zstd explicitly - intra-frame MT is a
zstd-path feature, the lzma2 path parallelizes via the outer per-stream
scope).

Research tick the same day: per-section entropy coding (order-0 rANS;
KT-adaptive context modeling, context = left+above voxel, exact lgamma
closed-form code lengths) was implemented to green (64/64 on the wip branch,
all three decode paths + glob conversion + shift admission) and measured
NEGATIVE end-to-end: JJ +8.8% (z6) / +6.7% (LZMA2), survival -10% (breaks
the 3.0x gate) despite -31.8% raw enc_fp member bytes. Root cause measured
three ways: slab streams win through CROSS-SECTION alignment the codec
harvests below any per-section entropy floor. Parked on
`wip-cm-section-form`; full probe tables + anti-retest gates in
docs/compression-research.md section 6.

## 0.9.47 — shim-evidence pristine gating (residual risk closes at the source)

Live-path pristine contracts now require the shim's per-write freshness
evidence (`nWriteChunk(III[BZ)Z`): chunk tag certifies
`Status=minecraft:full && InhabitedTime==0` at the first durable save
(`SavesShim.isFreshChunk`, both write paths; `write_chunk_fresh` on the
native side; `CSAVES_PRISTINE_FRESH=0` restores 0.9.44-0.9.46 semantics;
import/strip path unchanged). The 0.9.44 residual risk — a mutation between
worldgen and the first autosave hashed into the contract and reverted on
regeneration — is closed at the source with zero Paper patches: the
evidence rides in the tag the server already hands over.

Real-corpus A/B/C (`pristine_fresh_probe`, production write paths, first
write = payload + session-mutation root field, fresh=false):

- gate ON: pristine=0 stored=728, **edits_surviving=728/728**, compact
  1.37 MiB (x0.31 vs anvil), `unfresh_stored=728`;
- pre-0.9.47 semantics: pristine=728, **edits_reverted_on_regen=728/728**
  (the hazard, measured);
- pure-pregen invariance: fresh first writes contract 728/728, one relight
  session keeps pristine=728 stored=0, compact 0.03 MiB (**x0.0057 vs
  anvil**); CI pure-pregen gate unchanged (x0.01, 152 contracts, relight
  replay 318/318 kept).

Flag-off byte-identity: enc_fp EXACT 6d240d7166cdda99 / 12130 members /
9,971,843 bytes; JJ corpus verify ok=946 bad=0; bench_size compact x0.08;
68/68 tests (new: unfresh-first-write stores honestly + never contracts
later; fresh-first-write contracts; opt-out restores 0.9.46).

## 0.9.48 — pristine contract audit: the wrong assertion is caught at deploy time

The last named frontier (§10/§11 "contract/regen-drift audits") ships as
`pristine_audit` — read-only, production hash chain, exit-code gate. For
every pristine contract it re-derives the raw + strip-canonical hashes
from the ORIGINAL corpus bytes (the exact incoming-payload-bytes form the
contract check uses — verified against the write path) and predicts
per-chunk restore behavior: raw_match (bulletproof) / canon_match
(volatility absorbed) / DRIFT (will flip — assertion wrong for that
chunk); `--session` replays the §10 relight model against the stored
legends. `--import` mirrors the operator flow end-to-end: stage a copy
(the production import drains its source), import, audit the untouched
original.

Real-corpus measurement (`pristine_audit /tmp/store --corpus
bench-corpus/dim --import --session`):

- import 728 chunks, contracts=728 (canon legends 728, raw-only 0);
- legend check: **raw_match=728 drift=0** — every legend re-derives
  byte-exact from the corpus;
- session check: **kept=728/728** (relight raw-hash differs 728/728 —
  all raw misses canon-held, §10 verdict transfers to the live legends);
- negative control (one tampered payload, +17 B root field): **drift=1,
  session_flipped=1, verdict FAIL, exit 1** — the tampered chunk is
  named; deploy-time red light works.

CI gains the audit gate (size-bench job): import → audit → ship, counts
pinned, `bench_pristine_audit.txt` artifact. `Store::pristine_legend(key)`
is the public read-only tooling face. Gates: 69/69 tests (new:
`pristine_audit_legend_accessor` — legend equals the write-path hashes,
None after flip / whole-mode / raw-only containers), enc_fp EXACT
6d240d7166cdda99 / 12130 members / 9,971,843 bytes, bench_size compact
x0.08 (gate 0.15). The §12 frontier list is empty: bytes closed, erosion
closed, first-write hazard closed, assertion auditable.

## 0.9.49 — pristine-aware export: the round-trip closes, every None gets a name

The §12 "operator-facing surface" had one silent None left: the Anvil
exporter dropped every pristine regeneration contract (read = None BY
DESIGN → `continue`) — a pristine pregen world exported as region files
missing every contracted chunk with zero accounting; mappers/converters
would call it corruption. The exporter now returns
`ExportReport { regions, chunks, bytes, pristine_skipped, lost }` with
the accounting identity `chunks + pristine_skipped + lost ==
store.keys().len()`; `is_pristine` separates absent-by-contract from
genuinely-unreadable, `lost` keys print per-key stderr, and a partial
export announces itself (`728 chunk(s) absent by pristine contract —
vanilla regenerates them from the seed on load`).

Round-trip measured end-to-end on the real 728-chunk corpus (new
`export_roundtrip` probe):

- **Phase A (flag-off): byte-identical 728/728** — container → Anvil →
  container keeps every payload (production `hash_of` identity); the
  exporter is lossless for honest storage;
- **Phase B (mixed state — the production picture: pregen + player
  edits): 720 import contracts + 8 live-path flips → export exactly 8
  chunks + pristine_skipped=720, lost=0; re-importing the export with
  pristine_import ON re-contracts 8/8 with raw legends byte-equal to
  the stored-A payload hashes; the 720 contracted keys are absent from
  the export and from B** — the honestly-stored part becomes the new
  contract set, the regenerated part stays regeneration semantics.

CI gains the roundtrip gate (size-bench job; Phase A + Phase B counts
pinned, `bench_export_roundtrip.txt` artifact). Unit test
`export_accounts_for_every_key_and_roundtrips` pins the accounting
identity + legend equality on a synthetic store. Gates: 70/70 tests,
enc_fp EXACT 6d240d7166cdda99 / 12130 members / 9,971,843 bytes,
bench_size compact x0.08 (gate 0.15). Research §13: the last silent
None on an operator path is counted, named, and gated — export
accountable.

## 0.9.53 — the derived index goes extinct: POI-store elision (0 B persisted)

The POI store is the last non-chunk mass in a world dir, and it is a
DERIVED index: vanilla re-derives POIs from chunk block states on load
(`PoiManager.checkConsistencyWithBlocks` / Yarn `initForPalette`;
Moonrise redirects the same call). `CSAVES_POI_ELIDE=1` (opt-in, poi
stores only) turns the poi container into a bounded volatile session
scratchpad — LRU cap 64 MiB (`CSAVES_POI_ELIDE_CAP`), read-your-writes
within the session, 0 B persisted, reopen reads None and vanilla
rebuilds from block states (the exact semantics of a missing poi
file). Measured on the 262,144-chunk scale world (41 poi chunks/region
= 10,496 poi chunks): vanilla `poi/` 43.0 MiB (4 KiB sector padding
dominates) -> whole-mode container 1805.3 KiB -> elision **0 B**.
Whole-world arithmetic at the flagship tiers: Anvil region+poi
1069.0 MiB vs c-saves 65.0 KiB = **x0.000059 (99.994%)** — the elision
alone makes the whole-world ratio ~29x harsher. Legacy import names +
counts + removes the derived index (CSAVES_KEEP_MCA=1 keeps);
entities are NOT regenerable (mob spawn randomness) — parked
permanently. Gates: 75/75 tests, scale probe poi ladder greps
(vanilla corpus, 0.0 KiB persisted, RAM-only reopen, elision PASS,
whole-world < x0.0001).

## 0.9.52 — harsher arithmetic: the region-assert tier (0.13 B/chunk, x0.00003)

The answer to "is there harsher arithmetic": the per-chunk legend is
information-theoretically floored at ~32 B/chunk (random 128-bit hashes
do not compress — measured: raw-only tier x0.0044 = 17.9 B/chunk, canon
tier x0.0087 = 35.8 B/chunk on the same 262k world). Below the floor:
stop storing per-chunk information. `CSAVES_PRISTINE_REGION=1` (opt-in
on top of pristine + pristine_import): one 136 B region assert per
imported region file — a 1024-bit presence bitmap — instead of 1024
legends.

- the 262,144-chunk world weighs **34.0 KiB (x0.00003 of Anvil,
  99.997% saved)**: 256 records x 136 B, the bitmap arithmetic made
  flesh; after one identical re-save + one real edit (both stored
  honestly — no legend to verify, the deliberate trade): x0.00006;
- key-level semantics match the per-chunk contract everywhere:
  is_pristine/has_chunk/keys/chunk_count include asserted slots,
  read_chunk = None BY DESIGN, read_fail_reason names the assertion;
  the write path never re-contracts an asserted key and counts every
  supersede in `pristine.flips` (§12 telemetry);
- asserts persist in `pristine-regions.bin` (tmp+rename, outside the
  segment system — region keys share the u64 space with chunk keys);
  default-off: every existing path byte-identical without the flag
  (the 72-test suite pins both tiers, the new
  `pristine_region_assert_tier` test covers the full semantics);
- scale probe ladder on the SAME world: Anvil 1026.0 MiB -> raw-only
  4.5 MiB (x0.0044, 17.9 B/chunk legends, no stored edits) -> canon
  legends 9.0 MiB (x0.0087, 35.8 B/chunk, with its 256 stored edits)
  -> region-assert tier 34.0 KiB (x0.00003); CI greps pin the record
  count (256), the verification-cost semantics (262,142 of 262,144
  slots still asserted), the export accounting and the region
  re-import;
- parked with facts: POI elision (POI files are a derived index —
  vanilla re-scans block states on load, Moonrise redirects the same
  call) is the next candidate; entities are NOT regenerable — parked
  permanently.

Gates: 72/72 tests, enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843,
bench_size compact x0.08, export_roundtrip PASS, scale_probe 16 PASS.

## 0.9.51 — the -90% promise measured at world scale: 262,144 chunks, x0.0087

The claim the whole pristine line was built for is now measured at the
scale it names. `scale_probe` (new example) generates a structurally
realistic pregen world — the same Tectonic-like wgen generator the size
benchmarks use — at 512x512 chunks = 256 region files = 8192x8192
blocks, streams it to disk as vanilla zlib-6 .mca (one payload in RAM
at a time, regions written in parallel), imports it under pristine
contracts and adds 256 honest player edits (one per region, round-1
rebuilt chunks):

- vanilla Anvil baseline: 1,075,838,976 B (1026.0 MiB); container
  (store directory, every segment/header/frame/dictionary included):
  9,385,442 B (9.0 MiB) = **x0.0087 — 99.1% saved**, an order of
  magnitude beyond the -90% promise; 35.8 B/chunk = the 33 B canonical
  legend plus key and segment framing;
- scaling law: x0.0110 at 4,096 chunks, x0.0089 at 65,536, x0.0087 at
  262,144 — the ratio FALLS as the world grows (per-contract constant
  amortizes, edit mass stays a fixed small fraction);
- round-trip at scale: export materializes exactly the edits (256
  stored + 261,888 pristine_skipped + 0 lost = 262,144 keys), re-import
  re-contracts all 256 with legends describing the stored (normalized)
  form — raw == hash_of(read-back), canon == canonical(read-back) — and
  an identical re-save keeps the contract;
- timing: 401.5 s world generation (parallel region writes), 28.8 s
  full 262k-chunk import; total probe ~7 min;
- CI: new size-bench step `scale probe gate — the -90% promise at
  262,144-chunk world scale (v0.9.51)` — the probe panics at
  >= x0.10, the step greps the exact world size, the accounting
  identity, the re-import verdict and the PASS line.

Gates: 71/71 tests, enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843,
bench_size compact x0.08, export_roundtrip PASS, scale_probe 16 PASS.

## 0.9.50 — flip telemetry + contract-aware forensics: the §12 operator promise made true

§12 named "the flip events themselves" as the operator-visible surface —
but no counter existed, and forensics lied about contracts:
`read_fail_reason` on a contracted key reported "map-level checks pass,
0 blob refs — failure is in decompress/join" (nothing had failed — the
chunk is absent BY DESIGN) and a pending contract was called
"unparseable".

- `Store::pristine_flips` (per-store, race-free) counts live writes that
  superseded a contract (neither the raw nor the strip-canonical legend
  matched) — the exact "what the elision chose not to risk" event;
  surfaced as `pristine.flips` in `profile_json`;
- `read_fail_reason` resolves the current truth in read_chunk's order
  (pending batch wins over the committed map) and names pristine
  contracts at either stage — the sweep test caught the stale-contract
  case (a superseding pending recipe is the truth while the committed
  map still points at the old contract);
- `pristine_diagnostics_sweep` pins the full contract semantics across
  has_chunk / chunk_intact / keys / chunk_count / read_chunk (pending +
  flushed) / read_fail_reason (both stages) / flip-once / reopen.

No byte-level change. Gates: 71/71 tests, enc_fp EXACT
6d240d7166cdda99 / 12130 members / 9,971,843 bytes, bench_size compact
x0.08 (gate 0.15), export_roundtrip PASS. Research §14: bytes closed,
erosion closed, first-write hazard closed, assertion auditable, export
accountable, operator surface honest.

---

## v0.9.54 — the proven assert: seed verification and the divergent-chunk gate

The JJThunder corpus gates prove codec correctness on REAL worlds; the
scale probe proves the pristine economics on the synthetic 262,144-chunk
world. What neither proved until now: that "seed-reproducible" — the
claim the 0.13 B/chunk region-assert tier rests on — is true, per run,
not per operator's signature. v0.9.54 adds the verification machinery to
the scale probe:

- the seed-verification gate: every chunk on disk canonically
  hash-compared against the seed regeneration (262,144/262144 pinned);
  the surviving legends checked through the container against the same
  hashes (261,888/261888 pinned);
- the divergent-chunk gate: a round-1 (diverged) chunk is caught by the
  classifier, refused the assert for its slot, stored honestly through
  `write_chunk_fresh(fresh=false)` — the restore identity pins
  262,143 seed-restored + 1 container-stored = 262,144/262144 restored,
  0 lost.

The comparison is deliberately canonical (`canonical_contract_hash`),
not byte-level: NBT byte order drifts between independent
serializations (HashMap-backed compounds — fastnbt here, JVM map order
in real vanilla), and canonical equality of regenerated content is
exactly what the pristine tier guarantees on restore. No byte-level
change; the JJThunder corpus path is untouched. Gates: 75/75 tests,
enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843, bench_size compact
x0.08, export_roundtrip PASS, scale_probe 16 PASS with the three new
greps. Research §18.

## v0.9.55 — the challenge answered: every chunk touched, zero pristine help

The JJThunder gates prove codec correctness on REAL worlds; the scale
probe proves pristine economics where 261,888 of 262,144 chunks stay
contracted. What neither proved until now: the worst case — a world
where EVERY chunk was touched by a player. v0.9.55 adds the
`worst_case_probe` CI job (8th bench job) that deletes the elision
variable: the same 262,144-chunk wgen world with round-1 content in
EVERY slot, stored through the normal pipeline in two phases —

- phase A: every pristine flag OFF, zero contracts exist, pure codec
  (section dedup + zstd); measured ~235–240 B/chunk = x0.0575–0.0587
  of the 4,104 B/chunk Anvil baseline;
- phase B: pristine import of the seed twin, then 262,144 live edits
  supersede every contract, 0 pristine remain; the colonized container
  converges to ~296–301 B/chunk = x0.0723–0.0734.

Both containers are measured after the production maintenance pass
(sync → seal → compaction to completion) and every stored chunk is
verified SEMANTICALLY against the edited content — the same
per-section expanded-state-vector equality the JJ gates use — bad=0
pinned in both phases. The fully-colonized export accounts for every
key: 262,144 stored + 0 pristine_skipped + 0 lost. The in-process
gates assert container/anvil < x0.15 on 100% player-modified content
(a panic exits non-zero); the greps pin the counts, the CI artifact
(`bench_worst_case.txt`) carries the exact full-scale values.

No byte-level change to the JJThunder corpus path itself. Gates:
75/75 tests, enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843,
bench_size compact x0.08, export_roundtrip PASS, scale_probe 16 PASS,
worst_case_probe PASS with the nine new greps. Research §19.

## v0.9.58 — the streaming export: the runner wasn't flaky, the export was

Both v0.9.57 CI attempts died the same way: every gate green (phase A
x0.0588, lifecycle 262,144 edits → 0 pristine remain, verify bad=0 in
both phases, ratio B x0.0739 = 303 B/chunk), then a runner shutdown
(exit 143) ~2 minutes into `export_dir`. The export used to materialize
EVERY chunk payload (~5 GB at 262k chunks) before writing a region byte
— 262,144 chunks was the first corpus ever to reach the export path, and
it was OOM-killed there, twice. The fix is a streaming export: keys-only
grouping (~8 B each), then one region at a time — 1024 chunks (~20 MB)
is the only chunk content resident; same key order, same region layout,
same report semantics (`chunks + pristine_skipped + lost` still accounts
for every key). Local replay under a 3 GB sandbox (RAM that kills the
old export): 256 regions, 262,144 chunks, 1,073,741,824 B, 0
pristine_skipped, 0 lost, 146 s. worst_diag gained the export-stress
phase 4. No byte-level change to the JJThunder corpus path itself.
Gates: 75/75 tests, enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843,
bench_size compact x0.08, export_roundtrip PASS, worst_case_probe PASS
with the nine greps. Research §22.

## v0.9.57 — the gate caught the corpus: the three zero-edit chunks, made constructive

The v0.9.56 CI run refused the worst-case phase-B claim — and that
refusal was the system working: after 262,144 live edits, 3 chunks were
still pristine contracts (262,141 stored). The `worst_diag` forensic
replay isolated it in one run: NOT a race — the documented identical-
re-save canon skip (v0.9.46) fired exactly 3 times, because wgen's
round-reseeded heights are quantized and in flat zones land on the same
integer height in all 256 columns. A full-world scan: 188,829 chunks
differ in every column, ~100 flat-zone chunks differ in a handful, and
EXACTLY 3 — (158,372), (360,164), (375,463) — differ in ZERO columns;
their "edit" was canonically identical to the seed. The gate was right,
the corpus was wrong. The fix makes the edit constructive: every
round>0 chunk breaks its corner block (lx=15, lz=15) — the change
flows through sections, heightmaps and blending_data, so round-1 is
canonically visible in EVERY chunk by construction. Post-fix replay at
full 262,144-chunk scale: skip branch 0 hits, 262,144 stored, 0
contracted, no survivors. No byte-level change to the JJThunder corpus
path itself (wgen is a benchmark-only generator). Gates: 75/75 tests,
enc_fp EXACT 6d240d7166cdda99 / 12130 / 9,971,843, bench_size compact
x0.08, export_roundtrip PASS, worst_case_probe PASS with the nine
greps. Research §21.

## v0.9.56 — the pack sweep: the JJThunder import pattern generalized to bulk scale

The first CI run of worst_case_probe measured x0.2104 (863 B/chunk) at
262,144 chunks and the x0.15 gate refused it — correctly. The container
was the UN-SWEPT hot tier: one `request_pack` re-tiers one hot segment
per `compact_step`, every existing bench corpora fit in 1–2 segments,
and 262k chunks span ~4 × 64 MB ones. The fix re-raises
`request_pack` before every `compact_step` until no hot material
remains — the exact pattern this corpus path has always taken after an
Anvil import, now proven at bulk scale. Forced 12-segment local run
(`CSAVES_SEG_ROLL=1 MB`): every segment packed, converged weight
IDENTICAL to single-segment scale — 240 B/chunk (x0.0587) pure codec,
302 B/chunk (x0.0738) fully colonized, verify bad=0 both phases. Even
the un-swept hot tier was −79% on fully played content; the swept
container is −92%…−94% at every scale. Gates: 75/75 tests, enc_fp EXACT
6d240d7166cdda99 / 12130 / 9,971,843, bench_size compact x0.08, scale_probe
PASS, worst_case_probe PASS with the sweep. Research §20.

## v0.9.59 — the container is the world: the serve gate (job 3 of 9)

The same corpus path this document follows (Anvil in → container →
verify) gained its endgame form: Anvil is no longer a stored artifact.
`serve_region` renders any region in memory byte-for-byte identical to
the export view (one extracted region builder feeds both), `serve_chunk`
is the engine-hook entry for a future mod shim, and the takeover import
drains the .mca it reads. The new CI `serve-bench` job takes a
65,536-chunk vanilla world, plays every chunk live, then counts the .mca
files on disk: ZERO — the world lives only in the container; the served
bytes re-import and verify bad=0; enc_fp needle untouched
(6d240d7166cdda99 / members=12130 / bytes=9971843). 77/77 tests.
Research §23.

## v0.9.60 — the server plays from the container: the live-session gate (job 2 of 10)

The shim surface this corpus path serves (nReadChunk / nWriteChunk(fresh)
/ nEndSave over the natives' registry) got its own end-to-end gate:
`play_probe` runs a full server session — worldgen burst (fresh evidence
→ contracts, 0 stored), play (edits flip to honest storage, unchanged
re-saves stay contracted via the canon legend), RESTART (fresh registry,
same dir), reload (edits byte-exact, contract legends regenerate the
worldgen form) — and counts the .mca files on disk: ZERO. 16,384 chunks,
358 B/chunk (stored ~250 B, contracts ~35 B), 77/77 tests, enc_fp
untouched. Research §24.
