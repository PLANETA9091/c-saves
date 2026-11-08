# Big-world benchmark: vanilla Anvil vs c-saves

The `bigworld` GitHub Actions workflow (manual dispatch) generates a full
**512x512-chunk (262,144 chunks) JJThunder To The Max world** — 16 parallel
tile jobs of 128x128 chunks each on 4-core runners, then a bench job that
merges the tiles, spawns a 20,000-entity persistence corpus over the spawn
area and runs the byte-exact comparison + semantic verification
(`CSAVES_VERIFY_SAMPLE=10`).

## Local result (2026-10-25, 2-core sandbox)

A partially pregenerated JJThunder world (seed 90919091, datapack v0.9.0,
Paper 26.2 build 134): **5,108 terrain chunks** of real mountain terrain
(95,434+... 599,984 sections total, 131-132 sections per chunk) plus a
freshly summoned **20,000-entity corpus** (1,862 entity chunks, radius 380
blocks, PersistenceRequired).

| component | Anvil (.mca) | c-saves container | ratio |
|---|---|---|---|
| region (terrain) | 33.62 MiB | 12.92 MiB | **x2.60** |
| entities (20k)   | 7.39 MiB  | 0.65 MiB   | **x11.43** |
| **TOTAL**        | **41.02 MiB** | **13.57 MiB** | **x3.02** |

Semantic verification: `ok=6970 bad=0`.

Entity compression benefits compound at scale: natural spawns repeat the
same species NBT byte-for-byte across thousands of chunks, and the 4 MiB
whole-mode slabs give zstd the whole corpus in one window — 20k entities
collapse to 0.65 MiB (x11.4).

## Reproducing locally

1. Set up a Paper 26.2 server (Java 25) with the datapack in
   `jjworld/datapacks/` (level-name `jjworld` — the pack overrides the
   overworld dimension, so the world lives under
   `jjworld/dimensions/minecraft/overworld/`).
2. Disable the watchdog, then grow the world with `forceload add` waves of
   16x16 chunks (256-chunk command cap); `save-all flush` + `stop` between
   sessions (Paper buffers region writes — only flush makes them visible).
   Mountain tiles run ~0.4-4 chunks/s on 2 cores; a 4-core CI runner is
   several times faster.
3. Spawn entities with `scripts/jj/entities20k.py` over the loaded spawn
   area (needs `forceload add -512 -512 511 511` first).
4. `cargo run --release --example jj_profile -- <dim-dir>`.

## CI notes

* Tile jobs are resume-safe: every boot skips complete 16x16 wave blocks
  (region-slot checks), so a re-run continues where the previous attempt
  stopped.
* Each tile uploads only the 16 region files it owns (tiles are aligned to
  4x4 region-file boundaries, so ownership is disjoint and the merge is a
  plain copy).
* The bench job boots the merged world to summon the entity corpus (entity
  data is part of both sides of the comparison), then verifies a 1-in-10
  chunk sample semantically (full verification of 34M sections would triple
  the job time; the corpus job verifies 100%).

## bigworld-survival (0.9.5): the vanilla-terrain plateau, measured

The `bigworld-survival` workflow (Paper 26.3 pure vanilla worldgen, seed
77007, 512x512 chunks = 261,572 terrain chunks + 19,992 persisted entities
+ 2,000 loot chests, imported through the production pipeline with the
derived-light strip) measured on 6b05aaa (GLOB2 div=8 default):

| side | region | entities | TOTAL |
|---|---|---|---|
| Anvil (.mca) | 2135.99 MiB | 7.27 MiB | 2143.26 MiB |
| c-saves | 970.08 MiB | 0.86 MiB | 970.94 MiB |
| ratio | x2.20 | **x8.47** | **x2.21** |

Integrity **261,572/261,572** keys readable (missing 0, heal_rounds 0);
semantic verify **ok=26,342 bad=0** (1-in-10 sample). Enc-mode census:
2.29M dense vs 0.25M RLE sections — survival terrain is noise-dense.

**Why GLOB did not move this world.** The raw gid stream is 4096 bytes;
a 4-bit canonical dense stream is 2048. For the glob slab to win, zstd
must recover the 2x width from cross-section alignment alone. JJThunder
mountain tiles deliver that (the registry won x2.16 there and pushed the
corpus x2.99 -> x3.08); vanilla survival sections are ore-noise-unique,
alignment doesn't materialize, and the per-slab natural choice correctly
keeps canonical. GID-RLE (div=8) has nothing to bite on: the runny
sections never reach the glob pass (canonical RLE beats the packed width
outright). x2.21 is the same number the pre-GLOB architecture measured —
the plateau is the terrain's entropy, not a codec gap.

Class attribution (raw NBT): block_states data 46.0%, light 32.5%
(stripped on import — the server relights from isLightOn=0), palettes
9.5% (dropped by the registry), biomes 2.8%, heightmaps 3.0%.

Where the 50x-class ratios DO come from: repeated structure (the earlier
JJThunder 262k-chunk container measured x19.88), identical entity NBT
(x8.47-x11.4 here and in the JJ world), and player-built duplication —
content-addressed dedup converts all of it into single blobs. Pure fresh
worldgen has no such redundancy to claim.

## pristine import contracts (0.9.45): the huge-world projection

The x2.21 plateau above is the no-elision number. With
`CSAVES_PRISTINE=import` the background drain contracts pure-worldgen
chunks at a measured 26 B/chunk all-in (9,216-recipe measurement, contract
body + record overhead; see §0.9.45 in jjthunder-bench.md and research §9):

- real JJThunder region import (728 chunks): x0.26 → **x0.002 vs anvil**
  (-99.8%), pristine=728, stored=0;
- arithmetic projection for this 512x512 world (labeled, not measured
  end-to-end): 261,572 chunks × 26 B ≈ 6.8 MB of contracts vs 2,136 MiB
  region Anvil → region ~x0.003; with the entity corpus (0.86 MiB stored)
  and the loot-chest spawn edits the total stays two orders of magnitude
  under Anvil — far past the -90% target.

Safety: the operator asserts the world is pure worldgen under the seed it
will be restored with; any hash-differing re-save after restore flips that
chunk to normal storage (visited chunks get real storage, the untouched
mass stays contracted).
