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

## I/O speed (real JJThunder corpus, 2-core CI sandbox, single-threaded unless noted)

| operation | Anvil (zlib-6) | c-saves | vs Anvil |
|---|---|---|---|
| region read, warm | 821 MB/s, p90 150 us | **1401 MB/s, p90 47 us** | **1.7x faster, tails 3x better** |
| region read, p99.9 (stall) | 298 us | **306 us** | parity |
| region read, 2 IO threads | — | **29k chunks/s** | scales with Moonrise workers |
| region import (fresh write) | 53.8 MB/s, p99.9 5.3 ms | 33.2 MB/s, p99.9 11.3 ms | one-time cost, inside tick budget |
| region steady rewrite (autosave) | 53.8 MB/s, p90 2.3 ms | 36.3 MB/s, **p90 1.8 ms** | p90 tails BEAT Anvil |
| entity write (fresh) | 109 MB/s | **708 MB/s** | **6.5x faster** |
| entity steady rewrite | 109 MB/s | **1647 MB/s** | **15x faster** |

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
