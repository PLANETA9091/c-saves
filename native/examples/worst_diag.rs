//! worst_diag (v0.9.56 CI forensic): the 3-of-262144 contracted survivors of
//! worst-case phase B — reproduce, dissect, repeat.
//!
//! The CI gate run measured 262,141 stored after 262,144 live writes: three
//! keys were still `is_pristine() == true` — a contract survived a write
//! whose content is canonically DIFFERENT from the legend (raw equality is
//! impossible: fastnbt randomizes compound byte order per instance; canon
//! equality is impossible: heights derive from round-specific seeds). The
//! skip paths are therefore excluded — the suspect is a write/flush/sync
//! race that let a legend outlive its supersede. This probe replays phase B
//! in three sandbox-sized windows and, for every survivor, prints full
//! forensics: read Some/None, legend raw/canon against round-0 AND round-1
//! hashes, column-by-column height diff, then repeats the SAME edit — a
//! contract that dies on the second write is a transient race survivor; one
//! that survives twice is something worse.
//!
//! Environment:
//!   DIAG_DIR=<dir>          fixed work dir, phases share it (required)
//!   DIAG_PHASE=1|2|3        1: pristine import; 2: live edits; 3: dissect
//!   DIAG_SIDE=16            regions per side (16 -> 262,144 chunks)
//!   DIAG_THREADS=4          writer threads (CI ran 4)
//!   DIAG_STEP=16384         sync() stride during edits (probe parity)
//!
//! Exit code: 0 = no survivors / forensics printed; 42 = survivors present.

use c_saves::chunknbt::{canonical_contract_hash, hash_of};
use c_saves::store::{Cfg, Store, ST_PRISTINE_CANON, ST_PRISTINE_UNFRESH, ST_WRITE_SKIP};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

const SEED: u64 = 0xC0FFEE;

fn key_of(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn env_usize(name: &str, dflt: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
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

fn counters() -> (u64, u64, u64) {
    (
        ST_WRITE_SKIP.load(Ordering::Relaxed),
        ST_PRISTINE_CANON.load(Ordering::Relaxed),
        ST_PRISTINE_UNFRESH.load(Ordering::Relaxed),
    )
}

fn cfg_pristine() -> Cfg {
    Cfg {
        pristine: true,
        pristine_import: true,
        pristine_canon: true,
        ..Cfg::from_env()
    }
}

fn main() {
    let side = env_usize("DIAG_SIDE", 16);
    let threads = env_usize("DIAG_THREADS", 4);
    let step = env_usize("DIAG_STEP", 16384);
    let phase = std::env::var("DIAG_PHASE").unwrap_or_else(|_| "1".to_string());
    let dir = std::env::var("DIAG_DIR").expect("DIAG_DIR=<work dir> required");
    let work = PathBuf::from(&dir);
    let cdim = (side * 32) as i32; // chunks per world edge
    let total = (cdim * cdim) as usize;
    let world = wgen::World::new(SEED);

    eprintln!("[diag] phase {phase} side {side} ({total} chunks) threads {threads} step {step}");

    if phase == "1" {
        // ---- pristine import: every slot gets a round-0 payload through
        // the operator-asserted import path (write_chunk_import — the same
        // path import_dir drives under .mca files; the legend hashes the
        // payload bytes either way). Fresh dir, sync + seal at the end.
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let store = Store::open_cfg(&work, true, cfg_pristine()).unwrap();
        let done = AtomicUsize::new(0);
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for w in 0..threads {
                let (store, world, done) = (&store, &world, &done);
                s.spawn(move || {
                    for i in (w..total).step_by(threads) {
                        let cx = (i as i32) / cdim;
                        let cz = (i as i32) % cdim;
                        let p = wgen::chunk_payload(world, cx, cz, 0);
                        store.write_chunk_import(key_of(cx, cz), &p).expect("import ok");
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % 32768 == 0 {
                            eprintln!("[diag] imported {d}/{total}");
                        }
                    }
                });
            }
        });
        let (stored, contracted) = classify(&store);
        eprintln!(
            "[diag] import done in {:.1}s: keys={} contracted={} stored={}",
            t0.elapsed().as_secs_f64(),
            store.keys().len(),
            contracted.len(),
            stored.len()
        );
        assert!(stored.is_empty() && contracted.len() == total, "import must contract everything");
        store.sync();
        store.seal();
        eprintln!("[diag] phase 1 sealed");
    } else if phase == "2" {
        // ---- live edits: reopen (pristine cfg identical — is_pristine
        // semantics depend on it), every key write_chunk(k, round-1, false),
        // parallel, periodic sync (probe parity). Seal at the end.
        let store = Store::open_cfg(&work, true, cfg_pristine()).unwrap();
        let (stored0, contracted0) = classify(&store);
        eprintln!(
            "[diag] reopen: keys={} stored={} contracted={}",
            store.keys().len(),
            stored0.len(),
            contracted0.len()
        );
        assert!(stored0.is_empty() && contracted0.len() == total, "reopen must see the seed");
        let skip0 = counters();
        let done = AtomicUsize::new(0);
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for w in 0..threads {
                let (store, world, done, step) = (&store, &world, &done, step);
                s.spawn(move || {
                    for i in (w..total).step_by(threads) {
                        let cx = (i as i32) / cdim;
                        let cz = (i as i32) % cdim;
                        let p = wgen::chunk_payload(world, cx, cz, 1); // the real edit
                        store.write_chunk(key_of(cx, cz), &p, false).expect("live edit ok");
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % step == 0 {
                            store.sync(); // bound writer buffers (probe parity)
                        }
                        if d % 32768 == 0 || d == total {
                            eprintln!("[diag] edits {d}/{total}");
                        }
                    }
                });
            }
        });
        let skip1 = counters();
        eprintln!(
            "[diag] edits done in {:.1}s | skip-branch: raw+hit={} canon={} unfresh={} (delta raw+hit {} canon {})",
            t0.elapsed().as_secs_f64(),
            skip1.0,
            skip1.1,
            skip1.2,
            skip1.0 - skip0.0,
            skip1.1 - skip0.1
        );
        store.sync();
        store.seal();
        eprintln!("[diag] phase 2 sealed");
    } else if phase == "4" {
        // ---- export stress: reopen the fully-colonized container and run
        // export_dir at full 262,144-chunk scale under the sandbox's 3 GB
        // RAM. The pre-0.9.58 export materialized every payload (~5 GB
        // resident) before writing a byte — the CI runner died (exit 143,
        // OOM) twice, both times ~2 min into this phase. The streaming
        // export holds one region's payload at a time (~20 MB). Prints the
        // report; bad counts are asserted by worst_case_probe in CI.
        let store = Store::open_cfg(&work, true, cfg_pristine()).unwrap();
        let keys = store.keys();
        let (stored, contracted) = classify(&store);
        eprintln!(
            "[diag] reopen: keys={} stored={} contracted={}",
            keys.len(),
            stored.len(),
            contracted.len()
        );
        let out = std::env::var("DIAG_EXPORT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| work.join("export-b"));
        let t0 = Instant::now();
        let rep = c_saves::anvil::export_dir(&store, &out).expect("export ok");
        eprintln!(
            "[diag] export done in {:.1}s: regions={} chunks={} bytes={} pristine_skipped={} lost={}",
            t0.elapsed().as_secs_f64(),
            rep.regions,
            rep.chunks,
            rep.bytes,
            rep.pristine_skipped,
            rep.lost
        );
        assert_eq!(rep.chunks, total, "export materializes the whole world");
        assert_eq!(rep.pristine_skipped, 0, "no pristine contracts remain");
        assert_eq!(rep.lost, 0, "nothing lost");
        eprintln!("[diag] phase 4 done");
    } else {
        // ---- dissect: reopen, classify, forensics on every survivor,
        // repeat the SAME edit for survivors, classify again.
        let store = Store::open_cfg(&work, true, cfg_pristine()).unwrap();
        let (stored1, contracted1) = classify(&store);
        eprintln!(
            "[diag] after edits: keys={} stored={} contracted={}",
            store.keys().len(),
            stored1.len(),
            contracted1.len()
        );
        if contracted1.is_empty() {
            eprintln!("[diag] NO survivors this run — race not reproduced");
            return;
        }
        for &k in &contracted1 {
            let cx = (k >> 32) as i32;
            let cz = (k & 0xffff_ffff) as u32 as i32;
            let rc = store.read_chunk(k);
            let legend = store.pristine_legend(k);
            let p0 = wgen::chunk_payload(&world, cx, cz, 0);
            let p1 = wgen::chunk_payload(&world, cx, cz, 1);
            let (h0, h1) = (hash_of(&p0), hash_of(&p1));
            let (c0, c1) = (
                canonical_contract_hash(&p0),
                canonical_contract_hash(&p1),
            );
            let (raw_eq1, raw_eq0) = (
                legend.map(|l| l.0 == h1).unwrap_or(false),
                legend.map(|l| l.0 == h0).unwrap_or(false),
            );
            let (canon_eq1, canon_eq0) = (
                legend.iter().zip(c1.iter()).any(|(l, c)| l.1 == Some(*c)),
                legend.iter().zip(c0.iter()).any(|(l, c)| l.1 == Some(*c)),
            );
            let diff_cols = (0..256usize)
                .filter(|&i| {
                    let (lx, lz) = (i & 15, i >> 4);
                    world.height(cx, cz, lx, lz, 0) != world.height(cx, cz, lx, lz, 1)
                })
                .count();
            eprintln!(
                "[diag] SURVIVOR k={k:016x} cx={cx} cz={cz} read_chunk={}? \
legend.raw==h(r1)? {raw_eq1} | legend.canon==c(r1)? {canon_eq1} | \
legend.raw==h(r0)? {raw_eq0} | legend.canon==c(r0)? {canon_eq0} | \
height-diff cols {diff_cols}/256",
                rc.is_some()
            );
        }
        // repeat the SAME edit for survivors: a transient race survivor dies
        // on the second write; a deterministic one survives again.
        for &k in &contracted1 {
            let cx = (k >> 32) as i32;
            let cz = (k & 0xffff_ffff) as u32 as i32;
            let p = wgen::chunk_payload(&world, cx, cz, 1);
            store.write_chunk(k, &p, false).expect("repeat edit ok");
        }
        let (stored2, contracted2) = classify(&store);
        eprintln!(
            "[diag] after repeat edits: stored={} contracted={} (survivors killed: {})",
            stored2.len(),
            contracted2.len(),
            contracted1.len() - contracted2.len()
        );
        store.sync();
        store.seal();
        if !contracted2.is_empty() {
            std::process::exit(42);
        }
    }
    eprintln!("[diag] done");
}
