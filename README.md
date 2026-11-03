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

## v0.9.11 — zero-copy scan split: no full-tree parse, no tail re-serialization

The import split path parsed the WHOLE chunk payload into a fastnbt Value
tree (an allocation per NBT node) and then re-serialized the tail
(everything except sections) through a HashMap in arbitrary iteration
order. Both are pure overhead: import only needs (a) the sections list as
Values and (b) the tail bytes — which the source payload already contains
verbatim in vanilla key order (strictly more deterministic than HashMap
order, and it compresses better: cross-chunk tail prefixes stay aligned).

`scan_split` walks the root compound by header arithmetic (zero
allocation), removes the `sections` range, repacks each Heightmaps
LongArray through hmap_encode (the same transform tail_encode applies) and
patches isLightOn when stripping. Any structural doubt -> the full-parse
fallback runs unchanged. The raw tail is valid NBT; join parses it exactly
like the old tail.

The walk's own diagnostics (SCAN_FAIL reason + SKIP_LAST_POS beacon in
`split_stage_json`) caught a cursor-drift bug in the new `skip_value`:
TAG_Long_Array/Int_Array advanced by `l + 4 + l*8` instead of `4 + l*8` —
the element count was added as bytes, so every sections walk crossing a
block_states.data array overshot and fell back. Scan coverage on this
corpus: 522/728 -> **728/728** (the full-parse path never runs).

Encoder cuts in the same round: the palette remap fuses into the pack and
RLE loops (`dense_pack_mapped` / `dense_pack_wide_mapped` /
`rle_*_len_mapped` / `rle_encode_stream_mapped`) — byte-identical by
construction, since an injective remap preserves run structure and framing
— and the shift-pass identity serialization is allocation-free
(`pal_entry_identity_into` + borrowed sort keys).

Results (local, same corpus/run pair): import ST **36.9 -> 41.5-42.3 MB/s
(+13%)**, import p90 3496 -> 2873 µs; warm read parity (1200-1261 MB/s,
p50 17.1-17.3 µs); container unchanged (2.96 MiB = x0.08 vs Anvil in
bench_size). `enc_fp` byte-exact (fp 6d240d7166cdda99, 12 130 members),
corpus verify 946/946 bad=0, tests 53/53.

Follow-up (0.9.11.1): the SKIP_LAST_POS deepest-walk beacon was replaced
by SCAN_FAIL_POS — the exact byte position stored at the failing site
(zero cost on success, and one less relaxed atomic store per skip_value
call on the hot path); `scan_fail` in `split_stage_json` now carries
`{why, pos}`, and the per-key opening quotes it lost in the format-string
refactor are back (the JSON was invalid). A new `pub scan_probe` hook plus
`examples/probe_keys.rs` walk every corpus region chunk and cross-check
scan_root against fastnbt as the authority: 728/728 pass, 0 failures. A
re-run of speed_bench on this machine after the rebase measured import ST
44.3 MB/s (p50 240 µs), warm read 1349.8 MB/s — the residual scan_root
fail lines in bench logs come from the G2 dirty phase deliberately
flipping a payload byte; scan_root rejecting corrupted NBT into the
full-parse fallback is the designed safety behavior.

## v0.9.16 — fused unpack+counts+bounds + no-straddle pack fast paths: import +20%

The encoder core made THREE full passes over the same 4096 voxels per
section (unpack, sanity `any()`, counts loop) and packed through generic
variable-shift loops. `unpack_counts_vox` fuses all three into ONE pass
with width-specialized inner loops (bits 8 = `to_le_bytes` byte stream,
bits 4 = constant nibble shifts; vanilla no-straddle packing never crosses
a long so per-long extraction is independent), `dense_pack_mapped` takes
bits 4/8 (and wide bits 16) no-straddle byte paths. Per dense section:
enc 16.3 -> 10.3 µs, unp 3.8 -> 2.9 (counts fused in), pal 2.5 -> 0.07
(counts moved out), scan 465 -> 351 µs/chunk, shift 61.3 -> 48.8 (warm
facts.v cache side effect). Results (3-repeat confirmed): import ST
59.3-59.4 -> **70.6-72.6 MB/s** (+91% over 0.9.10), import MT 96.9-105.2
MB/s @ 2 threads, warm read 1321.9, p90 2300 -> 1786-1798 µs. All outputs
byte-identical — `enc_fp` fp 6d240d7166cdda99 members 12130 bytes 9971843
unchanged, probe_lite 42218 sections 0 mismatches, 53/53 tests, verify
728/728 + 218/218 + 1280/1280, bench_size x0.08 vs anvil compact,
container 1.66 MiB (x7.51), match pins unchanged (plane0=327, full=327,
dense_match=198).

## v0.9.15 — interleaved plane fingerprints: the shift build's fp pass ×3.25

The stage timers left the shift build at 14.6 ms of which the plane
fingerprint pass was the biggest slice (fp_ms 11.7 of 22.6-23.4): the
per-plane loop hashed the 256-entry FNV chain SEQUENTIALLY — 16 serial
dependency chains of ~3.5-cycle mul links, latency-bound at ~1 ns/step
over 4096 entries. `plane_fps` (shared by all three ShiftSec builders)
interleaves the four chains of each 4-plane group so they overlap:
fp_ms 11.7 -> **3.6**, shift build 22.6-23.4 -> **14.6 ms**, import ST
56.9-58.5 -> **59.3-59.4 MB/s** (+58% over 0.9.10), import MT 83.0-85.3,
warm read parity (1238-1266). The per-plane hash values are IDENTICAL —
pure scheduling — proven by the unchanged diagnostic pins: cand=447213,
plane0=327, full=327, ksum=3041, dense_match=198. Gates: `enc_fp`
fp 6d240d7166cdda99 members 12130 bytes 9971843 (byte-exact), probe_lite
42218 sections 0 mismatches, 53/53 tests, verify 728/728 + 218/218 +
1280/1280, bench_size x0.08 vs anvil compact, container 1.66 MiB (x7.51).

## v0.9.14 — shift build interns identities straight from the lite palette

The 0.9.13 shift build still materialized a full `SecView` per built
section (per-entry `props` Vecs + light-byte copies, ~8 µs × 2977 builds)
only to feed `intern_ids` with palette entries — and the allocation churn
was the "unp_ms 6.2 -> 30 across bench phases" inflation in the stage
timers. The lite path now interns identity bytes DIRECTLY from the typed
`PalLit` palette (`identity_from_lit` / `intern_ids_lit`: BTreeMap props
iterate in exactly the canonical sorted order the view builders sort to,
so the bytes — hence the chunk-global ids — are identical) and
`shift_sec_from_facts_lite` builds the ShiftSec without any view at all.
The view is materialized lazily at first winner-assembly use (`vw_get!` +
a `view_ready` mask: real builds mark their slot, meta-cache hits stay
not-ready and keep being skipped by the assembly exactly as before —
byte-identical output). The rare lite full build and the whole Value path
keep the eager view (their build functions consume it).

Results (same corpus, 3-repeat confirmed): import ST 53.5-56.3 ->
**56.9-58.5 MB/s** (+60% over 0.9.10, p50 112.9-115.9 µs), import MT
81-82 -> **79.2-84.1 MB/s**, warm read 1252.6 -> **1260.2-1302.3 MB/s**,
shift build 23.4/22.8/22.6 ms with unp_ms stable at 5.9-6.2 across all
phases (no inflation). Match decisions byte-identical (dense_match=198,
full=327, ksum=3041, cand=447213); `enc_fp` fp 6d240d7166cdda99 members
12130 unchanged, probe_lite 42218 sections 0 mismatches, 53/53 tests,
verify 728/728 + 218/218 + 1280/1280, bench_size x0.08 vs anvil compact,
container 1.66 MiB (x7.51).

## v0.9.13 — typed borrowed sections parse (lite): the sections list without a Value tree

The scan-split path handed the sections list to fastnbt as a `Value` tree:
one HashMap per palette entry plus two Strings per property, ~125 µs/chunk
(SPLT_SCANP) of allocations that never survive encoding. A serde typed parse
over the SAME bytes (`SecLite`: `Cow<str>` palette names/properties via a
custom visitor, BTreeMap properties — canonical order for free, borrowed
light slices, `LongArray` data) parses the same list **2.4x faster**; all
728 corpus chunks take the lite path (SPLT_LITE), scanp 125.1 -> **62.5
µs/chunk**, scan total 590.7 -> 479.0 µs, import ST 47.1-48.1 -> **53.5-56.3
MB/s** (+50% over 0.9.10, p50 240 -> 112 µs), import MT 69.5 -> 81-82 MB/s,
dirty10 612.9 -> 736.6 MB/s; warm read parity (1252.6), container unchanged
(1.66 MiB, x7.51 / x0.08).

Both parse currencies feed the SAME `SecView` (palette entries as
(name, props) borrows + data slice): `section_encode_view`, the shift pass
(`SecListSrc::Values` / `SecListSrc::Lite`) and the winner-assembly run
unchanged over either source. Shape-strict by construction
(`deny_unknown_fields` over exactly the 7 KNOWN section keys, Y: i8, palette
string-or-{Name, Properties}, data LongArray): ANY deviation fails the parse
and the chunk takes the old Value path unchanged — the lite path is a pure
fast path, never a behavior change. Encoder-None sections (oversized
palettes, malformed data) fall back to a hand-rolled valid-NBT serializer
(`sec_lite_to_bytes`, homogeneous list element types, no per-element tag
bytes). `CSAVES_NO_LITE=1` force-disables the lite path (bisect switch).

Byte-identity evidence: `examples/probe_lite.rs` asserts per corpus section
semantic equality, `hash_lite == structural_hash(stripped Value)` (the
enc-cache keys align across paths — feed_lite mirrors feed's sorted-key
byte stream) and IDENTICAL encoded bytes across both currencies — 42218
sections, 0 mismatches everywhere. Gates re-run: `enc_fp` fp
6d240d7166cdda99 members 12130 (byte-exact vs the 0.9.8 pin), 53/53 tests,
verify 728/728 + 218/218, bench_size x0.08 vs anvil compact.

## v0.9.12 — encoder-to-shift facts reuse: build once, use twice

Fine-grained build timers attributed 74% of the shift build (60 of
81 ms per 728-chunk import) to the voxel unpack + ident/counts fill —
work the encoder had already done: `section_encode` unpacks the same 4096
voxels and sorts the palette by the SAME comparator (frequency desc, then
plain/name/props-sorted — both derive counts from the same stream). The
encoder now captures `SectionFacts { v, order }` at the pack/RLE decision
point where both values were dead anyway (the voxel stream is moved out —
zero copy), threads them through the section list into the shift pass,
and `shift_sec_from_facts` builds each ShiftSec from id interning + ident
fill + plane fingerprints only: no unpack, no counts pass, no key sort.
2973/2977 builds take the fast path (the rest fall back to the unchanged
full build for enc-cache-hit or non-CSEC sections).

Results (same corpus, 3-repeat confirmed): shift build 81.3 -> 24.3 ms
(unp 60.0 -> 5.7 ms, key 2.8 -> 0.3 ms), shift stage 141 -> 62.6 µs/chunk,
scan total 641 -> 561 µs/chunk, import ST 43.2-44.3 -> **47.1-48.1 MB/s**
(+27% over 0.9.10, now 86% of same-run Anvil write). Match decisions
byte-identical (dense_match=198, full=327, ksum=3041); `enc_fp` fp
6d240d7166cdda99 unchanged, 53/53 tests.

## v0.9.9 — skip discarded tail compression on blob-path tails

`write_chunk_mode` compressed every chunk tail at z6 BEFORE the size branch;
for tails >= tail_blob_min (512 B — most chunk tails: block entities,
entities, heightmaps) the compressed output was thrown away and ensure_blob
recompressed the same bytes at the hot tier. compress_tier has no side
effects (dict sampling happens elsewhere), so the call moves inside the
inline branch. Stored bytes identical (blob tails unchanged, inline tails
same path), enc_fp byte-exact (fp 6d240d7166cdda99), jj verify 946/946,
tests 53/53. Local import p50 262.9 -> 252.9 µs (-4%), ST 34.9 -> 36.0 MB/s.

## v0.9.8 — warm-read canon cache: GLOB decode once per member, not per read

The CI speed gate's warm-read floor (0.4x same-run Anvil + 100 MB/s absolute
backstop) had been passing **via the backstop only**: the region corpus
measured warm read at 260 MB/s vs Anvil 1036 (0.25x), p50 15.9 µs but p90
549 µs — a collapsed tail, not a slow mean. Per-stage instrumentation (new
`slab_hit`/`slab_miss`/`decomp`/`glob_conv`/`shift_res` counters in
`read_stage_json`) pinpointed it: the slab cache was perfect (3860 hits, 0
misses, 0 decompress ms) while **`glob_conv` burned 39.4 µs × 13,575 = 534
ms — 92% of sectload**. Every warm read re-decoded every GLOB member
section: RLE gid decode of 4096 voxels + the frequency-canonical CSEC
rebuild, from scratch, every time, forever.

Fix: a bounded `CanonCache` (LRU by bytes, `CSAVES_CANON_CACHE`, default
16 MiB, 0 disables) keyed by the section blob's **content hash**. The stored
member bytes are immutable (content-addressed) and the state registry is
append-only, so the conversion is a pure function — each member decodes once
per process. The old two-pass positional assembly (a `gi` counter pairing
converted buffers with member slices) is replaced by per-index
`canon[i] <-> slices_raw[i]`, which also removes a latent desync hazard: one
failed conversion used to shift every later member of that chunk onto the
wrong canonical buffer (unreachable on valid stores — conversion failure
already implies corruption — but the invariant is now structural).

Results (local 2-core, same corpus, same run): warm region read **190.8 ->
1303.2 MB/s (x6.8)**, wall 0.391 -> 0.055 s, **p90 758 -> 49 µs (x15)**,
p99 1195 -> 75 µs; 2-thread warm read 4742 -> **36.5k chunks/s**. Warm read
is now **x1.6 vs same-run Anvil aggregate** (was 0.23x); p50 unchanged
(17.4 µs — the no-GLOB fast path was never slow). Cold read, import,
steady-write and size are unchanged (import 35.2 ST / 50.4 MT local,
container 1.81 MiB = x6.88 vs Anvil sector-padded), `enc_fp` fingerprints
byte-exact (12,130 members, fp 6d240d7166cdda99), corpus verify 946/946,
tests 52/52. The speed gate now means what it says: warm-read floor raised
0.4x -> **0.8x same-run Anvil** (backstop unchanged) — the pre-fix state
would fail it.

## v0.9.7 — import split-path cuts: lazy shift builds, single-unpack encode

Profiling the split path (the stages-split JSON) showed the sections stage
at 635 µs/chunk: section_encode 346 + shift_ref_pass 198 + structural_hash
72. Three cuts, byte-exact on the whole corpus (new `enc_fp` example
fingerprints every member: 12,130 members / 9,971,843 bytes — identical to
main before and after):

1. **Lazy ShiftSec builds.** The shift pass built a per-section view
   (unpack + canonical keys + 16 plane fingerprints, ~40 µs each) for EVERY
   eligible section before matching; the fingerprint pre-filter then rejects
   99.93% of candidate pairs and enc-cache hits are skipped as match sources
   entirely — most builds were never consulted. Builds now happen on first
   USE as match source/target. One real bug caught and fixed en route: the
   lazy getter must refuse INELIGIBLE targets without building (the eager
   `None` did), or the candidate budget and the chosen refs change. Shift
   build 124 -> 88 ms, shift stage 196 -> 152 µs/chunk.
2. **Single-unpack canonical encode.** `enc_pal_data_canon` re-unpacked the
   same 4096 voxels twice (once for the frequency counts, once for the
   remap/dense/RLE stages). Counts now come from the shared stream; failure
   modes are preserved (length mismatch -> None, garbage voxel ->
   ST_ENC_FALLBACK + None, no-data -> zero-count lexicographic order).
   section_encode 251 -> 224 ms.
3. **Batched structural-hash feeds.** LongArray/ByteArray feed xxh3 as ONE
   update over the raw two's-complement LE bytes (streaming == one-shot
   over the same byte sequence; was 256 / 2048 update calls per array).

Import 31.5 -> 34.3 MB/s single-thread, MT 47.9 -> 52.4 MB/s (local 2-core;
the CI runner splits differently — faster cores, relatively slower encode).
Corpus verify 946/946, container x3.07 unchanged, bench_size x0.08 vs anvil.

## v0.9.6 — hot-slab write batching: one frame per chunk on the import path

The import path used to run one bulk-zstd call per SECTION (~130 calls of
0.25-4 KiB per chunk; the ensure stage measured 16.5% of import wall time on
the real corpus). v0.9.6 batches a chunk's sections into ONE hot-tier slab
frame at write time:

* wire = SLAB V2 with a new **0x40 hot-tier kind bit**; no GLOB variant on
  the write path (the cold re-pack re-decides the form with a 4-8 MiB
  window), so the drained container is byte-equivalent to the old pipeline —
  corpus stays **x3.07, verify 946/946**.
* `scan()` rebuilds 0x40 slabs with `cold=false`, so the pack pass can still
  re-group them into 4-8 MiB cold frames after a reopen; pack-produced slabs
  now stamp `cold=true` explicitly (previously the slab flag alone gated
  re-packing).
* import throughput on CI (same-runner comparison): **33.3 -> 45.7 MB/s
  (+37%)** single-thread, **65.7 -> 90.4 MB/s (+38%)** multi-thread; ensure
  stage **-44%** (105.9 -> 59.3 ms), import wall time -27%. Local 2-core
  sandbox: +6-8% / +4-7% (smaller absolute gain — a larger share of that
  machine's import time sits in the parse/encode stage).
* regression tests: batch roundtrip across reopen + drain + invariant scan,
  and the below-threshold per-blob fallback. 52/52 tests, all size gates
  unchanged (bench_size x0.08 vs anvil, gate <= 0.15).

## v0.9.5 — GLOB2 (0xCC): RLE'd gid streams + u16 registries + a latent wide-palette fix

Three increments on the GLOB architecture, plus one bug the work flushed out:

1. **0xCC GLOB2 forms.** The raw 4096-byte gid stream is no longer the only
   wire: `[gid][varint run]` RLE pairs in vanilla order (gform 1), in the
   y-fast transposed order (gform 2), and their u16 twins (gform 3/4/5) for
   registries past 256 states. Raw-u8 members keep emitting the legacy 0xCB
   bytes unchanged — 0xCC appears only when a wider form wins. The old
   `registry > 256 states → refuse conversion` rule is gone: a section whose
   gids exceed 255 switches to the u16 stream instead (the registry may
   cross the line MID-conversion; registries up to 65536 states stay
   representable — modded worlds keep the x2.16 lever).
2. **Latent wide-palette corruption fix.** The u8 unpack wrapped voxel
   indices >= 256 (`as u8` on a 9..16-bit stream): the canonical encoder's
   frequency counts, its canonical remap, the section identity AND the biome
   identity all silently mis-mapped for palettes > 256 states whose data
   references the upper half. Vanilla terrain never gets there (its palettes
   are small); modded worlds could. All unpack paths are width-correct now
   (`unpack_nostroddle_wide` / `dense_unpack_vox`).
3. **The transpose decode fix — caught by the corpus gate.** The first
   build forgot to un-transpose gform 2/5 payloads (the transpose
   permutation is an involution — applying it again restores vanilla
   order). jj_profile: `verify ok=944 bad=2`, the two bad chunks showed the
   shift-ref signature exactly: identical per-state histograms, permuted
   positions (0xC9 members expanded against a rebuilt target palette).
   Regression test `glob_transposed_gid_form_roundtrip_byte_identical`
   pins the checkerboard-column case where gform 2 strictly wins.
4. **GID-RLE admission is NOT the canonical rule.** Uncompressed-size
   admission (the canonical 4x rule) lets mid-run sections into 0xCC whose
   slabs then compress WORSE — the raw gid stream's value is cross-section
   byte alignment in the zstd window, which RLE destroys (run boundaries
   differ per section). Measured on the JJ corpus: div=4 → x2.99
   (regression), div=8 → x3.07, div=16/off → x3.08. Default 8, tunable via
   `CSAVES_GID_RLE_DIV` (0 = off). The lever targets moderate-strata
   vanilla terrain (the survival bigworld), where sections fail the
   canonical 4x-vs-packed-width test but clear the 4 KiB gid window.

Corpus (CI-gated): TOTAL **x3.08** (region x3.07, entities x3.11),
`verify ok=946 bad=0`; 50/50 unit tests (new: transposed-gid roundtrip,
u8-RLE roundtrip with the divisor pinned, u16 wide-registry roundtrip).

## v0.9.4 — GLOB (global state registry) ON by default

jj_exp5 measured the section-size frontier on real bigworld tiles: the
winning architecture is a GLOBAL STATE REGISTRY — dense sections convert
to 1-byte global-id streams (175 distinct states in vanilla terrain →
x2.16 on the dense streams), cross-section repeats match through the
registry instead of per-section palettes. v0.9.3 shipped it opt-in
because the natural per-slab choice corrupted 32 of 946 JJ chunks.

**Root cause (0.9.4):** `glob_to_section` rebuilt target palettes in
count-ASCENDING order while the canonical encoder is count-DESCENDING.
0xC9 shift-ref members store their top planes as TARGET-CANONICAL
indices; every shift whose target was glob-rebuilt decoded through a
rotated palette (andesite at index 0). The rebuild now reproduces the
exact `enc_pal_data_canon` rule — glob roundtrips are byte-identical for
encoder-produced sections, shift permutations stay valid, dedup hashes
stay content-pure. Regression test `glob_rebuild_keeps_shiftref_target_order`
fails on pre-fix code; `Store::slab_invariant_scan` + the probe_glob /
probe_bad2 examples document the hunt (the map-vs-disk invariant was
CLEAN at every step — the bug lived in the codec layer, not the store).

Corpus with the natural choice (CI-gated): TOTAL **x3.08**
(region x3.07, entities x3.11), `verify ok=946 bad=0`; FORCE_GLOB
x3.08 bad=0; `CSAVES_GLOB=0` opt-out x2.99. Bigworld rerun queued.

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

## I/O speed (real JJThunder corpus, 2-core hosted CI runner, single-threaded unless noted; v0.9.9 same-run numbers, CI-confirmed)

| operation | Anvil (zlib-6) | c-saves | vs Anvil |
|---|---|---|---|
| region read, warm | 802 MB/s, p50 11.4 us | **1055 MB/s, p90 67.4 us** | **x1.3 aggregate, x2.3 p90** |
| region read, 2 IO threads | — | **31.7k chunks/s** | scales with Moonrise workers |
| region import (fresh write) | 55.5 MB/s, p50 104 us | 37.3 MB/s ST, **75.1 MB/s MT** | 0.67x ST, **x1.35 MT** — tails inside tick budget (p99.9 7.6 ms) |
| region steady rewrite (autosave) | 55.5 MB/s, p90 2.4 ms | **205 MB/s aggregate, p50 9.5 us, p90 0.33 ms** | **x3.7 aggregate, x7.3 p90** |
| 10%-dirty autosave cycle | — | **515 MB/s aggregate** | real mutations, p50 7.1 us |
| entity write (fresh) | 173.4 MB/s | **873.4 MB/s** | **x5.0 faster** |
| entity steady rewrite | 173.4 MB/s | **8212 MB/s** | **x47 faster** |
| container size (sector-padded) | 12.46 MiB | **1.81 MiB** | **x6.88 smaller** |

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
