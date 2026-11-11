//! serve_probe (v0.9.59): the container IS the world — Anvil is a view,
//! never a file.
//!
//! Until now the module's story was round-trip: vanilla saves Anvil, we
//! compress it into the container, export_dir materializes Anvil back when
//! the world is needed. That makes the container an ARCHIVE. The honest
//! endgame the module has been pointing at: the .mca form must not exist
//! on disk AT ALL — not while playing, not between sessions. This probe
//! proves the storage side of that endgame at CI scale:
//!
//!   Takeover: a vanilla-saved world (65,536 chunks, wgen round 0) is
//!     imported pristine — and import DRAINS the .mca files it reads. From
//!     that moment the world directory contains ONLY the container.
//!   Play: every chunk is live-edited in place (wgen round 1 — a real,
//!     canonically-visible edit per chunk, 0.9.57 constructive rule),
//!     parallel. The world state now lives in the container and nowhere
//!     else; there is no .mca to grow stale.
//!   Disk truth (the gate): a recursive walk of the whole world tree
//!     counts ZERO *.mca files. The storage layer is our format, only.
//!   Identity: `serve_region` renders every region of the world IN MEMORY
//!     from the container; each rendered file must be byte-for-byte
//!     identical to what `export_dir` writes (one region builder feeds
//!     both — the test proves it, the probe proves it at world scale).
//!   Replay: the SERVED bytes are written to a scratch directory and
//!     re-imported into a fresh store; every one of the 65,536 chunks is
//!     verified semantically (canonical section equality) against the
//!     edited content — bad=0. A world that exists only as our container
//!     yields, on demand, a fully valid vanilla world.
//!   Hook: `serve_chunk` — the engine-hook entry a mod would call — must
//!     return exactly the bytes the region view places at the chunk's
//!     slot, for a sample of chunks across all regions.
//!
//! Usage: cargo run --release --example serve_probe -- [regions_per_side]
//! Default side = 8 (64 regions = 65,536 chunks = 4096x4096 blocks).

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
        "csaves-serve-{tag}-{}",
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

/// Count *.mca files anywhere under `root` — the disk-truth gate.
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

/// One vanilla-format region file (round `round` content in every slot),
/// streamed to disk — the "vanilla-saved world" the probe takes over.
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
        buf.write_all(&[0u8; 8192])?;
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
            buf.write_all(&[2u8])?;
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
    for i in 0..1024usize {
        let t = ((rx as u64 * 1024 + rz as u64 * 31 + i as u64 * 7) % 86400) as u32;
        hdr.extend_from_slice(&t.to_be_bytes());
    }
    f.write_all(&hdr)?;
    f.sync_all()?;
    Ok(f.metadata()?.len())
}

/// Semantic chunk equality — the canonical section re-encoding permutes
/// palette order (semantically neutral), so chunk equality = per-section
/// expanded state vectors + structural equality of everything else.
fn chunks_semantic(a: &Value, b: &Value, strip: bool) -> bool {
    let mut ma = a.clone();
    let Value::Compound(mb) = b else { return false };
    if strip {
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

/// Read back EVERY stored chunk through `store` and compare semantically
/// against the edited content (wgen round 1) — parallel, atomics only.
fn verify_all_stored(
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
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store, world, ok, bad, done) = (&store, world, &ok, &bad, &done);
            s.spawn(move || {
                for i in (w..keys.len()).step_by(nproc) {
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
                                    eprintln!("[serve] {tag}: semantic mismatch at key {k:016x}");
                                }
                            }
                            Err(_) => {
                                bad.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[serve] {tag}: stored form unparseable at key {k:016x}");
                            }
                        },
                        None => {
                            bad.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[serve] {tag}: read=None at key {k:016x}");
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 16384 == 0 || d == keys.len() {
                        eprintln!("[serve] {tag}: verified {d}/{}", keys.len());
                    }
                }
            });
        }
    });
    (ok.load(Ordering::Relaxed), bad.load(Ordering::Relaxed))
}

/// The production maintenance pass: fsync, seal, drive the pack sweep to
/// completion (the JJThunder import pattern, generalized — see
/// worst_case_probe). The container weight is measured after this.
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
            eprintln!("[serve] compaction step {steps}");
        }
    }
    steps
}

fn main() {
    let side: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let n_regions = side * side;
    let total_chunks = n_regions * 1024;
    let world = wgen::World::new(SEED);
    let work = work_dir("probe");
    let gen_seed = work.join("gen-seed"); // the vanilla-saved world (round 0)
    std::fs::create_dir_all(&gen_seed).unwrap();
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);

    // ---- 0. the vanilla-saved world, parallel ----
    let t0 = Instant::now();
    let anvil_bytes = AtomicU64::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (world, gs, ab, dn) = (&world, &gen_seed, &anvil_bytes, &done);
            s.spawn(move || {
                for ri in (w..n_regions).step_by(nproc) {
                    let rx = (ri / side) as i32;
                    let rz = (ri % side) as i32;
                    let n = write_region_round(world, gs, rx, rz, 0).expect("region gen");
                    ab.fetch_add(n, Ordering::Relaxed);
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 16 == 0 || d == n_regions {
                        eprintln!(
                            "[serve] generated {d}/{n_regions} regions ({:.0} MiB)",
                            ab.load(Ordering::Relaxed) as f64 / 1048576.0
                        );
                    }
                }
            });
        }
    });
    eprintln!("[serve] generation done in {:.1}s", t0.elapsed().as_secs_f64());

    // ---- 1. TAKEOVER: import pristine; the import DRAINS the .mca files.
    // From here the vanilla form of the world ceases to exist on disk. ----
    let t1 = Instant::now();
    let cfg = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_canon: true,
        ..Cfg::from_env()
    };
    let store = Store::open_cfg(&work.join("world"), true, cfg).unwrap();
    let irep = anvil::import_dir(&gen_seed, &store).expect("takeover import ok");
    eprintln!("[serve] takeover import done in {:.1}s", t1.elapsed().as_secs_f64());
    assert_eq!(irep.chunks, total_chunks, "the whole world imports");
    assert_eq!(irep.failed, 0, "no import failures");
    let (stored0, contracted0) = classify(&store);
    assert!(stored0.is_empty() && contracted0.len() == total_chunks);
    println!(
        "serve world: {} regions x 1024 chunks = {} chunks taken over from vanilla Anvil",
        n_regions, total_chunks
    );

    // ---- 2. PLAY: every chunk live-edited in place (parallel). The state
    // of the world now lives in the container and nowhere else. ----
    let t2 = Instant::now();
    let done_e = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (store, world, keys, dn) = (&store, &world, &contracted0, &done_e);
            s.spawn(move || {
                for i in (w..keys.len()).step_by(nproc) {
                    let k = keys[i];
                    let cx = (k >> 32) as i32;
                    let cz = (k & 0xffff_ffff) as u32 as i32;
                    let p = wgen::chunk_payload(world, cx, cz, 1); // a real edit
                    store.write_chunk(k, &p, false).expect("live edit ok");
                    let d = dn.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 16384 == 0 {
                        store.sync();
                    }
                    if d % 16384 == 0 || d == keys.len() {
                        eprintln!("[serve] edits {d}/{}", keys.len());
                    }
                }
            });
        }
    });
    eprintln!("[serve] live edits done in {:.1}s", t2.elapsed().as_secs_f64());
    let (stored1, contracted1) = classify(&store);
    assert_eq!(stored1.len(), total_chunks, "every chunk live-edited");
    assert_eq!(contracted1.len(), 0, "no pristine chunk remains");
    let steps = drive_maintenance(&store);
    eprintln!("[serve] compaction {steps} step(s)");

    // ---- 3. DISK TRUTH: zero .mca anywhere in the world tree ----
    let mca_on_disk = count_mca(&work);
    assert_eq!(mca_on_disk, 0, "the world tree must contain zero .mca files");
    let container = dir_size(&work.join("world"));
    println!(
        "serve disk truth: {} .mca files on disk after takeover + play — the world lives ONLY in the container ({} B)",
        mca_on_disk, container
    );

    // ---- 4. IDENTITY: the serve view == the export view, byte-for-byte,
    // for EVERY region of the world; per-region serve latency ----
    let t3 = Instant::now();
    let ref_dir = work.join("ref-view"); // the derived view (a proof artifact)
    let rep = anvil::export_dir(&store, &ref_dir).expect("reference export ok");
    assert_eq!(rep.chunks, total_chunks);
    assert_eq!(rep.pristine_skipped, 0);
    assert_eq!(rep.lost, 0);
    let mut lat: Vec<f64> = Vec::with_capacity(n_regions);
    let mut identical = 0usize;
    let mut served_bytes = 0u64;
    // v0.9.67: the identity pass renders regions in parallel (each region
    // is an independent deflate stream; the per-region latency stays the
    // honest per-render wall time on a saturated host).
    let results: std::sync::Mutex<Vec<(usize, f64, bool, u64)>> =
        std::sync::Mutex::new(Vec::with_capacity(n_regions));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    std::thread::scope(|s| {
        for _ in 0..nproc.min(n_regions.max(1)) {
            s.spawn(|| loop {
                let ri = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if ri >= n_regions {
                    return;
                }
                let rx = (ri / side) as i32;
                let rz = (ri % side) as i32;
                let t = Instant::now();
                let served = anvil::serve_region(&store, rx, rz).expect("serve ok");
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let served = served.expect("region has keys");
                let on_disk = std::fs::read(ref_dir.join(format!("r.{rx}.{rz}.mca"))).unwrap();
                let same = served == on_disk;
                if !same {
                    eprintln!("[serve] region ({rx},{rz}) differs from the export view");
                }
                results.lock().unwrap().push((ri, ms, same, served.len() as u64));
            });
        }
    });
    let mut res = results.into_inner().unwrap();
    res.sort_by_key(|(ri, _, _, _)| *ri);
    for (_, ms, same, b) in &res {
        lat.push(*ms);
        if *same {
            identical += 1;
        }
        served_bytes += *b;
    }
    let first_ms = lat[0];
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_ms = lat.iter().sum::<f64>() / lat.len() as f64;
    let p90_ms = lat[(lat.len() as f64 * 0.9) as usize % lat.len()];
    let max_ms = *lat.last().unwrap();
    let t_serve_all = t3.elapsed().as_secs_f64();
    assert_eq!(identical, n_regions, "every region must serve byte-identical");
    println!(
        "serve identity: {identical}/{n_regions} regions byte-identical to the derived export view ({:.1} MiB served in {t_serve_all:.1}s)",
        served_bytes as f64 / 1048576.0
    );
    // an empty far region serves None
    assert!(anvil::serve_region(&store, 999_999, 999_999).unwrap().is_none());
    let anvil_view = dir_size(&ref_dir);
    let ratio = container as f64 / anvil_view.max(1) as f64;
    println!(
        "serve container: {} byte(s) ({:.1} MiB) = x{:.4} of the Anvil view ({} B) — 100% live content",
        container,
        container as f64 / 1048576.0,
        ratio,
        anvil_view
    );
    println!(
        "serve latency: first {:.1} ms, mean {:.1} ms, p90 {:.1} ms, max {:.1} ms per region (in-memory, no disk)",
        first_ms, mean_ms, p90_ms, max_ms
    );
    assert!(ratio < 0.15, "serve gate: ratio x{ratio:.4} exceeds the x0.15 promise");

    // ---- 5. REPLAY: the SERVED bytes, re-imported fresh, must BE the
    // world — every chunk semantically verified, bad=0 ----
    let t4 = Instant::now();
    let served_dir = work.join("served-view"); // built ONLY from serve_region bytes
    std::fs::create_dir_all(&served_dir).unwrap();
    for ri in 0..n_regions {
        let rx = (ri / side) as i32;
        let rz = (ri % side) as i32;
        let served = anvil::serve_region(&store, rx, rz).unwrap().unwrap();
        std::fs::write(served_dir.join(format!("r.{rx}.{rz}.mca")), &served).unwrap();
    }
    // a PLAIN store (every pristine flag off): the served world lands as
    // honest stored content, so verify_all_stored reads real payloads back
    let cfg_c = Cfg {
        pristine: false,
        pristine_import: false,
        pristine_canon: false,
        pristine_fresh: false,
        pristine_region: false,
        ..Cfg::from_env()
    };
    let store_c = Store::open_cfg(&work.join("replay"), true, cfg_c).unwrap();
    let irep_c = anvil::import_dir(&served_dir, &store_c).expect("replay import ok");
    eprintln!("[serve] replay import done in {:.1}s", t4.elapsed().as_secs_f64());
    assert_eq!(irep_c.chunks, total_chunks, "the served world re-imports fully");
    assert_eq!(irep_c.failed, 0);
    let keys_c = store_c.keys();
    let (ok_c, bad_c) = verify_all_stored(&store_c, &world, &keys_c, nproc, true, "replay");
    assert_eq!(bad_c, 0, "the served world must verify chunk-for-chunk");
    assert_eq!(ok_c, total_chunks);
    println!(
        "serve world replay: re-import of the SERVED bytes: {} chunks, ok={} bad={}",
        total_chunks, ok_c, bad_c
    );

    // ---- 6. HOOK: serve_chunk (the engine-hook entry) returns exactly the
    // bytes the region view places at the chunk's slot — sampled across
    // all regions ----
    let mut hook_ok = 0usize;
    let mut hook_n = 0usize;
    'outer: for ri in 0..n_regions {
        let rx = (ri / side) as i32;
        let rz = (ri % side) as i32;
        let served = anvil::serve_region(&store, rx, rz).unwrap().unwrap();
        for j in (0..1024usize).step_by(32) {
            // a deterministic spread of slots per region
            let cx = rx * 32 + (j & 31) as i32;
            let cz = rz * 32 + (j >> 5) as i32;
            let entry = anvil::serve_chunk(&store, cx, cz).expect("stored chunk serves");
            let idx = (cx.rem_euclid(32) + cz.rem_euclid(32) * 32) as usize;
            let raw = u32::from_be_bytes(served[idx * 4..idx * 4 + 4].try_into().unwrap());
            assert!(raw != 0, "slot {idx} must be located in region ({rx},{rz})");
            let off = (raw >> 8) as usize * 4096;
            let cnt = (raw & 0xff) as usize;
            let len = u32::from_be_bytes(served[off..off + 4].try_into().unwrap()) as usize;
            assert_eq!(served[off..off + len + 4], entry, "hook bytes == slot bytes");
            assert!(len + 4 <= cnt * 4096, "entry fits its sector run");
            hook_ok += 1;
            hook_n += 1;
            if hook_n >= 1024 {
                break 'outer;
            }
        }
    }
    println!(
        "serve chunk hook: {hook_ok}/{hook_n} sampled entries match the region view byte-for-byte"
    );

    println!("=== serve probe: PASS ===");
}
