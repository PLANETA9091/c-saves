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

## v0.9.49 — pristine-aware export: the round-trip closes, every None gets a name

The one operator-facing path that still had a silent None was the Anvil
exporter: it skipped every key whose `read_chunk` returned None — which
is exactly what a pristine regeneration contract returns BY DESIGN. A
pristine pregen world (the -99% class) exported as region files missing
every contracted chunk, with no counter and no way to tell elision from
loss; mappers/converters would report holes as corruption. The exporter
now accounts for every key: `ExportReport { regions, chunks, bytes,
pristine_skipped, lost }` with the identity `chunks + pristine_skipped +
lost == store.keys().len()`; contracted chunks are counted as
`pristine_skipped` (absent BY DESIGN — vanilla regenerates them from the
seed on load, the same semantic the live read path uses) and announce
themselves on stderr; genuinely unreadable keys are counted as `lost`
with per-key stderr (forensics in `read_fail_reason`).

The round-trip is now a measured invariant (`export_roundtrip` probe,
real 728-chunk corpus):

- **Phase A (flag-off): container → Anvil → container byte-identical
  728/728** — the exporter is lossless for honest storage;
- **Phase B (mixed state — pregen + player edits: 720 import contracts
  + 8 live-path flips): export writes exactly the 8 stored chunks +
  pristine_skipped=720; re-importing the export re-contracts 8/8 with
  raw legends byte-equal to the stored-A payload hashes; the 720
  contracted keys are absent from the export** — the honestly-stored
  part becomes the new contract set, the regenerated part stays
  regeneration semantics.

CI gains the roundtrip gate (Phase A + Phase B counts pinned,
`bench_export_roundtrip.txt` artifact). 70/70 tests (new:
`export_accounts_for_every_key_and_roundtrips`). Research §13: the last
silent None on an operator path is counted, named, and gated — export
accountable.

## v0.9.48 — pristine contract audit: the wrong assertion is caught at deploy time

The research docs (§10/§11) left one named frontier after the semantic
line closed: "contract/regen-drift audit tooling". v0.9.48 ships it as
`pristine_audit` — a read-only operator tool that re-derives every
pristine contract legend from the ORIGINAL Anvil corpus and predicts,
before the world ships, exactly which contracts would flip in production:

- **inventory** — contracts, strip-canonical (v0.9.46) vs raw-only
  (0.9.44/45) legends, stored keys, container size (read-only recipe
  walk through the new `Store::pristine_legend(key)` face);
- **legend check** (`--corpus`) — `hash_of` + `canonical_contract_hash`
  re-computed from the corpus bytes, the exact hashes the contract check
  uses (the legend is formed AND compared on the INCOMING payload bytes;
  verified against the write path). Classification per chunk:
  `raw_match` (byte-identical — bulletproof) / `canon_match` (the
  relight/timer/key-order class the legend absorbs) / **DRIFT** (the
  contract WILL flip — the pure-worldgen assertion is wrong for that
  chunk: wrong seed, post-drain edit, generator drift);
- **session check** (`--session`) — the §10 relight model (fresh light,
  bumped timers, key-order drift) replayed against the stored legends:
  kept/flipped prediction per chunk;
- exit code 0/1 = a CI-able predeploy gate; no writes to the audited
  store, ever; O(1) payload memory (region-file → hash → drop), so it
  scales to the 262k-chunk class; `--import` mirrors the operator flow
  (stage a copy — the production import drains its source — import, then
  audit against the untouched original).

Measured on the real corpus, end-to-end (`--import --session`): import
728 chunks, **legend check raw_match=728 drift=0**, **session
kept=728/728** (relight raw-hash differs 728/728 — every raw miss is
canon-held), verdict PASS. Negative control (one tampered payload, +17 B
root field — a real content change the legend must not absorb):
**drift=1, session_flipped=1, verdict FAIL, exit 1**, the tampered chunk
named. The wrong assertion is now a deploy-time red light, not a
production erosion story. CI gains the audit gate (import → audit →
ship, all counts pinned, `bench_pristine_audit.txt` artifact).

With this the §10/§11 frontier list is empty: bytes closed (§8/§9),
erosion closed (§10), first-write hazard closed (§11), assertion
auditable (§12). 69/69 tests (new: `pristine_audit_legend_accessor`).

## v0.9.47 — shim-evidence pristine gating: the residual risk closes at the source

The 0.9.44-0.9.46 contracts carried one documented, operator-accepted
residual risk: mutations landing between worldgen and a chunk's FIRST
durable save (a player walking into a freshly generated chunk before the
next autosave, Paper session metadata, a mod tag) were hashed INTO the
contract and silently reverted on regeneration — the contract's read=None
semantics made the pre-first-save edit disappear. The research docs pinned
the fix as "shim-side isNewChunk / InhabitedTime==0 passed as a per-write
flag"; v0.9.47 ships exactly that, with evidence read from the tag the
server itself hands the shim — no Paper patch, no generation-write
instrumentation:

- evidence rule (`SavesShim.isFreshChunk`, both write paths — Moonrise
  `moonrise$startWrite` and the legacy sync `write`): the chunk tag must
  certify **Status=minecraft:full && InhabitedTime==0** at the moment of
  its first durable save. Status=full excludes proto-stage saves (a proto
  contract could only flip on the later full re-save — now proto content
  stores honestly instead of forming a doomed contract). InhabitedTime==0
  excludes any player-visible interaction (standing in/near the chunk
  ticks it). An ABSENT InhabitedTime key is treated as not-fresh
  (conservative: only a tag that carries the zero is evidence);
- plumbing: `nWriteChunk(III[BZ)Z` (shim + RegisterNatives in lockstep —
  the module ships both), native `write_chunk_fresh(key, payload,
  whole_mode, fresh)` is the evidence channel; `write_chunk` (direct API)
  asserts freshness — the gate lives where evidence exists;
- cfg: `pristine_fresh` (default ON; `CSAVES_PRISTINE_FRESH=0` restores
  the exact 0.9.44-0.9.46 unconditional first-write semantics). The
  import/strip path is UNCHANGED — pregen worlds carry no runtime tag
  evidence, import contracts stay operator-asserted (`pristine_import`);
- measured on the real corpus through production write paths
  (`pristine_fresh_probe`, first write = payload + a session mutation
  modeled as an extra root field, fresh=false):
  **A) gate ON: pristine=0 stored=728, edits_surviving=728/728** — every
  pre-first-save edit survives regeneration (compact 1.37 MiB, x0.31);
  **B) pre-0.9.47 semantics: pristine=728, edits_reverted_on_regen
  =728/728** — the hazard, measured head-on; every contracted edit reads
  None BY DESIGN; **C) pure-pregen invariance: fresh first writes all
  contract, one relight session keeps pristine=728 stored=0, compact
  0.03 MiB (x0.0057 vs anvil)** — the huge-world win is byte-for-byte
  intact, the CI pure-pregen gate keeps its margin (x0.01, ratio ≤ 0.02);
- observability: `ST_PRISTINE_UNFRESH` counts first writes stored
  honestly because the evidence said not-fresh (surfaced in stats_json as
  `pristine.unfresh_stored` next to `pristine.canon_kept`) — the audit
  number: every count is a mutation class the contract path can no longer
  revert;
- new tests: not-fresh first write stores honestly and never contracts
  later (flip/edit semantics intact), fresh first write contracts
  (win unchanged), opt-out restores 0.9.46 semantics; 68/68.

Flag-off is byte-identical to 0.9.46: pristine defaults OFF, enc_fp EXACT
6d240d7166cdda99/12130/9971843, JJ corpus verify ok=946 bad=0, bench_size
compact x0.08, pregen gate x0.01 (152 contracts, relight replay 318/318
kept). The semantic-line frontier from §10 gains its last missing piece:
contracts now form only on chunks the SERVER certifies as pristine.

## v0.9.46 — strip-canonical contract legends: relight re-saves stop eroding the huge-world win

0.9.45's import contracts flip on the first **byte**-different re-save. A real
Paper session re-saves contracted chunks with fresh light and bumped
InhabitedTime/lastUpdate the moment a player merely walks past — every such
re-save eroded one contract into full storage (measured: ONE relight session
over the 728-chunk real corpus flipped 728/728 contracts, container back to
1.45 MiB x0.32). The contract now carries a second, **strip-canonical** hash:
an order-free structural digest of the payload minus everything a pristine
chunk regenerates anyway — per-section SkyLight/BlockLight (+ starlight state
bags), isLightOn, and the root lastUpdate/LastUpdate/InhabitedTime counters.
DataVersion, Status, palettes/block/biome data, Heightmaps, block_entities —
all still hashed (sorted-key order), so any seeded-content difference still
flips; only light-engine and save-tick volatility stops flipping.

- contract format: `[mode 7][raw 16B][canon 16B]` = 33 B (raw-only 17 B
  contracts from 0.9.44/0.9.45 containers parse and behave exactly as
  before); the O(1) raw fast path is untouched, the canonical legend only
  arbitrates byte-different re-saves;
- erosion A/B on the real corpus (728 chunks, one relight session replayed
  through the production import + write paths, `pristine_canon_probe`):
  canon OFF = 0.9.45 semantics, pristine=0 stored=728, compact 1.45 MiB
  (x0.32 vs anvil); **canon ON = pristine=728 stored=0, compact 0.02 MiB
  (x0.0054 vs anvil, -99.5%)** — the session no longer costs the huge-world
  win;
- pure-pregen demo (9,216 chunks): compact 0.43 MiB, x0.01 vs anvil, 42
  B/chunk all-in contract cost (the 33 B legend doubles the 17 B recipe;
  the CI pristine gate ratio ≤ 0.02 keeps real margin at 0.008);
- the CI pristine gate now also protects the legend: bench_size phase B2
  replays a relight session over a sample of contracted chunks and FAILS
  if any of them flips (measured 318 relit re-saves → 318 contracts kept,
  zero bytes stored);
- canonical formation cost measured honestly: ~1.6–1.8 s per 728 corpus
  chunks (~2.3 ms/chunk — full parse + order-free structural digest, 23 MB
  payload, 2-core sandbox); it rides the background import drain, and
  `CSAVES_PRISTINE_CANON=0` restores the raw-only 0.9.45 semantics;
- documented side measurement (`keys_probe`): the import rebuild form
  differs from the raw payload under the canonical hash for 204/728 corpus
  chunks (CSEC canonical palette order vs vanilla order in rebuilt
  sections) — irrelevant for contracts, because Paper REGENERATES a
  contracted chunk (read = None BY DESIGN) and the re-save comes from
  Paper's own serializer; the probe pins this reasoning.

Flag OFF is byte-identical to 0.9.45: enc_fp EXACT
6d240d7166cdda99/12130/9971843, bench 64x5 compact 1.24 MiB x0.08 verify
1957, 66/66 tests x3 (new: relight re-save keeps the contract, raw-only
opt-out falls back clean, canonical-hash strip-context isolation incl.
non-sections "BlockLight" name collisions).

## v0.9.45 — pristine IMPORT contracts: the huge-world lever lands

v0.9.44 contracted only the live path. Imported worlds — the exact shape of
huge pregenerated worlds — were exempt ("imported content is not
seed-reproducible"), which meant the -90% class was unreachable exactly
where huge worlds live. That exemption is now an explicit operator mode:
`CSAVES_PRISTINE=import|2` (or `CSAVES_PRISTINE_IMPORT=1` alongside
pristine=1) lets the import/strip path (`write_chunk_import`, the background
Anvil drain) form regeneration contracts too.

- same recipe mode 7 mechanics: 17 B contract, zero blobs, read = absent →
  Paper regenerates from the seed, flip-on-mismatch, persists across
  restarts; whole-mode stores (entities/POI) stay exempt;
- real-corpus import (bench-corpus/dim region, 728 chunks, 4.47 MiB anvil):
  pristine-off 1.17 MiB compact (x0.26) → **import-contracts 0.01 MiB
  (x0.002 vs anvil, -99.8%, 0.011x of pristine-off)**, pristine=728
  stored=0, import wall 0.2 s → 0.0 s (`pristine_import_probe` example,
  imports from a disposable copy — import_dir drains its source);
- pure-pregen world (9,216 chunks, live path, no edits): region logical
  533.8 MiB → **0.28 MiB total (x0.01 vs anvil, -99.2%)** — 26 B/chunk
  contract cost (9,216 recipes = 239,616 B incl. record overhead), zero
  blobs; CI gains a pristine pure-pregen gate (`CSAVES_PRISTINE=1
  bench_size 96 0`: compact ratio ≤ 0.02, pristine verify sample ≥ 100);
- honest erosion model: the operator ASSERTS the imported world is pure
  worldgen under the seed it will be restored with. On restore, any re-save
  that hashes differently (fresh light after relight, serializer/generator
  drift) flips that chunk to normal storage — SAFE, the regenerated content
  is stored — so erosion is bounded to chunks the server actually loads;
  the untouched pristine mass stays contracted;
- projection (arithmetic, not a measurement): 26 B/chunk × 261,572 chunks ≈
  6.8 MB of contracts for the 2,136 MiB bigworld-survival region data.

Flag OFF is byte-identical to 0.9.44: 63/63 tests (new: import contracts
flip/persist; the live-only import exemption stays pinned), enc_fp EXACT
6d240d7166cdda99/12130/9971843, bench 64x4 compact 1.24 MiB x0.08 verify
1661, jj x3.71 ok=946, survival x4.28 ok=1280.

## v0.9.44 — pristine-chunk elision: the order-of-magnitude lever, opt-in

The §2 semantic lever from the compression research ships. With
`CSAVES_PRISTINE=1` (region stores only; entity/POI stores are exempt by
construction), the FIRST-EVER write of a chunk becomes a **regeneration
contract** instead of stored content:

- recipe **mode 7** carries ONLY the payload hash — zero blobs, 17 B on
  disk (the smallest recipe form; encode always picks it, it can never
  silently degrade into storage);
- `read_chunk` returns None → the shim's `nReadChunk → null →
  vanillaReadData → NO_DATA` chain makes **Paper regenerate the chunk from
  the seed** — the exact semantics of a missing chunk in a save format;
- any later write with DIFFERENT content flows through the normal pipeline
  and supersedes the contract (**flip-on-mismatch**); an identical re-save
  hits a dedicated pristine fast path and stays a contract — including
  across RESTARTS (the contract hash lives in the recipe, not in RAM);
- import/strip writes never form contracts (imported content is not
  seed-reproducible); the whole-mode write path (entities/POI) never forms
  contracts;
- verify/tooling: `is_pristine(key)`, the CI benches gain a
  `pristine=N` bucket, contracts read as None BY DESIGN.

| gate | 0.9.43 (pristine off) | 0.9.44 pristine ON (bench 64x4) |
|---|---|---|
| c-saves fresh | 5.08 MiB | **0.11 MiB (x0.01 vs anvil)** |
| c-saves compact | 1.24 MiB (x0.08) | **0.59 MiB (x0.04 vs anvil)** |
| D verify | 1661 chunks, 25.5 s | 1623 + 38 contracts, 20.5 s |
| C compaction | 22.8 s | 9.5 s (nothing to compress) |

With the flag OFF every gate is byte-identical to 0.9.43 (62/62 tests incl.
two new pristine contract tests: roundtrip/flip/restart-persistence and
default-off/import-exempt; enc_fp EXACT
6d240d7166cdda99/12130/9971843; JJ ok=946 bad=0 pristine=0 x3.71;
survival ok=1280 bad=0 x4.28; bench compact 1.24 MiB x0.08).

**Honest semantics.** A contract asserts "this chunk equals what the seed
generator produces". The residual risk an operator accepts with the flag:
mutations between a chunk's generation and its FIRST-EVER durable save are
captured in the contract hash and get reverted on regeneration — the same
semantics the Bedrock Editor chunk-reset and community chunk-regen flows
accept. Everything AFTER the first save is provably safe (hash flip). A
future shim-side tightening (isNewChunk marker, InhabitedTime==0 filter at
the write call) can narrow the window further; the Rust contract already
accepts a per-write marker.

## v0.9.43 — the LZMA2 encoder goes EXTREME; PPMd and the solid-window export framing measured dead

One encode-only flag, every read byte untouched: `lzma2_compress` now uses
**preset 6 | PRESET_EXTREME** (deeper match search, nice_len 273 vs 64). The
EXTREME bit changes only encoder search parameters — the dict stays preset-6's
8 MiB, so `lzma2_decompress` and every 0xFE stream ever written stay exactly
as they are (old containers decode; new containers decode on old code too).
Measured on the real post-dedup member stream (12,130 members, one 6.29 MiB
group): 940,873 -> 915,488 B = -2.70%, with 6e/8e/9e producing IDENTICAL
bytes — the win is search depth, not window (dict 64 MiB measured +0.00%).

| gate | 0.9.42 | 0.9.43 (6 \| EXTREME) | floor |
|---|---|---|---|
| bench_size compact (4096 ch) | 1.35 MiB | **1.24 MiB (-8.1%)** | <0.15 |
| region slab bulk (exact) | 1,306,138 B | **1,190,263 B (-8.87%)** | — |
| JJ container | x3.36 smaller | **x3.71 smaller** | ≥2.6 |
| survival container | x4.01 smaller | **x4.28 smaller** | ≥3.0 |
| verify | 1661+946+1280 bad=0 | same | =0 |
| enc_fp | 6d240d7166cdda99 / 12130 / 9971843 | **EXACT** | pin |
| pack wall (C step, 2-core) | 8.1 s | 22.5 s | background |

The pack wall grows 1.66-2.8x depending on material (repetitive synthetic
terrain feeds the deeper search superlinearly) — background pass, watermark
throttled, per-stream parallel, same accepted trade class as 0.9.41/42.

The research tick that found it also CLOSED the codec front with three new
measurements: **PPMd solid-window** (the cheap proxy for a slab-level global
context model) LOSES to LZMA2-6 by +6.1% at order-16 on the raw corpus —
general-purpose CM does not bite this material, the slab-CM frontier stays
parked with a second independent negative; **brotli -11 solid** wins only
-3.2% over LZMA2-6 (8x slower), so a whole-container archive-export lever is
worth ~3%, not a feature; and the remaining zstd streams (recipe batches,
regular cold blobs, dict files) are SPEED-PINNED by architecture — recipe
flushes run in the write thread's critical path (z19 there measured ~49% of
fresh-import wall), cold blobs and dicts are negligible absolute bytes.
What remains for "world weighs less" is SEMANTIC: pristine-chunk elision
(§2, order-of-magnitude, needs the Paper-side marker product decision).

## v0.9.42 — LZMA2 becomes the default cold codec; per-section entropy coding measured negative

The 0.9.41 opt-in graduated to the default after the full gate sweep:

| gate | z6 (old default) | LZMA2 (new default) | floor |
|---|---|---|---|
| bench_size compact | 1.70 MiB (x0.11) | **1.35 MiB (x0.08)** | <0.15 |
| JJ container | x3.26 smaller | **x3.53 smaller** | ≥2.6 |
| survival container | x3.12 smaller | **x4.05 smaller** | ≥3.0 |
| cold-read verify (1661 chunks) | 28.2 s | **25.8 s** | — |
| import ST | 126 MB/s | 132 MB/s | ≥25 |
| verify | 728+218+946+1280 bad=0 | same | =0 |

The cold-read wall is neutral-to-better: the 20% smaller container buys back
the slower decode. The pack wall (1.0 -> 8.4 s on the 2-core bench scale)
stays background, watermark-throttled and per-stream parallel by design.
`CSAVES_COLD_CODEC=zstd` opts back out; the per-record codec stamp makes
both containers readable either way. enc_fp pin EXACT — the encode path is
untouched.

The same research tick took per-section entropy coding all the way to a
green implementation (order-0 rANS and KT-adaptive context modeling with
the context = left+above voxel, exact lgamma closed-form code lengths) and
measured it NEGATIVE end-to-end on both real corpora (+8.8% z6 / +6.7%
LZMA2 on JJ; -10% survival, breaking the 3.0x gate) despite -31.8% raw
member bytes: slab streams win through CROSS-SECTION alignment (glob gid
streams, RLE, dense zero-runs) that the codec harvests below any
per-section entropy floor. Implementation parked on `wip-cm-section-form`;
verdicts, probe tables and anti-retest gates in
`docs/compression-research.md` §6.

## v0.9.41 — opt-in LZMA2 cold codec: the research probe ships

The compression-research tick (docs/compression-research.md) probed codec
families on the real corpus and found the one lever the zstd family cannot
reach: **raw LZMA2 -6 compresses the slab streams -24.8% vs zstd-6 — and
-8.6% below the zstd-19 family ceiling — at the same wall time** (brotli -11
is 2.5% smaller but 8x slower; delta filters, byte transposes and BWT all
measured negative on voxel NBT). v0.9.41 ships it as an opt-in cold codec:

- `CSAVES_COLD_CODEC=lzma2` — cold slab streams (canon AND glob variants,
  the smaller still wins) go through raw LZMA2 preset 6 (liblzma, vendored
  static build — no system package). FIXED preset on both encode and decode
  (the raw format does not carry the dict size); CSAVES_COLD_LEVEL stays
  zstd-only.
- **The codec rides in the slab record's dict_id byte as the 0xFE
  sentinel** — the same wire-byte dispatch the 0.9.39 raw sentinel uses, so
  one hook at the top of decompress() covers slab cache, read_blob, batch
  loads, scan rebuild, pack re-group and verify with zero call-site
  changes. Dict-version space is capped at 253 so a store can never mint
  0xFE as a dict id. Hot frames keep 0xFF (raw); per-blob cold re-stores
  and recipe batches stay zstd (their dict_id slots carry real dict ids).
- Errors never mislabel a stream: lzma2_compress returns None -> the
  per-record stamp falls back to zstd together with the bytes.

Measured on the real container (bench_size, 9216 chunks, 5 edit rounds):
compact **3.80 -> 3.02 MiB (-20.5%)**, x0.08 vs Anvil (gate 0.15); slab bulk
3,723,428 -> 2,907,791 (-21.9%); fresh/hot 10.97 MiB unchanged; import
126 MB/s unchanged. The cost is honest: pack wall rises ~2-5x (LZMA2
compresses at ~4.6 MB/s single-thread; per-stream parallelism still spreads
across cores) — that is why it is opt-in, the default pack-water behavior
stays zstd-6.

Gates: 59/59 tests (new: cold_lzma2_slab_roundtrip — write, pack, drain,
reopen, read through LZMA2 slabs), enc_fp EXACT
(6d240d7166cdda99/12130/9971843), verify 728+218 bad=0 in the lzma2 mode.

## v0.9.40 — the ensure stage stops paying for its own copies

Fresh stage telemetry after 0.9.39 showed the two remaining per-frame costs
in `flush_hot_slab_group` were not compression but COPIES and HASHING:

- **The splice memmove (change A).** The raw-frame path built
  `[tab][raws...]` (~150 KiB) and then `splice(0..0, [0u8; 12])` to make
  room for the slab header — a full-frame memmove per flush hiding behind
  the "patched in place" comment. The body is now built with the 12-byte
  header reserved up front and patched after: slab_comp 6.2 -> **0.05
  us/frame**, byte-identical output.
- **dict_sample hashed at SipHash speed (change B).** The sample path took
  up to 16 KiB per member through `DefaultHasher` (~1.5 GB/s) — the whole
  17.6 us/frame dict_sample timer was THIS hash. `xxh3_64` over the same
  truncated bytes: 17.6 -> **9.2 us/frame**, now the data movement itself.
  Bucket keys shift (which samples train the hot dict), but the contract
  holds — same content set -> same dict; member raws, enc_fp and the
  drained container are untouched.

Same-host A/B (2-core, warmup `3 2` -> three `1 2` rounds): baseline
120.8-128.4 MB/s (median 125.7) -> patched **126.8-131.3 (median 131.0)**,
+3-4% import ST; ensure stage 30.4 -> 28.6 us/chunk; MT 195.8 MB/s (in the
documented +-25% band). The remaining dict_sample cost is the xxh3 over the
member raws themselves — the honest floor of "hash what you train on".

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843), verify
728+218 bad=0.

## v0.9.39 — the hot tier stops pretending: raw slab frames + the tail joins the frame

The ensure stage had grown back to 21% of the import wall (58.3 µs/chunk
of 275). The new `stages-ensure` decomposition (kept as CI telemetry)
found where: the slab flush burned 139 µs PER FRAME in `slab_comp` — and
a standalone probe showed bulk zstd costs 155-220 µs on a ~34 KiB frame
INDEPENDENT of level (-3 through 3), i.e. pure frame machinery, while a
memcpy of the same bytes costs ~0. The hot tier is transient material the
pack pass re-stores at the cold level anyway — paying a zstd frame for it
is work the final container never sees.

- **Raw hot slab frames (change A).** `flush_hot_slab_group` no longer
  compresses: the SLAB V2 stream is stored verbatim behind a
  `dict_id=0xFF` sentinel (dict ids are 0..=254 and slabs never carried a
  dict — unambiguous, and old containers can never contain it). One hook
  at the top of `decompress()` covers every read path (slab cache,
  read_blob, batch loads, scan rebuild, pack re-group); the slab header is
  patched in place in front of the stream, saving a full-frame memcpy per
  flush. Drained containers are byte-identical (modulo the documented
  ±19-byte tail-ordering jitter): the cold end state depends only on
  member raws, which did not change.
- **The tail joins the frame (change B).** A blob-eligible tail
  (>= `tail_blob_min`) rides the hot-slab batch as its LAST member —
  no standalone compress + record + append per tail; the batch map phase
  dedups it for free. Below the slab threshold the old per-blob path
  applies, tail included; tiny tails stay recipe-inline.

Same-host measure round (2-core, warmup `3 2` -> `1 2`): import ST
115.5 -> **130.2 MB/s (+12.7%)**, wall 0.205 -> 0.182 s — now 2.39x the
Anvil write rate on the same run; import MT 189.8 -> 201.2 MB/s (MT
bounces ±25% run-to-run on this co-tenant host; ST is the stable metric).
Ensure stage 58.3 -> 31.1 µs/chunk (-47%): slab flush 158.9 -> 67.7 µs
(slab_comp 139.3 -> 6.4, tail_blob 10.5 -> 0 per chunk). The transient
cost is honest disk: undrained containers hold raw member bytes until the
pack pass drains them — the packed container is unchanged at 1.94 MiB
(x6.42 vs Anvil sector-padded).

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843),
jj verify 946/946 bad=0, verify 728+218 bad=0, survival TOTAL x3.12
ok=1280 bad=0 (the documented post-0.9.34 z6 baseline), bench_size
compact x0.11 (gate 0.15), container x6.42.

## v0.9.38 — the encode loop closes: three probe verdicts, one presize

With unp (2593 ns/call) and pack (2199) carrying 44% of the scan wall and
both SIMD directions already documented-negative, this round probed the last
unprobed corners of the encode path — in-bench, output-pinned, one shipped:

- **RLE-A fusion — measured NEGATIVE, reverted.** The bounded SIMD
  transitions scan feeding the RLE-A candidate (305 ns/call) is computable
  as a register-only cmp+add riding the unpack loop — decision-identical
  (below cap both counts agree, at/above cap both reject). Measured:
  unp 2593 -> 3906 ns/call (+50.6%), scan 204.7 -> 231.0 µs/chunk, import
  119.4 -> 108.1 MB/s. The extract+counts loop is IPC-bound at ~4 µops per
  voxel: ~3 extra register µops cost +50%, and no saved scan can pay for
  that. THIRD negative verdict of this class (0.9.24 AVX2 bits4, 0.9.37
  nibble rewrites) — the encode loop admits no extra µops at any width.
- **hash_lite fold — sized and parked.** sh is 16.8 µs/chunk (127 ns x
  95434 sections), but its floor is the data-array hash itself: one xxh3
  update over the 8 KB LE long stream (~270 ns per block_states section) is
  mandatory dedup work that a fold cannot remove — folding only saves the
  dispatch/palette re-walk (~25-30 ns/section), a +2%-class play for a
  three-implementation hash-scheme rewrite. The numbers stay here.
- **Pack kernel — near its scalar floor.** packb per-call costs (bits2
  2091 ns, bits3 4293, bits4 4869 per 4096-voxel call) sit within ~30% of
  the scalar IPC bound for remap-load + shift + or per voxel; the 0.9.17
  group-split already broke the accumulator chain. SWAR/pdep rewrites
  projected at ~1.3x on 14 µs/chunk — parked.

Shipped: `section_encode_view` presizes the output from the known payload
extents (dense bytes are never larger than the input longs — out_bits <=
in_bits — plus light arrays and a 1.1 KB palette/framing headroom); the old
5-byte start grew ~9 times per section inside the enc timer. pack timer
2199 -> 2078-2167 ns/call across runs; import flat within this shared
2-core host's noise (ST 117.3-119.4 MB/s band; MT bounces ±25% run-to-run —
the MT-ensure timer alone measured 54.3M vs 82.3M ns on identical bytes).

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843), verify
728+218 bad=0, pack 0.41 s, container 1.94 MiB x6.42, warm read 1490 MB/s.

## v0.9.37 — the shift pass stops hashing: fingerprint dedup without SipHash

The 0.9.36 stage decomposition left residues inside scan_split unexplained,
so the split gained three probe timers (scanr / encl / tailb) and the full
shift-pass counter block (shiftc / shiftb / shiftt in split_stage_json).
The decomposition (probe-timer-inclusive, same instrumentation on both
sides): scan 219.6 us/chunk = scanr 24.7 (secb 21.7 = the fused walk) +
encl 138.4 (enc 105.7 = unp 48.4 + pack 41.5 + canon/pal ~15.8, plus
sh 17.0 outside the enc timer) + shift 49.2 + tailb 1.1 + residual 6.2.
The shift pass decomposed further: builds 18.0 (fp 4.9 + id 4.3 + key 0.6;
its ident fill is counted in unp), tail materialization 16.7, matching
13.4 — and the matching cost was NOT the 447k plane-fingerprint compares
(they reject 99.93% at plane0, ~2 us/chunk): it was the per-i
candidate-dedup `HashMap<Hash, ()>` — ~0.5M SipHash inserts (~25 ns each)
plus the map growth reallocs — pure hashing, ~14 us/chunk.

Changes, all output-identical (enc_fp pin EXACT):

- Candidate dedup: one u64 bloom bit (AND + bit test for a fresh key)
  over a reused scratch Vec (capacity == CANDIDATES, cleared per i); only
  same-slot collisions fall through to the exact Vec scan, so the dedup
  semantics are unchanged. Matching 13.4 -> 6.7 us/chunk.
- The identity-intern table (`tab`), the ref-materialization maps
  (t_rev / extra_of_id) and the per-thread enc cache switch from SipHash
  to the RawBuildC bucket hasher (keys are already fingerprints or
  chunk-global ids; correctness rides on full-key Eq). Tail
  materialization 16.7 -> 9.4 us/chunk.

Verdicts, documented negative: the nibble unpack kernel rewrite
(byte-plane extraction + 4-way interleaved sub-counters to break the
counts store-forward chain) measured 2.57 -> 3.78 us/call with branchy
worst-tracking and 2.57 -> 3.38 with predicted-not-taken checks — both
reverted; LLVM already schedules the constant-shift loop better than the
byte-pair form (an independent second confirmation of the 0.9.32
verdict). The shift-side tail/materialization HashMaps keep their
allocations; a scratch-vec replacement was considered and parked — the
remaining 9.4 us/chunk is dominated by the per-ref view clone +
non-block_states re-encode, not the maps.

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99/12130/9971843),
import ST 112.9 -> 117.5 MB/s (p50 60.2 -> 58.4 us), import MT ~180 ->
~186 MB/s (2-thread, noisy shared host — stage telemetry is the honest
witness), scan 219.6 -> 207.3 us/chunk, shift 49.2 -> 38.0, pack
0.42 s at z6, container 1.94 MiB x6.42, bench_size compact x0.11 (gate
0.15), survival verify ok=1280 bad=0, verify 728+218 bad=0.

## v0.9.36 — the root walk parses the section list in place (one walk, not two)

The import split walked every chunk's bulk TWICE: scan_root ran a raw
`skip_value` over the sections list (~30 KB per real chunk) only to record
its extent, and then `parse_sections_direct` walked the SAME bytes again to
parse them. v0.9.36 fuses the two: the direct walker gains an
end-as-output form (`parse_sections_direct_from`) and scan_root calls it
AT the sections key — the walk defines the extent, the parsed
`Vec<SecLite>` rides out in `ScanOut::direct`, and scan_split consumes it
(the direct retry / wrap / Value chain stays for any list the walker
refuses, byte-identical routes). Telemetry: scanp 21.1 µs/chunk → 0.03 µs
(the second walk is gone), scan 222.8 → 216.8 µs/chunk.

Honest sizing: the 0.9.16 note's "~30-40 µs skip" does not hold on this
corpus/binary — the new root-branch probes (`"root"` block in
split_stage_json: secb/hmap/skip) decompose scan_root as sections-branch
22.1 µs (the walk itself), Heightmaps 0.07 µs (post-memo), generic-key
skips 2.3 µs — the raw skip it replaces measured ~6 µs, so the net scan
gain is −6 µs/chunk (−2.7%), import ST 112.5 → 114.8 MB/s. The value is
structural: the chunk bulk is walked once, and the walk now OWNS the parse
(the next levers build on it). Also measured and settled this session:

- The enc-cache is NOT capacity-limited: 12146 encode misses per round at
  both 32 MiB and 256 MiB caps — they are the corpus's structurally UNIQUE
  sections (the other 83288 of 95434 hit at 87% within one import; bench
  rounds are separate processes, so cross-process retention is out of
  scope). The misses are mandatory work; the lever is the encode kernel
  itself (unp 2.58 + pack 2.20 µs/call = 88 µs/chunk — next candidate).
- container-fp is NOT a stable pin on this sandbox: four different values
  across runs of near-identical code (member bytes 1728105..1728193),
  including runs with the change fully disabled — pre-existing environment
  drift (the 0.9.30 lesson, now visible at fp level). The import-level
  enc_fp is the honest pin: 6d240d7166cdda99 / 12130 / 9971843 re-verified
  EXACT with the fused path ON.

Gates: 58/58 tests, enc_fp EXACT (fused on and off), verify 728+218 bad=0,
pack 0.41 s at z6, container 1.94 MiB x6.42, warm read 1508.9 MB/s. Next
levers by size: fused unpack→dense-pack kernel inside enc (88 µs/chunk,
documented-negative territory — probe first), shift-pass fingerprint
pre-filter (28 µs of matching after builds), hash_lite folded into the
direct walk (16.9 µs).

## v0.9.35 — concurrent supersede+pack: the batch_idx window that lost chunks

The CI test job on v0.9.34 failed three tests: the two glob-RLE form pins and
`concurrent_supersede_during_pack_keeps_newest_state` ("chunk 16 lost"). The
LEAK lines in the CI log gave the exact sequence: 8 `batch_idx miss` during
the seg-2 rewrite, seg-2 and seg-7 kept poisoned, then chunk 16 unreadable.

Root cause — the supersede path eagerly removed the key's `batch_idx` entry
BEFORE queuing the new pending recipe:

1. while chunks still pointed BATCHED at the old batch, batch_idx had no
   entry, so every read of the chunk resolved to None (the loss), and
2. a concurrent segment rewrite hitting the window took the batch_idx-miss
   branch and poisoned the whole segment (the CI log's 8 misses), and
3. a flush racing between the chunks read and the eager remove could delete
   a FRESH index entry the flush had just written (a second, independent
   loss path).

Fix, three parts: the new pending entry is queued BEFORE the committed
version's refs are dropped (a concurrent rewrite now always sees it and
skips instead of re-queueing stale bytes over the newer pending); the old
version resolves through the CAPTURED location via the new `load_batched`
helper (immune to a concurrent flush re-pointing batch_idx mid-resolution);
the batch_idx entry survives until the flush overwrites it. The rewrite loop
gains a pending-skip (pre-pull check + atomic check-and-push after the pull,
so last-write-wins can never roll a chunk back) and an anti-spin poison when
a stuck pending would never drain.

The two glob form failures were a different disease with the same trigger:
the tests wrote `CSAVES_GID_RLE_DIV` via `std::env::set_var` while the pack
path resolves that knob into a OnceLock (v0.9.32/33) — the resolution races
setenv from parallel tests, and setenv concurrent with getenv is UB besides.
All 14 test env writes are now test-only override statics; the CSAVES_GLOB
"1"-set/remove pair (a semantic no-op — the default is enabled) is gone.

Gates: 58/58 tests x8 runs at `--test-threads=8`, enc_fp EXACT
(6d240d7166cdda99 / 12130 / 9971843 — import untouched), pack 0.41 s at z6
(2-core sandbox), container 1.94 MiB (x6.42 vs sector-padded anvil), verify
728+218 bad=0.

## v0.9.34 — cold back to z6: the CI corpora overrule the local corpus

v0.9.33's cold 6 → 4 was decided on the local dim corpus, where the compact
gate moved x0.10 → x0.13 (gate 0.15) and everything looked green. The CI
real-world corpora disagreed, loudly:

| gate | 0.9.32 (z6) | 0.9.33 (z4) | floor |
|---|---|---|---|
| jj-bench TOTAL | x2.86 smaller | **x2.59 — FAIL** | 2.6 |
| survival-bench TOTAL | x3.13 | **x2.50 — FAIL** | 3.0 |

Per-component attribution: entities 3.03 → 2.96 (noise, the GLOB split did
not touch entity bytes) while region 2.83 → 2.52 — the entire miss is the
cold level inside the region streams. Survival section data (ore noise) is
where cold level bites; the GLOB-flat dim corpus cannot see it. z5
interpolates below the survival floor too — **z6 is the fastest level that
clears both floors with margin** (survival +4%, jj +10%).

Everything else from 0.9.33 stands: the 3-stage GLOB split, the parallel
pack build, bounded RLE admission. CI pack wall at z4 was 0.19 s; at z6 it
is expected ~0.3 s — still ~25x under the 0.9.26 pre-MT wall, and the
import scan stage (189 µs/chunk, 73% of the split) is now the real wall
anyway. Lesson recorded in `Cfg`: the cold level is a storage-engine
parameter owned by the real-world ratio gates, not by the local speed
corpus.

## v0.9.33 — pack build goes parallel (3-stage GLOB split) + cold z4

The sub-census found the pack build phase 95% GLOB conversion
(`bglob 145 ms` of `build 152 ms`), and Phase A was strictly serial —
`groups.into_iter().map()` — while the compress pool waited. The conversion
could not simply be parallelized: registry gid ASSIGNMENT ORDER pins every
stream byte. The split that preserves it exactly:

- stage 1 (pure, parallel): header checks + palette parse + dense unpack +
  bounds + first-reference order (`section_glob_pre`)
- stage 2 (SERIAL, the only registry-touching part): palette refs resolved
  in first-reference order — the exact `get_or_insert` sequence of the old
  per-voxel memo loop, including the >65535 abort semantics and the
  per-member `len() <= 65536` gate (`section_glob_resolve`)
- stage 3 (pure, parallel): gid map + RLE admission scans + winner +
  header + tail (`section_glob_finish`)

`section_to_glob` remains as the composition (tests, serial callers);
`monolithic == composition` on every glob unit test. Also on the path:
the two gid-RLE admission scans were full 4096-byte walks — they now
early-exit once the admission bound is busted (the v0.9.21b lever, ported
from the encode path; decisions identical by construction) and are skipped
entirely at `CSAVES_GID_RLE_DIV=0`; the bounds pass and the first-reference
scan fused into one voxel pass; the per-section getenv reads
(`CSAVES_GID_RLE_DIV`, `CSAVES_GLOB_DEBUG` x2, `CSAVES_FORCE_GLOB` per
group) resolved once.

Cold level 6 -> 4. The 0.9.29 verdict ("z4 buys storage, not speed") was
true only while build was serial: build no longer hides comp, and the
re-sweep shows z2 pack-flat (0.29 s) while z4 keeps -32% pack vs z6 for
+10% container (compact x0.10 -> x0.13, gate 0.15 still green).

Dim corpus (2-core, warmup-first, same sandbox): pack 0.48 -> 0.24 s
(-50%), build 152 -> 95 ms, comp 207 -> 57 ms (z4). Conversion bytes are
IDENTICAL to the serial flow by construction (container x5.81 = the z4
sweep's own bytes); the container fp moves with the level change only.

Gates: 58/58 tests, enc_fp EXACT (6d240d7166cdda99 / 12130 / 9971843 —
import untouched), verify 728+218 bad=0, survival ok=1280 bad=0, bench_size
compact x0.13 (gate 0.15), container 2.14 MiB (x5.81 vs sector-padded
anvil), pack 0.24 s.

## v0.9.32 — split-path hygiene: heightmap memo, once-resolved env toggles

The import split is now dominated by three measured blocks: section encode
(~105 µs/chunk), the shift pass (~48-51 µs), and the scan-root walk
(~48 µs, of which ~40 µs is heightmap work: every Heightmaps LongArray is
9-bit-unpacked, roundtrip-pack-validated and zigzag-varint re-encoded on
EVERY chunk import — 4.09 maps per chunk on the corpus). Heightmaps repeat
massively across chunks (flat regions, ocean floors, shared surface
configs), so the dense form is now memoized: key = xxh3_128 of the raw 296
bytes, value = the dense TAG_Byte_Array payload, thread-local (zero lock
traffic), 32k entries with clear-on-full. It is a pure-function cache —
identical input bytes must yield identical dense bytes — so the wire is
untouched: enc_fp 6d240d7166cdda99 / 12130 / 9971843 EXACT.

The per-chunk feature reads (CSAVES_KEEP_LIGHT / NO_SCAN / NO_SHIFT /
NO_LITE) sat as getenv calls on the hot split path (4-5 per chunk on every
import thread); they are process-stable toggles and now resolve once into
statics.

Measured on the dim corpus (stable per-call stage timers, warmup-first):
scan stage 232.8 -> 222.8 µs/chunk; wall-clock import on this 2-core
sandbox is too noisy for honest A/B (MT swung 139-177 MB/s across
identical-code runs). Two documented negatives from the same session: a
byte-pair nibble unpack rewrite REGRESSED 2572 -> 3228 ns/section (the
constant-shift loop was already auto-vectorized — reverted), and the
long-standing split-minus-scan stage delta (~58-68 µs/chunk) turns out to
be nested Instant::now() overhead on the virtualized host (the outer timer
absorbs ~70 inner timer sites), not work — future profiling should trust
per-call stage timers over stage deltas here. Next measured levers, in
order: shift-pass internals (48-51 µs), hot-slab zstd inside
ensure_blobs (~60 µs), unpack SWAR histogram.

Gates: 58/58 tests, enc_fp EXACT, verify 728+218 bad=0 (x3 checkpoints),
bench_size compact x0.11 (gate 0.15), survival TOTAL x3.13 ok=1280 bad=0,
container 1.94 MiB x6.42, pack 0.47 s.

## v0.9.30 — dictionary training stops being a lottery

The sample pool behind the shared zstd dictionary was a Vec that froze at
the first 2×dict_train arrivals. Which payloads arrive first depends on
fs::read_dir order (bench imports) or player spawn direction (production),
so dictionary quality was a coin flip: the same JJ corpus trained a
degenerate 1662-7773 B dictionary on this sandbox and a healthy ~100 KB one
on the previous one. The offline study (examples/dict_probe.rs,
dict_sel.rs) pinned the physics: the COVER trainer's output swings 4.6-53 KB
across 200-sample subsets of the SAME 982-sample stream, while the full
stream in content-hash order is stable — order matters and subsets gamble.

v0.9.30 replaces the pool with a content-hash bucket reservoir: identical
payloads collapse into one bucket (uniform sections cannot flood it, tiny
RLE forms are admitted like any other raw), each bucket keeps its LARGEST
member (arrival-independent), and training consumes the hash-ordered prefix
(32 samples, bounded so the COVER cost stays at the old ~35 ms on the pack
wall). Retraining fires only on fresh coverage (≥ dict_train/4) or +25%
bucket growth — no version churn. Regression test
dict_training_is_arrival_order_independent pins byte-identical dictionaries
across arrival rotations. The COLD tier is now dict-less, matching the pack
path (bulk/mt compress never took the dictionary); the shared dictionary
earns its keep on hot-tier small writes. Blobs carry their own dict_id, so
dict-framed cold blobs from older stores decode exactly as stored — wire
compatible both ways.

Size-gate honesty: this work is size-NEUTRAL on every gate (jj TOTAL,
survival TOTAL, bench_size compact all unchanged vs 0.9.29 same-env runs);
the cross-sandbox deltas everyone chases (jj 1.61 ↔ 1.83 MiB, bench_size
compact 3.58 ↔ 3.80 MiB) reproduce on UNCHANGED code and are environment
drift, not dict-size effects — proven by A/B with git stash today. The big
slab frames compress dict-less in the pack path; wiring the dictionary into
pack Phase-B variant streams is the real (parked) size lever. Gates:
58/58 tests (+1 new), jj verify 946/946, survival ok=1280, bench_size
compact x0.11 (gate 0.15), enc_fp EXACT, pack 0.45 s = 0.9.29 parity,
container x6.42.

## v0.9.29 — cold tier z12 → z6: the memo moved the knee

The palette memo (v0.9.28) cut the pack BUILD phase 4x, so compression is
no longer hidden behind variant assembly — the cold-level sweep had to be
re-run. Pack wall on the JJ corpus (2-core, warmup-first, MT off): z12
0.79 s (comp 538 ms, container 1.88 MiB x6.61) / z8 0.56 s (297 ms, 1.89
x6.59) / **z6 0.45 s (213 ms, 1.94 MiB x6.42)** / z4 0.29 s (60 ms, 2.15
x5.81). Default moves to z6: **-43% pack for +3% container**, and below
z6 the wall is the BUILD phase again (z4: comp 60 ms vs build 158 ms), so
z4's extra -16% pack buys +11% storage — wrong trade for a storage
engine. Every size gate byte-flat at z6: bench_size compact x0.10 (gate
0.15), jj TOTAL unchanged, survival x3.12 vs x3.13 at z12 (level-flat on
this env; the historical x4.01 spread across sandboxes is the documented
dict-training env anomaly, not a level cost), verify 946/946 + 1280
ok=0 bad, enc_fp EXACT, 57/57 tests. CSAVES_COLD_LEVEL=19 opt-out
unchanged.

CI verdict for v0.9.28 (a9467f5) received before this push — the memo
delivered on the 2-core runner: pack 1.43 → 0.53 s (-63%), build phase
803 → 97.6 ms (8.2x), import 174.7 MB/s ST / 335.9 MB/s MT (CI best),
warm read 1983.9 MB/s (+56%), cold read 794.9 MB/s, container x6.61
flat, compact x0.10.

## v0.9.27 — cold tier z19 → z12: pack 6.8x; auto-MT gated to 4+ cores

Two verdicts from the same CI run (0.9.26, 42022752248, green 7/7):

**Pack verdict NEGATIVE for intra-frame MT on 2-core hosts**: pack 7.45 →
7.87 s (+5.6%). The private-repo CI runner is 2-core, and there the pack
is WORK-BOUND (wall ≈ work/cores: comp 6.63 s ≈ 13 core-s of z19 over
2 cores) — no parallelism can shorten it, intra-frame MT only added job
overhead + scheduling contention. Fix: the auto-MT budget engages only at
cores ≥ 4 (where work/4 ≈ 3.3-3.7 s < the 4.86 s pole → pole-bound → MT
is the right division); 2-core hosts run the pure ST pool (0.9.25
behavior); explicit CSAVES_PACK_MT_WORKERS overrides on any host.

**The real big pack lever is less WORK, not more cores** — cold-level
sweep on the JJ corpus (MT off, warmup-first, same machine):

| cold | pack | comp | container | vs anvil |
|------|------|------|-----------|----------|
| 19   | 8.39 s | 7.62 s | 1.66 MiB | x7.51 |
| 15   | 2.53 s | 1.77 s | 1.86 MiB | x6.70 |
| 12   | **1.23 s** | 0.52 s | 1.88 MiB | x6.62 |

Default cold_level 19 → **12**: pack wall **6.8x smaller** (8.39 →
1.23 s local 2-core; comp phase 7.62 → 0.52 s — below 12 the wall is the
BUILD phase: variant streams + GLOB conversion, not compression). The
container pays +13% on the noisiest corpus only; the size gates did not
move: survival x4.01 identical, bench_size compact x0.08 identical (gate
0.15), jj x3.36 at +0.6% (1.60 → 1.61 MiB). Pack is the background drain
rate competing with tick threads for cores — 6.8x less maintenance CPU
is the point (the 2 GB hot-tier incident class). CSAVES_COLD_LEVEL=19
stays for the size-obsessed.

Gates: 57/57 tests, enc_fp 6d240d7166cdda99/12130/9971843 EXACT, jj
verify 946/946 TOTAL x3.36, survival x4.01 ok=1280, bench_size compact
x0.08, verify 728+218 bad=0, import 113.7 MB/s / warm read 1577.9 MB/s
(local, unchanged), container x6.62.

## v0.9.26 — zstd-MT inside the big pack streams: the pole divides

v0.9.25 parallelized pack ACROSS streams but left the wall = the longest
single stream (12.1 MB GLOB variant = 4.86 s of serial zstd-19 — no
per-stream pool can touch one stream). Phase B now compresses the
k = cores/2 LONGEST variant streams INTRA-FRAME (zstd nbWorkers = cores/k
each, total worker budget == cores, the rest of the bigs stay ST in the
pool; LPT logic — the poles are exactly what binds the makespan on 4+
cores). MT frames are standard zstd: they decode with the stock
single-threaded decoder, are deterministic per (input, level, worker
count), and dedup hashes stay content-based — every semantic gate is
unaffected; the drained container fp moves (job-boundary bytes differ
from the ST encoder), which is why the follow-up was parked in 0.9.25.
Tasks also run LONGEST-FIRST (with short tasks first the tail degenerates
into one long stream running alone). Physics note: on a work-saturated
2-core box the pack wall is total-work-bound (wall ≈ work/cores), so MT
is ~neutral there (measured 8.36 vs 8.39 s baseline — noise); the win
needs a pole-bound host (4+ cores: CI pack 7.45 s with a 6.63 s
comp wall dominated by the 4.86 s pole -> expected ~4-5 s). Explicit
override: CSAVES_PACK_MT_WORKERS (all bigs get that worker count — an
ops lever that may oversubscribe); CSAVES_PACK_MT_MIN (default 1 MiB, 0
= off) gates admission per stream; new pack-census fields mtjobs/mtns/
mtbytes report what went intra-frame. libzstd now builds with
ZSTD_MULTITHREAD (the ST paths — hot tier, recipe flush, small pack
streams — are byte-identical; enc_fp re-verified EXACT).

Gates: 57/57 tests (new pack_mt_deterministic_and_wire_compatible:
determinism + stock-decoder roundtrip + MT-drained invariants), enc_fp
6d240d7166cdda99/12130/9971843 EXACT, jj verify 946/946 TOTAL x3.36,
survival x4.01 ok=1280, bench_size compact x0.08, verify 728+218 bad=0,
container x7.53.

## v0.9.25 — parallel pack: slab compressions across all cores

The pack pass is the storage-conversion wall: zstd-19 compresses at
2.5-4 MB/s per core and was ~98% of the corpus bench's import+pack wall
(10.2 s pack vs 0.21 s import on 2 cores; 8.58 s on the 4-core CI runner).
`flush_slab_cands` now runs in three phases: (A) build every group's
variant streams serially IN GROUP ORDER — the GLOB conversion sequence is
byte-for-byte the serial one, so registry gids and stream bytes are
unchanged; (B) compress all variant streams through a scoped
work-stealing pool (`available_parallelism` threads, one zstd call per
task — deterministic per (input, level)); (C) append + blob-map updates
in the exact serial order with the exact serial decision logic. The
packed-slab segment (seg-00002, all container data) stays byte-identical;
the small tail segments carry a PRE-EXISTING run-to-run nondeterminism
(0.9.24 measured against itself: 1443173 vs 1443174 bytes) — not
introduced here. Local 2-core: pack 10.2 -> 8.0-8.2 s (-21%); the
per-task census (SPEED pack-census) shows real overlap (8.3 MB canon
3.32 s + 12.1 MB glob 4.86 s concurrent — 8.2 s serial). On 4-core CI
the same code is expected to cut the 8.58 s baseline by 40-50%. The
12.1 MB GLOB stream is the per-wave long pole; zstd-MT inside big
streams is the documented follow-up (output would differ from
single-threaded pack, so it is deliberately not taken now).

MT write hygiene from the first exclusive-lock census (new SPEED
mt-lock-census line): `id_of` hoisted to ONE `blobs.read()` per recipe
encode (was ~22 acquisitions/chunk; census: 10.8-12.3 ms of a 146 ms
2-thread import wall, 660 ns per contended acquisition); the supersede
block de-nested (parse_recipe_body's segment read + decompress and
drop_recipe_refs' blobs.write used to run INSIDE the recipe_batch
Mutex). MT recipe-stage inflation 20-30 -> 6.8 us/chunk; the MT wall
band itself is unchanged (148-181 MB/s on 2 cores) — split-stage
memory/cache effects dominate what locks no longer do.

Gates: 56/56 tests, enc_fp 6d240d7166cdda99/12130/9971843 EXACT, jj
verify 946/946 TOTAL x3.36, survival x4.01 ok=1280, bench_size compact
x0.08 (gate <= 0.15), verify 728+218 bad=0.

## v0.9.24 — hot tier drops to zstd fast level -3: import +13%

The hot tier is a staging area: the background pack pass decompresses hot
slabs and re-compresses everything at the cold tier (level 19, 4-8 MiB
window), so the hot level only bounds transient disk usage and write-path
CPU. Level 3's own defense ("2.1x faster than 12 with identical packed
size") applies all the way down into the zstd fast levels. Back-to-back
corpus runs (same machine, warm): import 96.5 (L3) -> 109.1 (L-3) MB/s,
ensure_blobs 79.2 -> 50.1 us/chunk (-37%), import p50 75.6 -> 65.8 us; a
later same-session run measured 113.0 MB/s ST / 173.0 MB/s MT. Fast -5/-7
bottom the ensure stage out at the same ~50 us (zstd stops being the term)
without further import gains, so -3 is the best speed at the smallest
transient size. The DRAINED container is byte-identical (jj x3.36, 728-chunk
x7.51, survival x4.01), verify 946+1280+728+218 bad=0, enc_fp pin unchanged
(6d240d7166cdda99 / 12130 / 9971843), tests 56/56. Cumulative import ST:
37.5 (0.9.10) -> 109-113 MB/s (+190-201%). `CSAVES_HOT_LEVEL` still
overrides (default now -3).

## v0.9.22 — ensure_blobs single-pass slab insert

flush_hot_slab_group inserted every member with `slab_off: 0` and then
re-probed each of them with `blobs.get_mut` in a second map walk to patch
the offset in — ~130 hash lookups per chunk for values the insert loop
already had in hand (the cursor is tab_len plus the preceding member
lens, known at insert time). One pass now; ensure_blobs 79.3 ->
76.6 us/chunk, import ST 98-104 -> 103-104 MB/s on the warm corpus.
enc_fp pin unchanged (6d240d7166cdda99), tests 56/56, verify
728+218+1280 bad=0.

## v0.9.21 — bounded SIMD RLE admission: import +12-15%

The pack-stage budget breakdown (new per-width `packb` timers in
`split_stage_json`) killed the vanilla-width passthrough hypothesis —
bits 5-7 repack is only 5% of the dense-pack cost — and exposed the real
lever: the RLE admission scans. Computing the transposed-stream
transition count for every section (13714 calls per region pass) cost
17.7 ms of the 45.0 ms pack stage — more than the dense pack itself
(13.7 ms).

Three cuts, all byte-exact (pinned by enc_fp, differential-tested):

1. **Bounded scans.** cap = dense_len/8 exactly separates rejection
   (transitions >= cap => runs*8 > dense_len is guaranteed) from
   survival (transitions < cap => the exact count is already in hand),
   so the AVX2 walk aborts the moment the running count reaches cap.
   Noise-heavy sections (~90%) exit within the first row pairs instead
   of streaming all 15: b-scan 1168 -> 460 ns/call.
2. **Vectorized column boundaries.** the 255 transposed-stream boundary
   pairs are order-independent; regrouped by z they become 15
   elementwise compares of two contiguous 16-element windows (2 AVX2
   loads each) plus 15 scalar cross-x pairs — no strided scalar walk.
3. **UNI skip.** a uniform raw stream transposes to the same single
   value, so both RLE candidates are one run with identical framing:
   la == lb and the strict-< tie rule already picks (1) — the b-scan
   is skipped entirely (0 fires on the layered JJThunder corpus, free
   insurance for uniform-heavy survival worlds).

Results (same corpus, warm runs): import ST **87-91 -> 98-104 MB/s**
(median ~100.5, +12-15%), b-scan 17.7 -> 6.3 ms per region pass (-64%),
enc stage 94.4 -> 75.6 ms, pack stage 45.0 -> 30.1 ms. Gates: tests
56/56 (three new: AVX2-vs-naive transition counters, bounded-contract
sweep, old-vs-new decision equivalence over a threshold-sweeping
differential), enc_fp fp=6d240d7166cdda99 members=12130 bytes=9971843,
verify 728+218+1280 bad=0, bench_size x0.08, survival_bench ok=1280.

## v0.9.20 — one ids lock per slab group

flush_hot_slab_group acquired `ids.write()` per member (130 RwLock
round-trips per chunk on top of the single blobs guard); one guard now
covers the loop. Neutral on the 2-core sandbox (compute-bound), removes
real cross-thread lock traffic for multi-core import hosts.

## v0.9.19 — ensure_blobs: O(n) dedup, fingerprint hasher, reused zstd CCtx

ensure_blobs_batch spent real time on three invisible per-chunk costs:
(1) in-batch duplicate detection was a linear `misses.iter().any()`
scan — O(n²), ~8.4k tuple compares per chunk; (2) the blobs/ids maps
hashed already-fingerprint keys through SipHash (Hash is a 128-bit
content hash, ids sequential u32) — ~520 map ops per chunk; (3) every
slab frame built a fresh zstd CCtx. Now: a HashSet with a
fingerprint-grade bucket hasher (RawBuild — bucket hash needs
uniformity only, every probe full-key-compares), blobs/ids on the same
hasher, and a per-thread reusable `bulk::Compressor` cached by level
(byte-identical output; recipe_batch 8.7 -> 5.8 µs/chunk as a side
effect). ensure_blobs 87.0 -> 79.8 µs/chunk, import p50 76.5 ->
72.4 µs, import MT 120 -> 125.8 MB/s. enc_fp byte-exact, 53/53 tests,
container 1.66 MiB unchanged.

## v0.9.18 — dir_bytes header fix: direct parser covers 100% of the corpus, fb 66 -> 0

The v0.9.17 direct parser refused 66/728 chunks (9%) into the fastnbt
wrap-parse. The new `direct.whys` refusal histogram showed 14
unknown-field + 49 Y-context + 3 uncounted refusals; an offline NBT
replay of the first refusing payload traced the walk to `dir_bytes`,
which consumed `len` bytes but never ADDED the 4-byte ByteArray length
header — every SkyLight/BlockLight-bearing section drifted 4 bytes
left, the misread tail usually ends 0x00, the compound "ends" early,
and the cascade of garbage fields (tag 0x99, 39321-byte "names")
tripped the refusal. The fastnbt fallback silently papered over it —
outputs were identical (all parity gates passed), only the path was
slow. Fix: `end = p + 4 + len`. direct ok=728 fb=0 whys all zero,
scanp 35.3 -> 22.3 µs/chunk (-37%), import ST 81-84 -> **87-89 MB/s**
(p50 77.6 -> 76.5 µs), +132% over 0.9.10's 37.5. All pins unchanged
(enc_fp fp 6d240d7166cdda99, probe_lite 42218x0, verify
728+218+1280, bench_size x0.08, shift pins).

## v0.9.17 — direct sections parser (fastnbt off the hot path) + group-split pack + lazy dense: import +15%

Three levers aimed at the split stage's remaining bulk (scanp 65 µs/chunk
of fastnbt serde work, pack 5.3 µs/data-array of latency-bound bit
packing, and the unconditional dense pack that RLE-winning sections
threw away):

1. `parse_sections_direct` — the sections list now parses IN PLACE from
   the chunk payload with a hand-rolled walker (no wrap-buffer copy, no
   fastnbt serde machinery). Safety is ONE-SIDED: any shape doubt —
   unknown field, wrong tag, bad UTF-8, count/position mismatch — returns
   None and the chunk takes the exact v0.9.13 fastnbt wrap path, so every
   chunk follows the route it took before. fastnbt's coercion semantics
   are mirrored where proven (any NBT int tag feeds i8/i32 fields through
   serde's num visitors, Long-only-when-it-fits); a refusal-reason
   histogram (`direct.whys` in split_stage_json) tracks the ~9% of chunks
   that still take the wrap path — pure diagnostics, outputs identical.
   scanp 65.4 -> 34.6 µs/chunk, import p50 113.7 -> 76-78 µs.
2. Group-split packing (`pack_groups`): 64 values x b bits = exactly b
   longs, so the stream shards into independent 64-value groups; the old
   single accumulator carried ONE or->shift->or dependency chain across
   all 4096 voxels (~4.5 cycles/voxel), and even per-group state stayed
   chained across groups (the renamer cannot break an acc->acc data
   dependency through the reset), so TWO groups are packed in lockstep
   with fully independent state — 2x ILP on the pack chain. Byte-identical
   by construction (same values, same long positions), pinned by enc_fp.
3. Lazy dense materialization: the RLE admission rule only consumes
   dense.len(), which is known without packing ((entries*out_bits+7)/8
   for the packer, data.len()*8 for the identity memcpy) — when an RLE
   candidate wins (~10% of corpus data arrays) the dense pack is skipped
   entirely.

Results (3-repeat confirmed): import ST 70.6-72.6 -> **82.6-83.8 MB/s**
(+122% over 0.9.10), import MT 117.3-121.6 MB/s @ 2 threads, warm read
1257.5 (parity). All outputs byte-identical — enc_fp fp 6d240d7166cdda99
members 12130 bytes 9971843 unchanged, probe_lite 42218 sections 0
mismatches, 53/53 tests, verify 728/728 + 218/218 + 1280/1280, bench_size
x0.08 vs anvil compact, container 1.66 MiB (x7.51), match pins unchanged
(plane0=327, full=327, ksum=3041, cand=447213, dense_match=198).

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
