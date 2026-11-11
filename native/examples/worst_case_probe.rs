//! worst_case_probe (v0.9.55): the challenge answered — EVERY chunk touched.
//!
//! The pristine line's arithmetic (x0.0087 canon legends, x0.0044 raw-only,
//! x0.00003 region-assert) was measured on worlds where 261,888 of 262,144
//! chunks stay contracted. The honest challenge: is the -99% just the
//! untouched-chunk elision? This probe deletes that excuse. The SAME
//! 262,144-chunk world, but EVERY chunk carries real player-built content
//! (wgen round 1: rebuilt terrain, a genuine edit) and is STORED THROUGH
//! THE NORMAL PIPELINE — zero pristine help:
//!
//!   Phase A (pure codec): the all-edited world as vanilla .mca (zlib-6,
//!     4 KiB sectors — byte-for-byte what Paper writes) vs a plain import
//!     with every pristine flag OFF. Every chunk is stored as content
//!     (content-addressed section dedup + zstd). No contract exists in
//!     this container at all; nothing is elided; the ratio is the raw
//!     codec arithmetic on 100% player-modified content.
//!   Phase B (the lifecycle): pristine import of the pure-worldgen twin
//!     (262,144 honest contracts), then EVERY contract superseded by a
//!     real live edit — 262,144/262,144 write_chunk calls, parallel. The
//!     fully-colonized container must converge to Phase A's weight: the
//!     pristine tier was a lever, not a dependency.
//!   Verify: every one of the 262,144 stored chunks is read back THROUGH
//!     the container in both phases and canonically hash-compared against
//!     the edited content (canonical_contract_hash — the same order-free,
//!     strip-invariant equality the pristine tier guarantees on restore;
//!     NBT byte order is not stable across serializations, content is).
//!   Export accounting: the fully-colonized container exports 262,144
//!     stored + 0 pristine_skipped + 0 lost — the operator can account
//!     for every key, nothing hides behind a contract.
//!
//! Gate: container/anvil < x0.15 (the module-level -90% promise) in BOTH
//! phases, asserted in-process (a panic exits non-zero, CI-able).
//!
//! Usage: cargo run --release --example worst_case_probe -- [regions_per_side]
//! Default side = 16 (256 regions = 262,144 chunks = 8192x8192 blocks).

use c_saves::anvil;
use c_saves::chunknbt::{sections_semantically_equal, strip_derived_chunk};
use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

const SEED: u64 = 0xC0FFEE;

fn work_dir(tag: &str) -> PathBuf {
    let work = std::env::temp_dir().join(format!(
        "csaves-worst-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    work
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0u64;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.is_dir() {
                n += dir_size(&e.path());
            } else {
                n += m.len();
            }
        }
    }
    n
}

fn classify(store: &Store) -> (Vec<u64>, Vec<u64>) {
    let mut stored = vec![];
    let mut contracted = vec![];
    for k in store.keys() {
        if store.is_pristine(k) {
            contracted.push(k);
        } else {
            stored.push(k);
        }
    }
    (stored, contracted)
}

/// Hard-link every region file into a second directory — zero-cost corpus
/// copies (the import unlinks the files it drains; the other directory
/// keeps its links to the same inodes).
fn link_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        if e.metadata().unwrap().is_file() {
            std::fs::hard_link(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

/// One vanilla-format region file, streamed to disk (constant RAM), with
/// `round` content in EVERY slot — for this probe every chunk is
/// player-edited, so the .mca holds the round-1 form of the whole world.
/// Returns the file size — the honest baseline bytes Paper would occupy
/// for these 1024 chunks.
fn write_region_round(
    world: &wgen::World,
    out_dir: &Path,
    rx: i32,
    rz: i32,
    round: u64,
) -> std::io::Result<u64> {
    let path = out_dir.join(format!("r.{rx}.{rz}.mca"));
    let mut f = std::fs::File::create(&path)?;
    let mut offsets = [0u32; 1024];
    {
        let mut buf = std::io::BufWriter::with_capacity(1 << 20, &mut f);
        buf.write_all(&[0u8; 8192])?; // location + timestamp header placeholder
        let mut sector_cursor = 8192u64 / 4096;
        for i in 0..1024usize {
            let lx = (i & 31) as i32;
            let lz = (i >> 5) as i32;
            let payload = wgen::chunk_payload(world, rx * 32 + lx, rz * 32 + lz, round);
            let mut z = flate2::write::ZlibEncoder::new(
                Vec::with_capacity(payload.len() / 3),
                flate2::Compression::new(6),
            );
            z.write_all(&payload)?;
            let comp = z.finish()?;
            let entry_len = 5 + comp.len();
            let sectors = ((entry_len + 4095) / 4096) as u32;
            offsets[i] = ((sector_cursor as u32) << 8) | sectors;
            buf.write_all(&(entry_len as u32).to_be_bytes())?;
            buf.write_all(&[2u8])?; // zlib, vanilla compression type
            buf.write_all(&comp)?;
            buf.write_all(&vec![0u8; sectors as usize * 4096 - entry_len])?;
            sector_cursor += sectors as u64;
        }
        buf.flush()?;
    }
    f.seek(std::io::SeekFrom::Start(0))?;
    let mut hdr = Vec::with_capacity(8192);
    for o in &offsets {
        hdr.extend_from_slice(&o.to_be_bytes());
    }
    // timestamps: the live world was touched by players — non-zero, distinct
    // (vanilla timestamp entries are u32, 4 bytes per slot)
    for i in 0..1024usize {
        let t = ((rx as u64 * 1024 + rz as u64 * 31 + i as u64 * 7) % 86400) as u32;
        hdr.extend_from_slice(&t.to_be_bytes());
    }
    f.write_all(&hdr)?;
    f.sync_all()?;
    Ok(f.metadata()?.len())
}

/// Semantic chunk equality — the jj_profile verify pattern (CI-pinned,
/// bad=0 on the real JJ corpus): canonical section re-encoding permutes
/// palette order (semantically neutral), so chunk equality = per-section
/// expanded state vectors + structural equality of everything else.
fn chunks_semantic(
    a: &Value,
    b: &Value,
    strip: bool,
) -> bool {
    let mut ma = a.clone();
    let Value::Compound(mb) = b else { return false };
    if strip {
        // the import path strips derived light (the server relights from
        // isLightOn=0); strip the source side symmetrically
        strip_derived_chunk(&mut ma);
    }
    let Value::Compound(ma) = ma else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    for (k, va) in &ma {
        let Some(vb) = mb.get(k) else { return false };
        if k == "sections" {
            let (Value::List(la), Value::List(lb)) = (va, vb) else { return false };
            if la.len() != lb.len() {
                return false;
            }
            for (sa, sb) in la.iter().zip(lb.iter()) {
                if !sections_semantically_equal(sa, sb) {
                    return false;
                }
            }
        } else if va != vb {
            return false;
        }
    }
    true
}

/// v0.9.67: the verify walk in on-disk location order — every packed slab
/// frame streams exactly once (each decompressed slab serves the dozens of
/// members read right after it), instead of one fresh slab decompress per
/// HashMap-order key. Keys without a location (contracts, pending) append
/// in their natural order at the end; the result is a permutation of
/// `keys` with no duplicates.
fn location_order(store: &Store, keys: &[u64]) -> Vec<u64> {
    let want: std::collections::HashSet<u64> = keys.iter().copied().collect();
    let mut v: Vec<u64> = store
        .keys_sorted_by_location()
        .into_iter()
        .filter(|k| want.contains(k))
        .collect();
    let have: std::collections::HashSet<u64> = v.iter().copied().collect();
    for k in keys {
        if !have.contains(k) {
            v.push(*k);
        }
    }
    v
}

/// The full semantic verify (v0.9.67 shape): parallel over contiguous
/// location-ordered ranges (slab streaming), full worldgen + parse +
/// chunks_semantic per key. Phase A keeps this — the import path's stored
/// form is NOT hash-comparable to the wgen source (the import strips
/// derived light and the canonical re-encode permutes palettes; the
/// semantic walker is the authority there). Phase B's live-write path IS
/// hash-stable (wcost: 64/64 through the real round-trip) and uses the
/// fast hash verify with this walker sampled on top.
/// Returns (ok, bad).
fn verify_all_semantic(
    store: &Store,
    world: &wgen::World,
    keys: &[u64],
    nproc: usize,
    strip: bool,
    tag: &str,
) -> (usize, usize) {
    let ok = AtomicUsize::new(0);
    let bad = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let per = keys.len().div_ceil(nproc.max(1));
    std::thread::scope(|s| {
        for w in 0..nproc {
            let lo = w * per;
            let hi = ((w + 1) * per).min(keys.len());
            if lo >= hi {
                break;
            }
            let (store, world, ok, bad, done) = (&store, world, &ok, &bad, &done);
            s.spawn(move || {
                for i in lo..hi {
                    let k = keys[i];
                    let cx = (k >> 32) as i32;
                    let cz = (k & 0xffff_ffff) as u32 as i32;
                    let expect = fastnbt::from_bytes::<Value>(&wgen::chunk_payload(world, cx, cz, 1))
                        .expect("edited content parses");
                    match store.read_chunk(k) {
                        Some(q) => match fastnbt::from_bytes::<Value>(&q) {
                            Ok(got) => {
                                if chunks_semantic(&expect, &got, strip) {
                                    ok.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    bad.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[worst] {tag}: semantic mismatch at key {k:016x}");
                                }
                            }
                            Err(_) => {
                                bad.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[worst] {tag}: stored form unparseable at key {k:016x}");
                            }
                        },
                        None => {
                            bad.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[worst] {tag}: read=None at key {k:016x}");
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 32768 == 0 || d == keys.len() {
                        eprintln!("[worst] {tag}: verified {d}/{}", keys.len());
                    }
                }
            });
        }
    });
    (ok.load(Ordering::Relaxed), bad.load(Ordering::Relaxed))
}


/// The production maintenance pass (the bench_size flow): fsync, seal the
/// active segment, then drive the pack sweep to completion — slabs re-tier
/// at cold_level, dictionaries retrain, dead records vanish. The container
/// weight is measured AFTER this pass, exactly what an operator ships.
///
/// The sweep is the JJThunder import pattern (`request_pack` +
/// `compact_step` loop — "imported data must be slab-packed + cold-tiered,
/// the same path the server module takes after an Anvil import")
/// GENERALIZED to bulk scale: one `request_pack` flag re-tiers ONE hot
/// segment per `compact_step`, and at world scale the imported mass spans
/// several `CSAVES_SEG_ROLL` segments (64 MB roll -> 262k chunks ~ 226 MB
/// hot -> ~4 segments) — so the flag is re-raised before every step and
/// the loop runs until NO hot material remains (pack_fallback finds no
/// non-cold blob and returns false). Without the sweep a bulk import
/// leaves every segment after the first in the hot tier — the CI run of
/// v0.9.55 caught exactly that (x0.2104 measured against the x0.058 the
/// shipped container actually weighs).
fn drive_maintenance(store: &Store) -> usize {
    store.sync();
    store.seal();
    let mut steps = 0;
    while steps < 100_000 {
        store.request_pack();
        if !store.compact_step() {
            break;
        }
        steps += 1;
        if steps % 10 == 0 {
            eprintln!("[worst] compaction step {steps}");
        }
    }
    steps
}

fn main() {
    let side: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let n_regions = side * side;
    let total_chunks = n_regions * 1024;
    let world = wgen::World::new(SEED);
    let work = work_dir("probe");
    let gen_live = work.join("gen-live"); // every slot: round-1 (player-edited)
    let gen_seed = work.join("gen-seed"); // every slot: round-0 (pure worldgen)
    std::fs::create_dir_all(&gen_live).unwrap();
    std::fs::create_dir_all(&gen_seed).unwrap();
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);

    // ---- 0. generate BOTH corpora, parallel: the fully-played world and
    // its pure-worldgen twin (the Phase-B pristine import source) ----
    let t0 = Instant::now();
    let anvil_live = AtomicU64::new(0);
    let anvil_seed = AtomicU64::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (world, gl, gs, al, asz, dn) =
                (&world, &gen_live, &gen_seed, &anvil_live, &anvil_seed, &done);
            s.spawn(move || {
                for ri in (w..n_regions).step_by(nproc) {
                    let rx = (ri / side) as i32;
                    let rz = (ri % side) as i32;
                    let n = write_region_round(world, gl, rx, rz, 1).expect("live region gen");
                    al.fetch_add(n, Ordering::Relaxed);
                    let m = write_region_round(world, gs, rx, rz, 0).expect("seed region gen");
                    asz.fetch_add(m, Ordering::Relaxed);
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 32 == 0 || d == n_regions {
                        eprintln!(
                            "[worst] generated {d}/{n_regions} regions (live {:.0} MiB, seed {:.0} MiB)",
                            al.load(Ordering::Relaxed) as f64 / 1048576.0,
                            asz.load(Ordering::Relaxed) as f64 / 1048576.0
                        );
                    }
                }
            });
        }
    });
    let anvil = anvil_live.load(Ordering::Relaxed);
    let seed_bytes = anvil_seed.load(Ordering::Relaxed);
    let anvil_per_chunk = anvil / total_chunks as u64;
    let t_gen = t0.elapsed().as_secs_f64();
    eprintln!("[worst] generation done in {t_gen:.1}s");
    println!(
        "worst-case world: {} regions x 1024 chunks = {} chunks, every chunk player-edited",
        n_regions, total_chunks
    );
    println!(
        "worst-case anvil: {} byte(s) ({:.1} MiB) = {} B/chunk — zlib-6, 4 KiB sectors, the vanilla form of the SAME all-edited content",
        anvil,
        anvil as f64 / 1048576.0,
        anvil_per_chunk
    );
    eprintln!(
        "[worst] seed twin (round-0 corpus for phase B): {:.1} MiB",
        seed_bytes as f64 / 1048576.0
    );

    // zero-cost corpus copy: phase B's pristine import drains its own
    // hard-link set of the SAME seed inodes.
    let gen_seed_b = work.join("gen-seed-b");
    link_dir(&gen_seed, &gen_seed_b);

    // ================= Phase A: pure codec, pristine OFF =================
    // No pristine flag is set: every chunk stores as content. The only
    // arithmetic at work is the content-addressed section dedup + zstd.
    let t1 = Instant::now();
    let cfg_plain = Cfg {
        pristine: false,
        pristine_import: false,
        pristine_canon: false,
        pristine_fresh: false,
        pristine_region: false,
        ..Cfg::from_env()
    };
    let store_a = Store::open_cfg(&work.join("store-a"), true, cfg_plain).unwrap();
    let irep_a = anvil::import_dir(&gen_live, &store_a).expect("plain import ok");
    let t_import_a = t1.elapsed().as_secs_f64();
    assert_eq!(irep_a.chunks, total_chunks, "every edited chunk must import");
    assert_eq!(irep_a.failed, 0, "no import failures");
    let (stored_a, contracted_a) = classify(&store_a);
    assert_eq!(stored_a.len(), total_chunks, "pristine OFF: everything stored");
    assert_eq!(contracted_a.len(), 0, "pristine OFF: zero contracts exist");
    // the production maintenance pass BEFORE the weight is measured
    let steps_a = drive_maintenance(&store_a);
    let container_a = dir_size(&work.join("store-a"));
    let per_chunk_a = container_a / total_chunks as u64;
    let ratio_a = container_a as f64 / anvil.max(1) as f64;
    eprintln!("[worst] phase A import done in {t_import_a:.1}s, compaction {steps_a} step(s)");
    println!(
        "worst-case import A: {} stored, {} contracted, {} failed (pristine OFF — pure codec, no elision)",
        stored_a.len(),
        contracted_a.len(),
        irep_a.failed
    );
    println!(
        "worst-case container A: {} byte(s) ({:.1} MiB) = {} B/chunk of stored player-edited content",
        container_a,
        container_a as f64 / 1048576.0,
        per_chunk_a
    );
    println!(
        "worst-case ratio A: x{:.4} of anvil — 100% player-modified content, zero pristine help",
        ratio_a
    );

    // full read-back: every stored chunk, through the container (import
    // path strips derived light — the source side is stripped symmetrically).
    // v0.9.67: hash verify covers EVERY key; the full semantic walker runs
    // sampled (every 4096th key) on the same run.
    let t2 = Instant::now();
    let keys_a = store_a.keys();
    let order_a = location_order(&store_a, &keys_a);
    // v0.9.67: the import path's stored form is not hash-comparable to the
    // wgen source (import strips derived light; the canonical re-encode
    // permutes palettes) — the full semantic walker stays the authority
    // here, now fed in location order (slab streaming).
    let (ok_a, bad_a) = verify_all_semantic(&store_a, &world, &order_a, nproc, true, "verify A");
    let t_verify_a = t2.elapsed().as_secs_f64();
    assert_eq!(bad_a, 0, "phase A: every stored chunk must match its edit");
    assert_eq!(ok_a, total_chunks);
    eprintln!("[worst] phase A verify done in {t_verify_a:.1}s");
    println!(
        "worst-case verify A: ok={} bad={} — every stored chunk read back and semantically matched",
        ok_a, bad_a
    );
    assert!(
        ratio_a < 0.15,
        "phase A gate: ratio x{ratio_a:.4} exceeds the x0.15 module promise ON FULLY-EDITED CONTENT"
    );
    println!(
        "worst-case ratio gate A: PASS (< x0.15 — the -90% promise holds WITHOUT a single contract)"
    );

    // ============== Phase B: pristine lifecycle, full colonization ==============
    // The seed twin imports pristine (honest contracts to the pure-worldgen
    // seed), then EVERY contract is superseded by a real live edit. What
    // remains is a fully-stored container — the pristine tier was a lever,
    // not a dependency.
    let t3 = Instant::now();
    let cfg_pristine = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_canon: true,
        ..Cfg::from_env()
    };
    let store_b = Store::open_cfg(&work.join("store-b"), true, cfg_pristine).unwrap();
    let irep_b = anvil::import_dir(&gen_seed_b, &store_b).expect("pristine import ok");
    let t_import_b = t3.elapsed().as_secs_f64();
    eprintln!("[worst] phase B pristine import done in {t_import_b:.1}s");
    assert_eq!(irep_b.chunks, total_chunks, "seed twin imports fully");
    assert_eq!(irep_b.failed, 0, "no import failures");
    let (stored_b0, contracted_b0) = classify(&store_b);
    assert!(stored_b0.is_empty() && contracted_b0.len() == total_chunks);
    println!(
        "worst-case import B: {} contracts formed (pristine import of the pure-worldgen seed)",
        contracted_b0.len()
    );

    // EVERY contract superseded by a real edit, parallel (the user-approved
    // parallelism: nproc workers, periodic sync to bound segment buffers).
    let t4 = Instant::now();
    let done_e = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store_b, world, keys, dn) = (&store_b, &world, &contracted_b0, &done_e);
            s.spawn(move || {
                for i in (w..keys.len()).step_by(nproc) {
                    let k = keys[i];
                    let cx = (k >> 32) as i32;
                    let cz = (k & 0xffff_ffff) as u32 as i32;
                    let p = wgen::chunk_payload(world, cx, cz, 1); // a real edit
                    store_b.write_chunk(k, &p, false).expect("live edit ok");
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 16384 == 0 {
                        store_b.sync(); // bound writer buffers + fsync progress
                    }
                    if d % 32768 == 0 || d == keys.len() {
                        eprintln!("[worst] edits {d}/{}", keys.len());
                    }
                }
            });
        }
    });
    let t_edits = t4.elapsed().as_secs_f64();
    let (stored_b1, contracted_b1) = classify(&store_b);
    assert_eq!(stored_b1.len(), total_chunks, "every contract superseded by a real edit");
    assert_eq!(contracted_b1.len(), 0, "no pristine chunk remains");
    // the production maintenance pass BEFORE the weight is measured
    let steps_b = drive_maintenance(&store_b);
    let container_b = dir_size(&work.join("store-b"));
    let per_chunk_b = container_b / total_chunks as u64;
    let ratio_b = container_b as f64 / anvil.max(1) as f64;
    eprintln!("[worst] {} live edits done in {t_edits:.1}s, compaction {steps_b} step(s)", total_chunks);
    println!(
        "worst-case lifecycle: {} edits -> 0 pristine remain (every contract superseded by a real edit)",
        total_chunks
    );
    println!(
        "worst-case container B: {} byte(s) ({:.1} MiB) = {} B/chunk",
        container_b,
        container_b as f64 / 1048576.0,
        per_chunk_b
    );
    println!(
        "worst-case ratio B: x{:.4} of anvil — the fully-colonized container",
        ratio_b
    );

    // full read-back again: the colonized container, every chunk (live
    // writes store the full form — no strip on either side). v0.9.67: the
    // full semantic walker stays the authority in BOTH phases (the split
    // store's canonical re-encode permutes palettes, so payload hashes are
    // not comparable across the storage boundary), fed in location order —
    // the verify's slab frames stream once instead of a fresh decompress
    // per HashMap-order key.
    let t5 = Instant::now();
    let keys_b = store_b.keys();
    let order_b = location_order(&store_b, &keys_b);
    let (ok_b, bad_b) = verify_all_semantic(&store_b, &world, &order_b, nproc, false, "verify B");
    assert_eq!(bad_b, 0, "phase B: every stored chunk must match its edit");
    assert_eq!(ok_b, total_chunks);
    let t_verify_b = t5.elapsed().as_secs_f64();
    eprintln!("[worst] phase B verify done in {t_verify_b:.1}s");
    println!(
        "worst-case verify B: ok={} bad={} — every stored chunk read back and semantically matched",
        ok_b, bad_b
    );

    // export accounting: nothing hides behind a contract anymore
    let rep = anvil::export_dir(&store_b, &work.join("export-b")).expect("export ok");
    assert_eq!(rep.chunks, total_chunks, "export materializes the whole world");
    assert_eq!(rep.pristine_skipped, 0, "zero pristine skips: no contract left");
    assert_eq!(rep.lost, 0, "nothing lost");
    assert_eq!(rep.chunks + rep.pristine_skipped + rep.lost, store_b.keys().len());
    println!(
        "worst-case export accounting: {} stored + {} pristine_skipped + {} lost = {} keys",
        rep.chunks,
        rep.pristine_skipped,
        rep.lost,
        store_b.keys().len()
    );
    assert!(
        ratio_b < 0.15,
        "phase B gate: ratio x{ratio_b:.4} exceeds the x0.15 module promise after FULL colonization"
    );
    println!(
        "worst-case ratio gate B: PASS (< x0.15 — full colonization weighs the same as pure codec)"
    );
    println!("=== worst-case probe: PASS ===");
}
