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
