//! pristine_import_probe: the huge-world lever measured on a REAL corpus.
//!
//! Imports a real Anvil dimension directory (region/*.mca) through the
//! production import path (`anvil::import_dir` -> `write_chunk_import`) into
//! two fresh stores:
//!   A) pristine OFF (0.9.44 default import semantics: everything stored)
//!   B) pristine + pristine_import ON (v0.9.45: the first-ever import write
//!      of a region-store key becomes a 17 B regeneration contract)
//! and compares container bytes vs the Anvil bytes on disk.
//!
//! Honest semantics: run B asserts the imported world is pure worldgen under
//! the seed it will be restored with (operator decision, opt-in). Pristine
//! contracts read None BY DESIGN; the probe counts them via is_pristine over
//! the full key set and verifies that non-pristine keys still read back.
//!
//! Usage: cargo run --release --example pristine_import_probe -- <dim-dir>

use c_saves::anvil;
use c_saves::store::{Cfg, Store};
use std::path::PathBuf;
use std::time::Instant;

fn dir_bytes(p: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_file() {
                    total += md.len();
                } else if md.is_dir() {
                    total += dir_bytes(&e.path());
                }
            }
        }
    }
    total
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(e.file_name());
        if e.metadata().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}

fn run_import(label: &str, dim: &PathBuf, pristine_import: bool) -> (usize, usize, u64, u64, f64) {
    let tag = if pristine_import { "import-contracts" } else { "pristine-off" };
    let work = std::env::temp_dir().join(format!(
        "csaves-pip-{}-{}",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let store_dir = work.join("region");
    let base = Cfg::from_env();
    let cfg = if pristine_import {
        Cfg { pristine: true, pristine_import: true, ..base }
    } else {
        base
    };
    let store = Store::open_cfg(&store_dir, true, cfg).unwrap();
    // import_dir DRAINS its source (deletes clean .mca files — production
    // semantics), so the probe always imports from a disposable copy
    let regions = work.join("staging-region");
    copy_dir(&dim.join("region"), &regions);
    let t0 = Instant::now();
    let rep = anvil::import_dir(&regions, &store).expect("import ok");
    let import_secs = t0.elapsed().as_secs_f64();
    store.sync();
    let fresh = store.disk_bytes();

    let mut pristine_n = 0usize;
    let mut stored_n = 0usize;
    let mut stored_missing = 0usize;
    for k in store.keys() {
        if store.is_pristine(k) {
            pristine_n += 1;
        } else if store.read_chunk(k).is_some() {
            stored_n += 1;
        } else {
            stored_missing += 1;
        }
    }
    assert_eq!(
        pristine_n + stored_n,
        store.chunk_count(),
        "[{label}] every imported chunk is either a contract or readable storage"
    );
    assert_eq!(stored_missing, 0, "[{label}] no unreadable stored chunks");

    // drain to the compact state (same machinery as bench_size phase C)
    let mut steps = 0;
    while store.compact_step() {
        steps += 1;
        if steps >= 500 {
            break;
        }
    }
    let compact = store.disk_bytes();

    println!(
        "[{label}] {tag}: chunks={} skipped={} failed={} import={:.1}s fresh={:.2}MiB compact={:.2}MiB ({} pack steps) pristine={} stored={}",
        rep.chunks,
        rep.skipped_present,
        rep.failed,
        import_secs,
        fresh as f64 / 1048576.0,
        compact as f64 / 1048576.0,
        steps,
        pristine_n,
        stored_n
    );
    let _ = std::fs::remove_dir_all(&work);
    (rep.chunks, steps, fresh, compact, import_secs)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));
    let anvil_region = dir_bytes(&dim.join("region"));
    println!("anvil region on disk: {:.2} MiB", anvil_region as f64 / 1048576.0);

    let (chunks_a, _, fresh_a, compact_a, secs_a) = run_import("A", &dim, false);
    let (chunks_b, _, fresh_b, compact_b, secs_b) = run_import("B", &dim, true);
    assert_eq!(chunks_a, chunks_b, "both runs import the same chunk set");

    println!("=== summary (region store, {} chunks) ===", chunks_a);
    println!(
        "anvil region         {:>10.2} MiB",
        anvil_region as f64 / 1048576.0
    );
    println!(
        "A pristine-off       {:>10.2} MiB compact (x{:.2} vs anvil) import {:.1}s",
        compact_a as f64 / 1048576.0,
        compact_a as f64 / anvil_region.max(1) as f64,
        secs_a
    );
    println!(
        "B import-contracts   {:>10.2} MiB compact (x{:.2} vs anvil, {:.3}x of A) import {:.1}s",
        compact_b as f64 / 1048576.0,
        compact_b as f64 / anvil_region.max(1) as f64,
        compact_b as f64 / compact_a.max(1) as f64,
        secs_b
    );
}
