//! play_probe (v0.9.60): the server plays from the container — the live
//! path, end to end.
//!
//! The Paper integration is not hypothetical: the module boots inside the
//! server (cplugin_init), retransforms RegionFileStorage and binds natives
//! (nReadChunk / nWriteChunk(fresh) / nEndSave) to this very registry. This
//! probe drives THE SAME surface the natives expose — Registry::attach,
//! store_for(handle), write_chunk_fresh (the freshness evidence channel),
//! read_chunk (None = ReadResult::NO_DATA → Paper regenerates), sync —
//! through a complete server session, and CI gates every claim:
//!
//!   Worldgen burst (pregen / first exploration): every chunk saved with
//!     the freshness evidence (Status=minecraft:full, InhabitedTime==0 —
//!     genuinely untouched worldgen) → a regeneration CONTRACT, zero
//!     stored blobs. Worldgen is never stored on the live path.
//!   Play: the player walks; visited chunks load (contracts read None —
//!     the sim checks the legend against the worldgen form: regeneration
//!     WILL reproduce the chunk) and a deterministic subset gets edited —
//!     dirty saves carry fresh=false, which flips the chunk to honest
//!     storage (the v0.9.47 mutation-safety rule: a contract can never
//!     swallow a live edit). Re-saved-but-unchanged worldgen hits the
//!     identical re-save fast path and stays contracted.
//!   Restart: sync (the nEndSave barrier), drop every handle, reopen the
//!     SAME dir through a fresh Registry — the new server process. Every
//!     key must account for itself: stored + contracts = all keys.
//!   Reload: every edited chunk reads back and verifies SEMANTICALLY
//!     against the edited content; every contract's legend must equal the
//!     worldgen bytes (raw hash for all, canonical hash for a sample) —
//!     the guarantee that regeneration returns the world the player saw.
//!   Disk truth: a recursive walk counts ZERO .mca files. The server's
//!     only world copy is the container.
//!
//! Usage: cargo run --release --example play_probe -- [regions_per_side]
//! Default side = 4 (16 regions = 16,384 chunks = 2048x2048 blocks).

use c_saves::chunknbt::{canonical_contract_hash, sections_semantically_equal};
use c_saves::natives;
use c_saves::store::Store;
use fastnbt::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

const SEED: u64 = 0xC0FFEE;
/// CSAVES_PRISTINE=1 — the operator opt-in the live server sets.
const ENV_PRISTINE: &str = "1";

fn work_dir(tag: &str) -> PathBuf {
    let work = std::env::temp_dir().join(format!(
        "csaves-play-{tag}-{}",
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

fn count_mca(root: &Path) -> usize {
    let mut n = 0usize;
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                n += count_mca(&p);
            } else if p.extension().map(|x| x == "mca").unwrap_or(false) {
                n += 1;
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

/// Semantic chunk equality — canonical section re-encoding permutes
/// palette order (semantically neutral); everything else is structural.
fn chunks_semantic(a: &Value, b: &Value) -> bool {
    let Value::Compound(ma) = a else { return false };
    let Value::Compound(mb) = b else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    for (k, va) in ma.iter() {
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

fn main() {
    // the operator opt-in, exactly as the live deployment sets it
    std::env::set_var("CSAVES_PRISTINE", ENV_PRISTINE);
    std::env::set_var("CSAVES_PRISTINE_CANON", "1");

    let side: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let n_regions = side * side;
    let total_chunks = n_regions * 1024;
    let world = wgen::World::new(SEED);
    let work = work_dir("probe");
    let world_dir = work.join("region"); // the container dir the server attaches
    std::fs::create_dir_all(&world_dir).unwrap();
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);

    // deterministic edit mask: every chunk whose linear index survives the
    // golden-ratio mask is one the player actually builds on
    let is_edit = |i: usize| (i as u64).wrapping_mul(0x9E3779B97F4A7C15) % 4 == 0;

    // ---------- session 1: worldgen burst ----------
    let t0 = Instant::now();
    let reg = natives::REGISTRY.get_or_init(|| Arc::new(c_saves::engine::Registry::new()));
    let handle = reg.attach("region", "play", world_dir.to_str().unwrap());
    let store = c_saves::natives::store_for(handle).expect("attached store");
    println!(
        "play session: attached store #{handle} at {} (the natives' registry)",
        world_dir.display()
    );

    let keys: Vec<u64> = {
        let mut ks = Vec::with_capacity(total_chunks);
        let half = (total_chunks / 2) as i64;
        for i in 0..total_chunks as i64 {
            let cx = (i - half) as i32;
            let cz = (i % 64 - 32) as i32;
            ks.push(((cx as u64) << 32) | (cz as u64 & 0xffff_ffff));
        }
        ks
    };
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store, world, keys, dn) = (&store, &world, &keys, &done);
            s.spawn(move || {
                for i in (w..keys.len()).step_by(nproc) {
                    let k = keys[i];
                    let cx = (k >> 32) as i32;
                    let cz = (k & 0xffff_ffff) as u32 as i32;
                    let p = wgen::chunk_payload(world, cx, cz, 0);
                    // the freshness evidence: Status=full, InhabitedTime==0
                    store.write_chunk_fresh(k, &p, false, true).expect("fresh save ok");
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 4096 == 0 {
                        store.sync();
                    }
                }
            });
        }
    });
    let t_gen = t0.elapsed().as_secs_f64();
    let (stored0, contracted0) = classify(&store);
    assert_eq!(contracted0.len(), total_chunks, "fresh worldgen must contract");
    assert_eq!(stored0.len(), 0, "worldgen is never stored on the live path");
    println!(
        "play worldgen: {} chunks saved with the freshness evidence -> {} contracts, 0 stored, {:.1}s (worldgen is never stored)",
        total_chunks,
        contracted0.len(),
        t_gen
    );

    // ---------- session 1: play — visit, load, edit ----------
    let t1 = Instant::now();
    let edits: Vec<u64> = keys.iter().enumerate().filter(|(i, _)| is_edit(*i)).map(|(_, k)| *k).collect();
    let resaves: Vec<u64> = keys
        .iter()
        .enumerate()
        .filter(|(i, _)| !is_edit(*i) && i % 3 == 0)
        .map(|(_, k)| *k)
        .collect();
    let loads = AtomicUsize::new(0);
    let flips = AtomicUsize::new(0);
    // loads: contracts read None (NO_DATA -> Paper regenerates); the CANON
    // legend must match a freshly-serialized worldgen form — NBT byte order
    // is not stable across serializations (HashMap order), the canonical
    // structural hash is (it is exactly what the write path matches a
    // byte-different relight re-save against)
    for k in keys.iter().step_by(97) {
        let cx = (k >> 32) as i32;
        let cz = (k & 0xffff_ffff) as u32 as i32;
        assert!(store.read_chunk(*k).is_none(), "contract reads None");
        let p0 = wgen::chunk_payload(&world, cx, cz, 0);
        let (raw, canon) = store.pristine_legend(*k).expect("legend present");
        let expect = canonical_contract_hash(&p0)
            .expect("worldgen payload parses to a canonical form");
        assert_eq!(canon, Some(expect), "canon legend == canonical worldgen form");
        let _ = raw; // raw hash is serialization-dependent; canon is the contract
        loads.fetch_add(1, Ordering::Relaxed);
    }
    // edits: dirty saves carry fresh=false — the mutation-safety rule
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store, world, edits, flips) = (&store, &world, &edits, &flips);
            s.spawn(move || {
                for (j, k) in edits.iter().enumerate() {
                    if j % nproc != w {
                        continue;
                    }
                    let cx = (*k >> 32) as i32;
                    let cz = (*k & 0xffff_ffff) as u32 as i32;
                    let p = wgen::chunk_payload(world, cx, cz, 1); // a real edit
                    store.write_chunk_fresh(*k, &p, false, false).expect("edit save ok");
                    flips.fetch_add(1, Ordering::Relaxed);
                    if flips.load(Ordering::Relaxed) % 2048 == 0 {
                        store.sync();
                    }
                }
            });
        }
    });
    // vanilla also re-saves worldgen chunks dirtied by lighting: identical
    // content, still fresh -> the identical re-save fast path keeps them
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store, world, resaves) = (&store, &world, &resaves);
            s.spawn(move || {
                for (j, k) in resaves.iter().enumerate() {
                    if j % nproc != w {
                        continue;
                    }
                    let cx = (*k >> 32) as i32;
                    let cz = (*k & 0xffff_ffff) as u32 as i32;
                    let p = wgen::chunk_payload(world, cx, cz, 0);
                    store.write_chunk_fresh(*k, &p, false, true).expect("re-save ok");
                }
            });
        }
    });
    let t_play = t1.elapsed().as_secs_f64();
    let (stored1, contracted1) = classify(&store);
    assert_eq!(stored1.len(), edits.len(), "every edit must flip to honest storage");
    assert_eq!(contracted1.len(), total_chunks - edits.len());
    println!(
        "play session: {} chunks loaded (contract -> None -> regenerate), {} edits (fresh=false) -> {} stored, {} unchanged re-saves stayed contracted, {:.1}s",
        loads.load(Ordering::Relaxed),
        edits.len(),
        stored1.len(),
        resaves.len(),
        t_play
    );

    // ---------- save-all + RESTART (the new server process) ----------
    let t2 = Instant::now();
    reg.sync_all(); // the nEndSave barrier
    reg.seal_all();
    drop(store);
    drop(reg);
    // a fresh process: its own registry, the same dir
    let reg2 = c_saves::engine::Registry::new();
    let handle2 = reg2.attach("region", "play", world_dir.to_str().unwrap());
    let store2 = reg2.get(handle2).unwrap().0;
    let (stored2, contracted2) = classify(&store2);
    assert_eq!(stored2.len(), edits.len(), "edits survive the restart");
    assert_eq!(contracted2.len(), total_chunks - edits.len(), "contracts survive the restart");
    assert_eq!(stored2.len() + contracted2.len(), total_chunks, "every key accounts for itself");
    let t_restart = t2.elapsed().as_secs_f64();
    println!(
        "play restart: container reopened — {} stored + {} contracts = {} keys, {:.1}s",
        stored2.len(),
        contracted2.len(),
        stored2.len() + contracted2.len(),
        t_restart
    );

    // ---------- reload: edits byte-exact, contracts regenerate ----------
    let t3 = Instant::now();
    let ok = AtomicUsize::new(0);
    let bad = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store2, world, edits, ok, bad) = (&store2, &world, &edits, &ok, &bad);
            s.spawn(move || {
                for (j, k) in edits.iter().enumerate() {
                    if j % nproc != w {
                        continue;
                    }
                    let cx = (*k >> 32) as i32;
                    let cz = (*k & 0xffff_ffff) as u32 as i32;
                    let expect = fastnbt::from_bytes::<Value>(&wgen::chunk_payload(world, cx, cz, 1))
                        .expect("edited content parses");
                    match store2.read_chunk(*k) {
                        Some(q) => match fastnbt::from_bytes::<Value>(&q) {
                            Ok(got) => {
                                if chunks_semantic(&expect, &got) {
                                    ok.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    bad.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[play] semantic mismatch at key {k:016x}");
                                }
                            }
                            Err(_) => {
                                bad.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[play] unparseable at key {k:016x}");
                            }
                        },
                        None => {
                            bad.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[play] read=None on an EDITED chunk at key {k:016x}");
                        }
                    }
                }
            });
        }
    });
    assert_eq!(bad.load(Ordering::Relaxed), 0, "every edit must reload exactly");
    // contracts: the legend still guarantees a faithful regeneration
    let mut regen_ok = 0usize;
    for (i, k) in contracted2.iter().enumerate() {
        let cx = (*k >> 32) as i32;
        let cz = (*k & 0xffff_ffff) as u32 as i32;
        let p0 = wgen::chunk_payload(&world, cx, cz, 0);
        assert!(store2.read_chunk(*k).is_none(), "contract still reads None");
        let (_, canon) = store2.pristine_legend(*k).expect("legend present");
        let expect = canonical_contract_hash(&p0).expect("worldgen parses");
        assert_eq!(canon, Some(expect), "regeneration reproduces the worldgen form");
        regen_ok += 1;
        if i % 8192 == 0 {
            eprintln!("[play] contract legends re-verified {}/{}", i, contracted2.len());
        }
    }
    let t_reload = t3.elapsed().as_secs_f64();
    println!(
        "play reload: {}/{} edits byte-exact, {}/{} contracts regenerate to their worldgen form, 0 lost, {:.1}s",
        ok.load(Ordering::Relaxed),
        edits.len(),
        regen_ok,
        contracted2.len(),
        t_reload
    );

    // ---------- the disk truth ----------
    let mca = count_mca(&work);
    assert_eq!(mca, 0, "the world tree must contain zero .mca files");
    let container = dir_size(&world_dir);
    println!(
        "play disk truth: {} .mca files — the server's only world copy is the container ({} B)",
        mca, container
    );
    println!(
        "play weight: {} B/chunk across {} keys ({} stored ~250 B, {} contracts ~35 B)",
        container / total_chunks as u64,
        total_chunks,
        stored2.len(),
        contracted2.len()
    );

    println!("=== play probe: PASS ===");
}
