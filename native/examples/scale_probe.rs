//! scale_probe (v0.9.51): the -90% claim measured AT WORLD SCALE.
//!
//! bench_size pins the codec ratio on a 728-chunk real corpus (x0.08 of
//! vanilla Anvil). The pristine line promises more: a HUGE pregenerated
//! world must weigh almost nothing. This probe builds a synthetic but
//! structurally realistic pregen world with the SAME Tectonic-like
//! generator the size benchmarks use (wgen) at 262,144 chunks (512x512
//! chunks = 256 region files = an 8192x8192-block world), then measures
//! the operator-visible weight end to end:
//!
//!   1. vanilla Anvil baseline: .mca files, zlib-6, 4 KiB sector layout —
//!      byte-for-byte what Paper would have written for the same world;
//!   2. pristine import (operator-asserted contracts): the whole pregen
//!      mass collapses to contract legends (~33 B/chunk), payloads dropped;
//!   3. player reality: one live edit per region (a built/spawn chunk),
//!      stored honestly through the normal pipeline (flip-on-mismatch);
//!   4. the container weight is measured from the store directory itself —
//!      every segment, header, frame and dictionary included (worst case);
//!   5. the v0.9.49 export accounting at scale (chunks + pristine_skipped
//!      + lost == keys) and the exported Anvil re-imports with exact
//!      legends for every edit.
//!
//! Gate: container/anvil < x0.10 — the user-level -90% promise, asserted
//! at 262k-chunk scale (a panic exits non-zero, CI-able).
//!
//! Usage: cargo run --release --example scale_probe -- [regions_per_side]
//! Default side = 16 (256 regions = 262,144 chunks).

use c_saves::anvil;
use c_saves::chunknbt::{canonical_contract_hash, hash_of};
use c_saves::store::{Cfg, Store};
use std::collections::BTreeMap;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

const SEED: u64 = 0xC0FFEE;
const FLIP_STRIDE: usize = 1024; // one edited chunk per region file

fn work_dir(tag: &str) -> PathBuf {
    let work = std::env::temp_dir().join(format!(
        "csaves-scale-{tag}-{}",
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

/// One vanilla-format region file, streamed to disk (constant RAM: one
/// chunk payload + its zlib output at a time). Returns the file size —
/// the honest baseline bytes Paper would occupy for these 1024 chunks.
fn write_region(world: &wgen::World, out_dir: &Path, rx: i32, rz: i32) -> std::io::Result<u64> {
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
            let cx = rx * 32 + lx;
            let cz = rz * 32 + lz;
            let payload = wgen::chunk_payload(world, cx, cz, 0);
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
    hdr.extend_from_slice(&[0u8; 4096]); // timestamps: pregen, never touched
    f.write_all(&hdr)?;
    f.sync_all()?;
    Ok(f.metadata()?.len())
}

fn main() {
    let side: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let n_regions = side * side;
    let total_chunks = n_regions * 1024;
    let blocks = side * 512;
    let world = wgen::World::new(SEED);
    let work = work_dir("probe");
    let gen = work.join("gen");
    std::fs::create_dir_all(&gen).unwrap();

    // ---- 1. generate the pregen world as vanilla .mca files (parallel) ----
    let t0 = Instant::now();
    let anvil_bytes = AtomicU64::new(0);
    let done = AtomicUsize::new(0);
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (world, gen, ab, dn) = (&world, &gen, &anvil_bytes, &done);
            s.spawn(move || {
                for ri in (w..n_regions).step_by(nproc) {
                    let rx = (ri / side) as i32;
                    let rz = (ri % side) as i32;
                    let n = write_region(world, &gen, rx, rz).expect("region gen");
                    ab.fetch_add(n, Ordering::Relaxed);
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 32 == 0 || d == n_regions {
                        eprintln!(
                            "[scale] generated {d}/{n_regions} regions ({:.0} MiB)",
                            ab.load(Ordering::Relaxed) as f64 / 1048576.0
                        );
                    }
                }
            });
        }
    });
    let anvil = anvil_bytes.load(Ordering::Relaxed);
    let t_gen = t0.elapsed().as_secs_f64();
    eprintln!(
        "[scale] generation done in {t_gen:.1}s: {n_regions} region file(s), {:.1} MiB",
        anvil as f64 / 1048576.0
    );

    // ---- 2. pristine import: the pregen mass collapses to contracts ----
    let cfg = Cfg { pristine: true, pristine_import: true, ..Cfg::from_env() };
    let store = Store::open_cfg(&work.join("store"), true, cfg).unwrap();
    let t1 = Instant::now();
    let irep = anvil::import_dir(&gen, &store).expect("import ok");
    let t_import = t1.elapsed().as_secs_f64();
    assert_eq!(irep.chunks, total_chunks, "every generated chunk must import");
    assert_eq!(irep.failed, 0, "no import failures");
    let (stored, contracted) = classify(&store);
    assert!(stored.is_empty(), "flag-pure pregen must contract, not store");
    assert_eq!(contracted.len(), total_chunks);
    eprintln!(
        "[scale] import done in {t_import:.1}s: {} contracts, {} stored",
        contracted.len(),
        stored.len()
    );

    // ---- 3. player reality: one honest edit per region ----
    let mut flip_payloads: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for k in contracted.iter().step_by(FLIP_STRIDE).take(n_regions) {
        let cx = (k >> 32) as i32;
        let cz = (k & 0xffff_ffff) as u32 as i32;
        let p = wgen::chunk_payload(&world, cx, cz, 1); // round 1: rebuilt terrain = a real edit
        store.write_chunk(*k, &p, false).expect("live edit ok");
        assert!(!store.is_pristine(*k), "edit must supersede the contract");
        flip_payloads.insert(*k, p);
    }
    let flips = flip_payloads.len();
    store.sync();
    // The stored (normalized) form of every edit — exactly what read_chunk
    // hands back and what the exporter materializes. The live path
    // re-serializes through the section model, so the stored bytes may
    // differ from the handed-in payload byte-wise (semantically identical —
    // the Phase-A round-trip on the REAL corpus pins byte-identity for the
    // import->store->export path; here we anchor the legends to the STORED
    // form, which is what the export actually contains).
    let mut stored_payloads: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for k in flip_payloads.keys() {
        let q = store.read_chunk(*k).expect("stored edit reads back");
        stored_payloads.insert(*k, q);
    }
    let container = dir_size(&work.join("store"));
    let db = store.disk_bytes();
    let ratio = container as f64 / anvil.max(1) as f64;

    // ---- 4. export accounting at scale (v0.9.49 semantics) ----
    let rep = anvil::export_dir(&store, &work.join("export")).expect("export ok");
    assert_eq!(rep.chunks, flips, "export materializes exactly the edited part");
    assert_eq!(rep.pristine_skipped, total_chunks - flips);
    assert_eq!(rep.lost, 0, "nothing lost");
    assert_eq!(rep.chunks + rep.pristine_skipped + rep.lost, store.keys().len());

    // ---- 5. the exported Anvil re-imports with working legends ----
    // Legend semantics this pins: the re-import legend re-derives from the
    // exported bytes (= the stored form of each edit, read above). Both the
    // raw and the strip-canonical legend must describe EXACTLY that content,
    // and an identical re-save must keep the contract — the operational
    // no-erosion property the whole pristine line rests on.
    let cfg2 = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_canon: true,
        ..Cfg::from_env()
    };
    let s2 = Store::open_cfg(&work.join("store2"), true, cfg2).unwrap();
    let irep2 = anvil::import_dir(&work.join("export"), &s2).expect("re-import ok");
    assert_eq!(irep2.chunks, flips, "re-import sees exactly the edited part");
    let (st2, ct2) = classify(&s2);
    assert!(st2.is_empty(), "exported edits re-contract on import");
    let mut legend_miss = 0usize;
    for (k, q) in &stored_payloads {
        let Some((raw, canon)) = s2.pristine_legend(*k) else {
            legend_miss += 1;
            eprintln!("[scale] legend missing at key {k:016x}");
            continue;
        };
        if raw != hash_of(q) {
            legend_miss += 1;
            eprintln!("[scale] raw legend mismatch at key {k:016x}");
        }
        if canon != canonical_contract_hash(q) {
            legend_miss += 1;
            eprintln!("[scale] canonical legend mismatch at key {k:016x}");
        }
        // operational no-erosion: the identical re-save keeps the contract
        s2.write_chunk_import(*k, q).expect("re-save ok");
        if !s2.is_pristine(*k) {
            legend_miss += 1;
            eprintln!("[scale] identical re-save eroded the contract at key {k:016x}");
        }
    }
    assert_eq!(legend_miss, 0, "every edit must re-contract with a working legend");

    // ---- verdict ----
    println!(
        "scale world: {} regions x 1024 chunks = {} chunks ({}x{} blocks, {} region file(s))",
        n_regions, total_chunks, blocks, blocks, n_regions
    );
    println!(
        "scale anvil: {} bytes ({:.1} MiB) — vanilla zlib-6 sector layout",
        anvil,
        anvil as f64 / 1048576.0
    );
    println!(
        "scale container: {} bytes ({:.1} MiB) [disk_bytes {}] after {} contracts + {} stored edits",
        container,
        container as f64 / 1048576.0,
        db,
        total_chunks,
        flips
    );
    println!(
        "scale ratio: x{:.4} of anvil -> {:.1}% saved",
        ratio,
        (1.0 - ratio) * 100.0
    );
    if ratio >= 0.10 {
        panic!(
            "scale gate FAIL: container is x{ratio:.4} of anvil (target < x0.10 — the -90% promise at 262k scale)"
        );
    }
    println!("scale ratio gate: PASS (< x0.10 = the -90% promise holds at world scale)");
    println!(
        "scale export accounting: {} stored + {} pristine_skipped + {} lost = {} keys",
        rep.chunks,
        rep.pristine_skipped,
        rep.lost,
        store.keys().len()
    );
    println!(
        "scale reimport: {}/{} edits re-contracted; canonical legends verified, identical re-save keeps the contract",
        ct2.len(),
        flips
    );
    println!(
        "scale timing: generate {t_gen:.1}s, import {t_import:.1}s, total {:.1}s",
        t0.elapsed().as_secs_f64()
    );
    println!("=== scale probe: PASS ===");

    let _ = std::fs::remove_dir_all(&work);
}
