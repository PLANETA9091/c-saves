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
