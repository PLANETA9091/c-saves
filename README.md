# c-saves — Rust-native Minecraft world storage

CRUSSTY module: content-addressed section dedup + zstd container, taking
over Paper 26.x chunk IO. Worlds stay 100% byte-semantically compatible
(every stored chunk is verified against the source on import), the container
is radically smaller, and reads/writes stay off the server thread's critical
path (background pack passes, bounded slab cache).

## v0.8.0 direction (attribution-driven): the entropy floor map

`survival_exp` (CI job `survival-exp`) compressed every component class of a
realistic survival corpus separately at zstd-19 and predicted every codec
lever BEFORE touching production code:

| class | raw | z19 weight | verdict |
|---|---|---|---|
| dense block_states data | 40.8 MB | 861 KB (53.8%) | **0.084 bits/voxel — zstd already exploits cross-section LZ matches; custom light/plane/shift codecs all LOSE** |
| block entities (chests) | 4.2 MB | 352 KB (22.0%) | z19 concat floor; item-level dedup measured WORSE (429 vs 302 KB) |
| skylight | 7.6 MB | 203 KB (12.7%) | plane-const codec 823 KB, column-staircase 582 KB — both LOSE to plain z19 (203 KB) |
| heightmaps | 1.5 MB | 140 KB | model overestimates; production stores delta-varint |
| palettes | 2.3 MB | 45 KB | minor |
| shift-ref admission | 4/19941 sections | — | survival terrain sections are content-unique (ore noise) |
| plane-level dedup | 63.9% planes duplicate | — | unique planes are individually incompressible; net LOSS vs slab z19 |

Conclusion: on noise-heavy synthetic terrain every remaining category sits at
its practical entropy floor, and the ONLY honest way forward is attribution
on a REAL survival world (`bigworld-survival` workflow: 512x512 vanilla
chunks + 2000 NBT loot chests + 20k entities, Paper 26.3). `real_exp` breaks
that corpus down by class, chunk-dedup rate, entity kind and BE family.

## v0.9.1: correctness round — scan refcount fix + self-healing autosave skip

The unchanged-write fast path (Paper autosaves re-serialize unchanged chunks
every cycle; hash match returns O(1) where Anvil pays inflate+deflate)
exposed TWO latent production bugs:

1. **scan() rebuilt refcounts as 0 for every BATCHED recipe** — the reopen
   pass resolved recipes with `load_recipe`, which cannot follow the kind-9
   batch index (BATCHED is a sentinel, not a length). Reopen → refs=0 → the
   first full-rewrite cycle superseded recipes down to refs=0 and evicted
   live blobs that other, still-unrewritten recipes referenced (dangling
   ids, unreadable chunks). Masked for months because subsequent full
   rewrites re-created the blobs; any chunk NOT rewritten afterwards would
   break. Now resolved through the batch index.
2. **read-your-writes**: a rewrite of a BATCHED chunk removes its batch_idx
   entry at supersede time, so the stale committed pointer was unreadable
   until the next flush — reads now take the pending recipe batch FIRST.

The fast path is SELF-HEALING: on a hash match it probes chunk_intact()
(recipe resolves + every referenced blob is live — a few map lookups, no
decompress) and falls through to the full write on any damage, repairing
from the payload the server just handed us.

Speed (corpus bench, vs the Anvil zlib-6 baseline): steady-state autosave
p50 **9.3 us vs Anvil ~150 us**, aggregate 124 MB/s vs 62.6; a 10%-dirty
autosave cycle (real mutations) runs at 284 MB/s aggregate, p50 8 us.

## v0.9.0: light-uniform codec + bigworld pack-drain fix

The 512x512-chunk real JJThunder world (261,542 chunks) measured **31.8% of
all raw NBT bytes in light arrays** (SkyLight 1.64 GB + BlockLight 2.01 GB).
CSEC v4 (0xC8, third flag byte) collapses all-0 BlockLight (the underground
majority — previously 2048 RAW bytes per section) and all-0/all-1 SkyLight to
flag bits; streams without v4 features stay byte-identical v3 so dedup hashes
are stable. Also: the bigworld pack drain now re-requests a pack before every
compact step (the old loop stopped after one segment and left the 2 GB hot
tier unpacked at z3), `big_profile` scans EVERY written key for integrity
(self-heal re-import isolates pack-loss vs write-loss), and the palette
attribution bug is fixed (fastnbt `to_bytes` needs a compound root; bare
lists measured 0 and dumped ~1.86 GB of palettes into `sec_other`).

## v0.6.0 (WIP, profiling-driven): section RLE forms

Profiling the real JJThunder corpus showed the remaining region bytes live in
NON-uniform terrain sections (strata boundaries, cliffs, caves). v0.6.0 adds
two content-deterministic RLE forms next to the dense stream:

* vanilla order RLE — the voxel stream has x fastest / y slowest, so
  horizontal strata planes are contiguous: a 2048-byte 2-bit dense section
  with two strata bands becomes ~6 raw bytes;
* y-fast transposed RLE — one column per run, collapsing vertical walls and
  per-column boundaries.

The smallest of {dense, RLE, RLE-transposed} wins; the choice is a pure
function of content, so dedup hashes stay stable. Noise sections (gravel
patches, cheese caves) keep the dense form. Flag slot: CSEC3 f2 bits 4-5
(block_states) / 6-7 (biomes); legacy v3 streams decode unchanged. Section
slabs grew 2 MiB -> 4 MiB (longer zstd windows, same 32 MiB cache budget).

## Headline numbers (real JJThunder To The Max worlds)

Large world slice — 5,108 real mountain chunks (599,984 sections,
131-132 sections/chunk) + 20,000-entity corpus, Paper 26.2:

| component | Anvil | c-saves | ratio |
|---|---|---|---|
| region | 33.62 MiB | 12.92 MiB | x2.60 |
| entities (20k) | 7.39 MiB | 0.65 MiB | **x11.43** |
| **TOTAL** | **41.02 MiB** | **13.57 MiB** | **x3.02** |

32x32-chunk corpus (committed under `bench-corpus/`, CI-verified every
push): TOTAL x2.92, entities x3.13, `verify ok=946 bad=0` (v0.7.0: 8 MiB
slab frames + shift-ref section codec). Survival-shaped corpus (chests,
farms, skylight): TOTAL **x4.00**, entities x1.94 (v0.7.1 compact fix).

Synthetic Tectonic-style wgen bench (4096 chunks): compact container =
**x0.08 of Anvil** (x173 vs logical).

## v0.9.3 correctness round + LIVE Paper 26.3 (tps-bench)

Three latent bugs root-caused and CI-regressed in one round:

1. **Writer-segment roll hole (silent data loss).** `append()` rolled the
   writer with `seek(written)` while the append buffer still held unflushed
   records belonging at `written - buf.len()`: a hole of zeros over the
   segment tail records. Batch records decoded rawlen=0, blobs decompressed
   to garbage, the full-rewrite pack skipped them and the segment drop made
   the loss permanent — 23,380 of 261,572 chunks unreadable on the 262k-chunk
   bigworld (11,819 the run before; the corpus benches never roll a segment
   with a non-empty buffer, so they stayed clean). Roll now seeks correctly;
   `Cfg.seg_roll` (CSAVES_SEG_ROLL) lets tests force rolls; the pack drop
   path is poison-guarded (any unreadable victim keeps the segment, loudly).
2. **Probe-cache self-deadlock (server freeze).** `chunk_intact`'s cache
   lookup was an `if let` scrutinee; in edition 2021 the scrutinee guard
   lives through the else branch, so the miss path re-locked the same mutex
   against itself — every unchanged-chunk autosave (Paper's steady state)
   would freeze the server. Shipped in v0.9.1's probe_cache; every
   speed-bench since died at the job timeout unnoticed. Fixed by binding
   the lookup to a standalone statement.
3. **Concurrent supersede during pack (rollback/poison).** The pack's
   recs loop used a snapshot; a supersede in flight left dead pointers
   whose batch_idx entries were already removed — moving them tripped the
   poison or re-queued the OLD recipe over the newer pending one (silent
   chunk-state rollback). The loop re-checks the live chunks pointer.

**262,573-chunk real survival world (512x512, vanilla terrain, 10k
entities, bigworld-survival CI): integrity 261,573/261,573 (missing 0),
heal_rounds 0, verify ok=26,342 bad=0:**

| store | Anvil | c-saves | ratio |
|---|---|---|---|
| region | 2135.99 MiB | 970.43 MiB | x2.20 |
| entities | 7.27 MiB | 0.86 MiB | x8.48 |
| **TOTAL** | **2143.25 MiB** | **971.29 MiB** | **x2.21** |

(vs the light-stripped raw the container is ~x7.7; vanilla terrain is
noisy — dense sections at the z19 entropy floor — datapack worlds with
extreme terrain compress x3.0+.)

**Live Paper 26.3, c-saves injected via the CRUSSTY runtime (tps-bench
CI, 2-core runner, 1024-chunk world + 10k persisted entities, 8 samples
per phase, 256-chunk forceload-window rotation, timed save-all):**

| metric | A (Anvil) | B (c-saves) | B vs A |
|---|---|---|---|
| TPS (avg) | 19.96 | 19.95 | 1.00x |
| MSPT avg 5s | 5.78 ms | 5.96 ms | 1.03x |
| MSPT max 5s | 12.11 ms | 10.41 ms | **0.86x (better)** |
| forceload rotate (unload+save+load 256 chunks) | 491.6 s | 493.2 s | 1.00x |
| module attached / import | n/a | 897+193 chunks, 0 failed | YES |

(save-all flush never converges on the 2-core runner with 10k ticking
entities — both engines censored at the RCON timeout; engine save speed
is hard-gated by the speed-bench jobs instead.)

## I/O speed (real JJThunder corpus, 2-core CI sandbox, single-threaded unless noted)

| operation | Anvil (zlib-6) | c-saves | vs Anvil |
|---|---|---|---|
| region read, warm | 723 MB/s, p50 12.7 us | **2237 MB/s, p50 10.8 us** | **x3.1 faster** |
| region read, 2 IO threads | — | **34.8k chunks/s** | scales with Moonrise workers |
| region import (fresh write) | 62.6 MB/s | **51.0 MB/s, p99.9 3.9 ms** | near-parity, tails inside tick budget |
| region import, 2 save threads | — | **96.7 MB/s** | x1.9 scaling (Moonrise-style parallel saves) |
| region steady rewrite (autosave) | 62.6 MB/s, p90 2.2 ms | **~40 MB/s, p90 2.0 ms** | p90 tails BEAT Anvil |
| entity write (fresh) | 202 MB/s | **708 MB/s** | **x3.5 faster** |
| entity steady rewrite | 202 MB/s | **1893 MB/s** | **x9.4 faster** |

The v0.8.0 speed work (profiling-driven): the recipe-batch flush ran
zstd-19 synchronously inside the write thread — measured via the SPEED
stages-import breakdown, sync flush pauses cost ~40% of fresh-import wall
time and spiked p99.9 to 10.4 ms; the flush now runs at z6 (Cfg.recipe_level,
CSAVES_RECIPE_LEVEL) with a 192 KiB batch: p99.9 import dropped 10.4 -> 3.9 ms
(-64% max), container size unchanged. The WST_RECIPE stage timer was also
fixed (it measured cumulatively from write_chunk entry, misattributing 49%
of import to the recipe path; the real cost lives in split_parse_encode =
fastnbt parse + per-section CSEC encode — the next speed lever).

The v0.5.0 speed work: fused CSEC-to-NBT emitter (the read path used to
rebuild a `HashMap<String, Value>` per palette entry and re-serialize the
whole tree — it now walks the CSEC streams and emits wire-format NBT
directly, with per-content-hash splice caching; the stored tail is spliced
verbatim with only the heightmap arrays transformed back — zero parse work),
one-pass dense/no-straddle
converters (memcpy fast path for dividing widths), batched single-pass
section loads, lock-free segment mmaps, O(1) recipe-batch indexing, and a
hot tier at zstd-3 (measured 2.1x faster imports at IDENTICAL packed size,
because the background pack pass recompresses at 19 anyway).

`speed_bench` (CI job `speed-bench`) records full latency percentiles
(p50/p90/p99/p99.9/max) and per-stage nanosecond breakdowns for every run —
all stall points are captured as CI artifacts.

## Why the container is small

* **CSEC** — canonical section codec: frequency-canonical palettes
  (dominant block -> index 0, permutation-invariant dedup hashes), compound
  {Name, Properties} palette entries, narrow dense packing at ceillog2(n)
  when it beats the vanilla 4-bit clamp, starlight fields, dense heightmaps.
* **Content-addressed dedup** — identical sections (the 95%+ uniform stone /
  air / deepslate of extreme-height worlds) are stored once, chunk-locality
  ordered inside slabs so zstd windows see neighboring sections of one chunk
  adjacently.
* **Verbatim whole-mode blobs** — entity/POI payloads are never
  re-serialized (alphabetical NBT re-serialization destroys the vanilla
  field-order prefix alignment that gives cross-entity matches).
* **Batched slabs** — 2 MiB section slabs, 4 MiB entity slabs: each zstd-19
  frame sees the whole store's window; 20k entities compress to the
  concatenated floor (x11 vs per-blob x9).
* **Varint slab entry tables** (v2), compressed recipe batches (kind-9),
  no dictionary for whole-mode stores (measured net loss), frequency-only
  cold-tier recompression.

## Verification

Every import and pack pass is followed by semantic chunk verification
(expanded block-state vectors, palette-permutation safe). The jj-bench CI
gate fails on any drift (`bad=0` required).

## CI

* `bench` — unit tests (24), synthetic size gate (compact <= 15% of Anvil),
  JJThunder corpus gate (bad=0, ratio >= 2.6x).
* `bigworld` (manual dispatch) — generates a full 512x512-chunk JJThunder
  world on 16 parallel runners (128x128-chunk tiles), spawns 20k entities in
  the merged world and publishes the Anvil-vs-c-saves comparison. See
  `docs/bigworld-bench.md`.

## Layout

* `native/` — the Rust engine (`cdylib` + `rlib`), injected into Paper via
  the cplug shim (JNI).
* `bench-corpus/dim` — real JJThunder world subset used by the CI gate.
* `docs/` — benchmark methodology and results.
* `scripts/jj/` — server drivers: RCON client, entity spawners, tile
  generator (`gen_tile.py`).
