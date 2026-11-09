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
//!      legends for every edit;
//!   6. the v0.9.53 POI phase: a vanilla poi/ dir is fabricated at a
//!      documented density and measured; today's whole-mode container is
//!      compared against the elision (CSAVES_POI_ELIDE semantics — session
//!      scratchpad, 0 B persisted, vanilla rebuilds from block states);
//!   7. the v0.9.54 seed-verification gate: BEFORE any tier may claim a
//!      chunk is seed-reproducible, every chunk on disk is decompressed and
//!      hash-compared against the seed regeneration (wgen round 0 — the
//!      exact content a missing chunk restores to at runtime), and the
//!      surviving legends are checked THROUGH THE CONTAINER against the
//!      same seed hashes — the assert is proven, not assumed;
//!   8. the v0.9.54 divergent-chunk gate: a chunk whose content diverged
//!      from the seed (a mod, a data-pack edit, any pre-first-save
//!      mutation) is CAUGHT by the classifier, refused the assert and
//!      stored honestly — nothing lost, nothing silently reverted.
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
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
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

/// Hard-link every region file into a second directory — zero-cost corpus
/// copies for the multi-tier phases (the import unlinks the files it
/// drains; the other directories keep their links to the same inodes).
fn link_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        if e.metadata().unwrap().is_file() {
            std::fs::hard_link(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

/// One vanilla-format region file, streamed to disk (constant RAM: one
/// chunk payload + its zlib output at a time). Returns the file size —
/// the honest baseline bytes Paper would occupy for these 1024 chunks.
fn write_region(
    world: &wgen::World,
    out_dir: &Path,
    rx: i32,
    rz: i32,
    slot_override: Option<(usize, Option<Vec<u8>>)>,
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
            let cx = rx * 32 + lx;
            let cz = rz * 32 + lz;
            if let Some((s, None)) = slot_override {
                if s == i {
                    continue; // absent slot: the offsets entry stays 0, no sectors
                }
            }
            let payload = match &slot_override {
                Some((s, Some(p))) if *s == i => p.clone(),
                _ => wgen::chunk_payload(world, cx, cz, 0),
            };
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

/// One vanilla-format POI region file: per-chunk POI lists in the same
/// .mca container layout (8 KiB header + 4 KiB sectors, zlib-6). 41 poi
/// chunks per region — the documented density of a lived-in survival world
/// (villages, job sites, beds, bee nests). Returns the file size.
fn write_poi_region(out_dir: &Path, rx: i32, rz: i32) -> std::io::Result<u64> {
    let path = out_dir.join(format!("r.{rx}.{rz}.mca"));
    let mut f = std::fs::File::create(&path)?;
    let mut offsets = [0u32; 1024];
    {
        let mut buf = std::io::BufWriter::with_capacity(1 << 18, &mut f);
        buf.write_all(&[0u8; 8192])?; // location + timestamp header placeholder
        let mut sector_cursor = 8192u64 / 4096;
        for i in (0..1024usize).step_by(25) {
            let lx = (i & 31) as i32;
            let lz = (i >> 5) as i32;
            let payload = poi_chunk_nbt(rx * 32 + lx, rz * 32 + lz);
            let mut z = flate2::write::ZlibEncoder::new(
                Vec::with_capacity(payload.len()),
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
    hdr.extend_from_slice(&[0u8; 4096]);
    f.write_all(&hdr)?;
    f.sync_all()?;
    Ok(f.metadata()?.len())
}

/// One poi chunk's NBT in the vanilla PoiManager save form: a root
/// compound with a `Data` list of {Type, Pos[I;3], FreeTickets} records.
/// 1-3 POIs per chunk, deterministic in the coordinates.
fn poi_chunk_nbt(cx: i32, cz: i32) -> Vec<u8> {
    let n = 1 + ((cx as i64 * 341 + cz as i64 * 173).rem_euclid(3)) as usize;
    let mut b = Vec::with_capacity(128);
    b.push(0x0A); // TAG_Compound root
    b.extend_from_slice(&0u16.to_be_bytes()); // ""
    b.push(0x09); // TAG_List
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(b"Data");
    b.push(0x0A); // element type: TAG_Compound
    b.extend_from_slice(&(n as u32).to_be_bytes());
    for i in 0..n {
        b.push(0x0A);
        b.extend_from_slice(&0u16.to_be_bytes());
        // Type: "minecraft:bed"
        b.push(0x08);
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(b"Type");
        b.extend_from_slice(&13u16.to_be_bytes());
        b.extend_from_slice(b"minecraft:bed");
        // Pos: [I;3]
        b.push(0x0B);
        b.extend_from_slice(&3u16.to_be_bytes());
        b.extend_from_slice(b"Pos");
        b.extend_from_slice(&3u32.to_be_bytes());
        b.extend_from_slice(&(cx * 16 + i as i32).to_be_bytes());
        b.extend_from_slice(&64i32.to_be_bytes());
        b.extend_from_slice(&(cz * 16 + i as i32).to_be_bytes());
        // FreeTickets: 2
        b.push(0x03);
        b.extend_from_slice(&11u16.to_be_bytes());
        b.extend_from_slice(b"FreeTickets");
        b.extend_from_slice(&2i32.to_be_bytes());
        b.push(0x00); // end element
    }
    b.push(0x00); // end root
    b
}

/// The v0.9.54 seed-verification gate, one region file: every present
/// slot's on-disk payload is decompressed and CANONICALLY compared against
/// the seed regeneration (wgen round 0). The comparison is
/// `canonical_contract_hash` — order-free, strip-invariant — because NBT
/// byte order is NOT stable across independent serializations (fastnbt's
/// compounds are HashMap-backed; a real JVM behaves the same), and the
/// thing the pristine tier guarantees is canonical equality of the
/// restored content: read=None -> vanilla regenerates -> Paper re-serializes
/// -> the canon legend absorbs the byte drift. When `seed_hashes` is
/// given, the canonical seed hash of every verified key is recorded for
/// the through-the-container legend truth check.
struct RegionVerdict {
    present: usize,
    ok: usize,
    divergent: Option<(usize, u128, u128)>, // (slot, disk canon, seed canon)
}

fn verify_region_against_seed(
    world: &wgen::World,
    path: &Path,
    rx: i32,
    rz: i32,
    seed_hashes: Option<&Mutex<BTreeMap<u64, u128>>>,
) -> RegionVerdict {
    let data = std::fs::read(path).expect("region file readable");
    let mut v = RegionVerdict { present: 0, ok: 0, divergent: None };
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([
            data[i * 4],
            data[i * 4 + 1],
            data[i * 4 + 2],
            data[i * 4 + 3],
        ]);
        if loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        let len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        let comp_type = data[off + 4];
        assert_eq!(comp_type, 2, "probe corpus is zlib (vanilla type 2)");
        let mut disk_payload = Vec::with_capacity(len * 4);
        flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
            .read_to_end(&mut disk_payload)
            .expect("slot decompresses");
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        let seed_payload = wgen::chunk_payload(world, cx, cz, 0);
        v.present += 1;
        let dh = canonical_contract_hash(&disk_payload).expect("disk slot parses as NBT");
        let sh = canonical_contract_hash(&seed_payload).expect("seed regen parses as NBT");
        if dh == sh {
            v.ok += 1;
        } else if v.divergent.is_none() {
            v.divergent = Some((i, dh, sh));
        }
        if let Some(m) = seed_hashes {
            m.lock().unwrap().insert(wgen::key(cx, cz), sh);
        }
    }
    v
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
                    let n = write_region(world, &gen, rx, rz, None).expect("region gen");
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
    // zero-cost corpus copies: the legend-tier import below unlinks its
    // files one by one; the raw-only and region-assert tiers drain their
    // own hard-link sets of the SAME inodes.
    let gen_b = work.join("gen-legend");
    let gen_raw = work.join("gen-raw");
    let gen_region = work.join("gen-region");
    link_dir(&gen, &gen_b);
    link_dir(&gen, &gen_raw);
    link_dir(&gen, &gen_region);

    // ---- 2. pristine import: the pregen mass collapses to contracts ----
    let cfg = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_canon: true,
        ..Cfg::from_env()
    };
    let store = Store::open_cfg(&work.join("store"), true, cfg).unwrap();
    let t1 = Instant::now();
    let irep = anvil::import_dir(&gen_b, &store).expect("import ok");
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

    // ---- 6. the raw-only legend tier (v0.9.44/0.9.45 semantics, 17 B/chunk)
    // Same world, legends without the strip-canonical second hash — the
    // intermediate point of the arithmetic ladder (relight re-saves erode
    // this tier; 0.9.46 added the canon legend to absorb that class).
    let cfgr = Cfg { pristine: true, pristine_import: true, pristine_canon: false, ..Cfg::from_env() };
    let sraw = Store::open_cfg(&work.join("store-raw"), true, cfgr).unwrap();
    let irepr = anvil::import_dir(&gen_raw, &sraw).expect("raw-only import ok");
    assert_eq!(irepr.chunks, total_chunks, "raw-only tier imports the full world");
    let (stored_r, contracted_r) = classify(&sraw);
    assert!(stored_r.is_empty() && contracted_r.len() == total_chunks);
    sraw.sync();
    let container_r = dir_size(&work.join("store-raw"));
    let ratio_r = container_r as f64 / anvil.max(1) as f64;
    println!(
        "scale raw-only tier: {} bytes ({:.1} MiB) = x{:.4} of anvil (17 B/chunk legends, {} contracts)",
        container_r,
        container_r as f64 / 1048576.0,
        ratio_r,
        contracted_r.len()
    );
    drop(sraw);
    let _ = std::fs::remove_dir_all(&work.join("store-raw"));

    // ---- 6b. the seed-verification gate (v0.9.54): the assert is PROVEN,
    // not assumed. Before any tier may claim a chunk is seed-reproducible,
    // every chunk on disk is decompressed and hash-compared against the
    // seed regeneration (wgen round 0) — the exact content a missing chunk
    // restores to at runtime (read=None -> vanilla regenerates from the
    // seed). This is what makes the pristine tier a content-addressed
    // delta against the seed rather than a "skip": the classification is
    // verified, 262,144 times, on every CI run. The surviving legends are
    // then checked THROUGH THE CONTAINER against the same seed hashes.
    let seed_map = Mutex::new(BTreeMap::<u64, u128>::new());
    let v_ok = AtomicUsize::new(0);
    let v_present = AtomicUsize::new(0);
    let v_div = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for w in 0..nproc {
            let (world, gr, sm, ok, pr, dv) =
                (&world, &gen_region, &seed_map, &v_ok, &v_present, &v_div);
            s.spawn(move || {
                for ri in (w..n_regions).step_by(nproc) {
                    let rx = (ri / side) as i32;
                    let rz = (ri % side) as i32;
                    let v = verify_region_against_seed(
                        world,
                        &gr.join(format!("r.{rx}.{rz}.mca")),
                        rx,
                        rz,
                        Some(sm),
                    );
                    pr.fetch_add(v.present, Ordering::Relaxed);
                    ok.fetch_add(v.ok, Ordering::Relaxed);
                    if v.divergent.is_some() {
                        dv.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let (v_ok, v_present, v_div) = (
        v_ok.load(Ordering::Relaxed),
        v_present.load(Ordering::Relaxed),
        v_div.load(Ordering::Relaxed),
    );
    assert_eq!(v_present, total_chunks, "every generated slot present on disk");
    assert_eq!(v_ok, total_chunks, "every disk chunk equals the seed regeneration");
    assert_eq!(v_div, 0, "no divergent chunk in the verified corpus");
    println!(
        "scale seed verify: {v_ok}/{v_present} chunks on disk match seed regeneration (asserts and legends are proven, not assumed)"
    );
    // the legend truth: every contract still in the container must be a
    // TRUE statement about the seed — the canonical legend equals the
    // canonical hash of the seed regeneration (the exact semantic the
    // canon legend exists to carry: byte order drifts between
    // serializations, content does not)
    let seed_map = seed_map.into_inner().unwrap();
    let (stored_v, contracted_v) = classify(&store);
    assert_eq!(stored_v.len(), flips, "the flipped set is stored, not contracted");
    let mut legend_truth_miss = 0usize;
    for k in &contracted_v {
        match (store.pristine_legend(*k), seed_map.get(k)) {
            (Some((_, Some(canon))), Some(sh)) if canon == *sh => {}
            _ => legend_truth_miss += 1,
        }
    }
    assert_eq!(
        legend_truth_miss, 0,
        "every surviving legend is a true statement about the seed"
    );
    println!(
        "scale legend truth: {}/{} surviving legends equal the seed hash (checked through the container)",
        contracted_v.len(),
        contracted_v.len()
    );

    // ---- 7. the region-assert tier (v0.9.52, ~0.13 B/chunk) — the harsher
    // arithmetic: NO per-chunk legends at all, ONE 136 B record per region
    // asserts its slots seed-reproducible. The trade, verified here: ANY
    // re-save of an asserted key (identical included) stores honestly.
    let cfgc = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_region: true,
        ..Cfg::from_env()
    };
    let sc = Store::open_cfg(&work.join("store-region"), true, cfgc.clone()).unwrap();
    let irepc = anvil::import_dir(&gen_region, &sc).expect("region-assert import ok");
    assert_eq!(irepc.chunks, total_chunks, "region tier imports the full world");
    assert_eq!(irepc.failed, 0);
    let (stored_c, contracted_c) = classify(&sc);
    assert!(stored_c.is_empty(), "region tier stores nothing on a pure pregen import");
    assert_eq!(contracted_c.len(), total_chunks, "every slot present-by-contract");
    assert_eq!(
        sc.pristine_region_count(),
        n_regions,
        "one assert record per region file"
    );
    assert_eq!(sc.keys().len(), total_chunks, "keys() synthesizes the asserted slots");
    assert_eq!(sc.chunk_count(), total_chunks);
    sc.sync();
    let container_c = dir_size(&work.join("store-region"));
    let ratio_c = container_c as f64 / anvil.max(1) as f64;
    println!(
        "scale region-assert: {} region record(s) replace {} per-chunk legends",
        sc.pristine_region_count(),
        total_chunks
    );
    println!(
        "scale region container: {} bytes ({:.1} KiB) = x{:.5} of anvil -> {:.2}% saved",
        container_c,
        container_c as f64 / 1024.0,
        ratio_c,
        (1.0 - ratio_c) * 100.0
    );
    if ratio_c >= 0.001 {
        panic!(
            "region-assert tier FAIL: x{ratio_c:.5} of anvil (target < x0.001 — the 0.13 B/chunk arithmetic)"
        );
    }
    println!("scale region gate: PASS (< x0.001 — the harsher arithmetic holds)");

    // the verification cost, made concrete: an IDENTICAL re-save cannot be
    // verified without a legend — it stores honestly. One identical re-save
    // (spawn chunk (0,0)) + one real edit (the world's last chunk) — the
    // untouched rest of the world stays a 136-byte record each.
    let ka = wgen::key(0, 0);
    let kb = wgen::key(side as i32 * 32 - 1, side as i32 * 32 - 1);
    let pa = wgen::chunk_payload(&world, 0, 0, 0); // IDENTICAL to the import
    let pb = wgen::chunk_payload(&world, side as i32 * 32 - 1, side as i32 * 32 - 1, 1);
    sc.write_chunk(ka, &pa, false).expect("identical re-save ok");
    sc.write_chunk(kb, &pb, false).expect("edit ok");
    assert!(!sc.is_pristine(ka), "identical re-save supersedes the assert (no legend to verify)");
    assert!(!sc.is_pristine(kb), "edit supersedes the assert");
    assert!(sc.read_chunk(ka).is_some() && sc.read_chunk(kb).is_some(), "both stored honestly");
    assert_eq!(
        sc.pristine_flips.load(Ordering::Relaxed),
        2,
        "both re-saves counted as flips (§12 telemetry)"
    );
    assert!(sc.is_pristine(wgen::key(0, 1)), "the untouched rest stays asserted");
    sc.sync();
    let container_c2 = dir_size(&work.join("store-region"));
    let ratio_c2 = container_c2 as f64 / anvil.max(1) as f64;
    println!(
        "scale region verification cost: 2 re-save(s) stored honestly (identical included), {} of {} slots still asserted -> x{:.5} of anvil after edits",
        total_chunks - 2,
        total_chunks,
        ratio_c2
    );

    let rege = work.join("export-region");
    let repc = anvil::export_dir(&sc, &rege).expect("region export ok");
    assert_eq!(repc.chunks, 2, "export materializes exactly the stored part");
    assert_eq!(repc.pristine_skipped, total_chunks - 2);
    assert_eq!(repc.lost, 0);
    assert_eq!(repc.chunks + repc.pristine_skipped + repc.lost, sc.keys().len());
    println!(
        "scale region export accounting: 2 stored + {} pristine_skipped + 0 lost = {} keys",
        repc.pristine_skipped,
        sc.keys().len()
    );

    // re-import the export at region grain: each edited chunk re-asserts
    // its region slot (the export carries exactly one chunk per region)
    let sc2 = Store::open_cfg(&work.join("store-region2"), true, cfgc).unwrap();
    let irep2c = anvil::import_dir(&rege, &sc2).expect("region re-import ok");
    assert_eq!(irep2c.chunks, 2);
    let (stored_2c, contracted_2c) = classify(&sc2);
    assert!(stored_2c.is_empty(), "exported edits re-assert at region grain");
    assert_eq!(contracted_2c.len(), 2);
    assert_eq!(sc2.pristine_region_count(), 2, "one assert per exported region file");
    println!("scale region reimport: 2/2 edits re-asserted at region grain");
    drop(sc2);
    let _ = std::fs::remove_dir_all(&work.join("store-region2"));
    drop(sc);
    let _ = std::fs::remove_dir_all(&work.join("store-region"));

    // ---- 7b. the divergent-chunk gate (v0.9.54): the negative proof.
    // A chunk whose content DIVERGED from the seed (a mod, a data-pack
    // edit, any pre-first-save mutation) must never take the assert: the
    // seed-verification classifier catches it, the protocol refuses the
    // assert for that slot and the chunk is stored HONESTLY — nothing
    // lost, nothing silently reverted to seed content (the 0.9.44 hazard
    // class, demonstrated end to end at scale).
    let drx = (side / 2) as i32;
    let drz = (side / 2) as i32;
    const DSLOT: usize = 515; // lx=3, lz=16 — deterministic probe coordinates
    let dcx = drx * 32 + (DSLOT as i32 & 31);
    let dcz = drz * 32 + (DSLOT as i32 >> 5);
    let div_payload = wgen::chunk_payload(&world, dcx, dcz, 1); // round 1: real different content
    let rname = format!("r.{drx}.{drz}.mca");
    let tmpdir = work.join("tmp-region");
    std::fs::create_dir_all(&tmpdir).unwrap();
    // the divergent corpus file: full 1024 slots, slot DSLOT divergent
    write_region(&world, &tmpdir, drx, drz, Some((DSLOT, Some(div_payload.clone()))))
        .expect("divergent region ok");
    let gen_div = work.join("gen-div");
    std::fs::create_dir_all(&gen_div).unwrap();
    std::fs::rename(tmpdir.join(&rname), gen_div.join(&rname)).expect("rename ok");
    // the classifier catches it — the gate receipt
    let vd = verify_region_against_seed(&world, &gen_div.join(&rname), drx, drz, None);
    assert_eq!(vd.present, 1024, "the divergent file is a full region");
    assert_eq!(vd.ok, 1023, "1023 slots still match the seed");
    let (dslot, _dh, sh_div) = vd.divergent.expect("the divergent slot is caught");
    assert_eq!(dslot, DSLOT);
    println!(
        "scale divergence gate: chunk ({dcx},{dcz}) rejected — disk hash != seed hash (content is not seed-reproducible)"
    );
    // the protocol: the operator asserts ONLY the verified corpus — the
    // divergent region is rewritten with the slot absent (gen-div-verified,
    // rename over the hard link leaves `gen`'s inode intact), and the
    // divergent chunk itself goes through the honest store path.
    let gen_dv = work.join("gen-div-verified");
    link_dir(&gen, &gen_dv);
    write_region(&world, &tmpdir, drx, drz, Some((DSLOT, None))).expect("verified region ok");
    std::fs::rename(tmpdir.join(&rname), gen_dv.join(&rname))
        .expect("rename over the hard link ok");
    let cfgd = Cfg {
        pristine: true,
        pristine_import: true,
        pristine_region: true,
        ..Cfg::from_env()
    };
    let sd = Store::open_cfg(&work.join("store-div"), true, cfgd).unwrap();
    let irepd = anvil::import_dir(&gen_dv, &sd).expect("verified import ok");
    assert_eq!(irepd.chunks, total_chunks - 1, "every verified chunk imports");
    assert_eq!(irepd.failed, 0);
    let (stored_d, contracted_d) = classify(&sd);
    assert!(stored_d.is_empty() && contracted_d.len() == total_chunks - 1);
    assert_eq!(
        sd.pristine_region_count(),
        n_regions,
        "256 assert records — the divergent region's bitmap holds 1023 slots"
    );
    assert!(
        !sd.is_pristine(wgen::key(dcx, dcz)),
        "the divergent slot is not asserted"
    );
    // the divergent chunk goes through the honest store path — modeled
    // exactly as production: the chunk carries tick evidence, so the
    // JNI boundary hands fresh=false to write_chunk_fresh and the
    // v0.9.47 gate STORES it normally instead of forming a contract
    // (a plain write_chunk asserts freshness and would contract it —
    // the wrong model for a chunk the engine knows was modified)
    sd.write_chunk_fresh(wgen::key(dcx, dcz), &div_payload, false, false)
        .expect("divergent chunk stored honestly");
    let q_d = sd.read_chunk(wgen::key(dcx, dcz)).expect("divergent chunk reads back");
    assert_ne!(
        canonical_contract_hash(&q_d).expect("stored divergent chunk parses"),
        sh_div,
        "the stored form is still NOT the seed content (nothing silently reverted)"
    );
    sd.sync();
    let container_d = dir_size(&work.join("store-div"));
    let repd = anvil::export_dir(&sd, &work.join("export-div")).expect("divergent export ok");
    assert_eq!(repd.chunks, 1, "exactly the divergent chunk materializes");
    assert_eq!(repd.pristine_skipped, total_chunks - 1);
    assert_eq!(repd.lost, 0, "nothing lost");
    assert_eq!(repd.chunks + repd.pristine_skipped + repd.lost, sd.keys().len());
    println!(
        "scale divergence protocol: {} slots asserted (verified against the seed), 1 divergent chunk stored honestly",
        total_chunks - 1
    );
    println!(
        "scale divergence container: {} B — one incident costs one honest chunk, {} stay asserted",
        container_d,
        total_chunks - 1
    );
    println!(
        "scale restore identity: {} seed-restored + 1 container-stored = {}/{} restored, 0 lost",
        total_chunks - 1,
        sd.keys().len(),
        sd.keys().len()
    );
    drop(sd);
    let _ = std::fs::remove_dir_all(&gen_div);
    let _ = std::fs::remove_dir_all(&gen_dv);
    let _ = std::fs::remove_dir_all(&work.join("store-div"));
    let _ = std::fs::remove_dir_all(&work.join("export-div"));
    let _ = std::fs::remove_dir_all(&tmpdir);

    // ---- 8. the POI store (v0.9.53): the derived index, elided ----
    // A lived-in world also carries poi/*.mca — the points-of-interest
    // index. Vanilla rebuilds it from chunk block states on load
    // (PoiManager.checkConsistencyWithBlocks / Yarn initForPalette;
    // Moonrise redirects the same call), so persisting poi bytes buys
    // nothing across restarts. This phase measures the vanilla poi/ weight
    // at a documented density (41 poi chunks per region), today's
    // whole-mode container, and the elision (CSAVES_POI_ELIDE semantics:
    // bounded session scratchpad, read-your-writes, 0 B persisted).
    let poi_dir = work.join("gen-poi");
    std::fs::create_dir_all(&poi_dir).unwrap();
    let anvil_poi = AtomicU64::new(0);
    for ri in 0..n_regions {
        let rx = (ri / side) as i32;
        let rz = (ri % side) as i32;
        let n = write_poi_region(&poi_dir, rx, rz).expect("poi gen");
        anvil_poi.fetch_add(n, Ordering::Relaxed);
    }
    let anvil_poi = anvil_poi.load(Ordering::Relaxed);
    let poi_chunks = 41usize * n_regions;
    println!(
        "poi vanilla weight: {n_regions} file(s), {poi_chunks} poi chunks, {:.1} MiB (4 KiB sector granularity per poi chunk)",
        anvil_poi as f64 / 1048576.0
    );

    let poi_keys: Vec<u64> = (0..n_regions)
        .flat_map(|ri| {
            let rx = (ri / side) as i32;
            let rz = (ri % side) as i32;
            (0..1024usize).step_by(25).map(move |i| {
                wgen::key(rx * 32 + (i & 31) as i32, rz * 32 + (i >> 5) as i32)
            })
        })
        .collect();
    assert_eq!(poi_keys.len(), poi_chunks);
    let poi_payloads: BTreeMap<u64, Vec<u8>> = poi_keys
        .iter()
        .map(|&k| {
            let cx = (k >> 32) as i32;
            let cz = (k & 0xffff_ffff) as u32 as i32;
            (k, poi_chunk_nbt(cx, cz))
        })
        .collect();

    // today's behavior: the whole-mode store persists the poi payloads
    let s_off = Store::open_cfg(&work.join("store-poi-off"), true, Cfg::from_env()).unwrap();
    s_off.set_whole_mode();
    for (k, p) in &poi_payloads {
        s_off.write_chunk(*k, p, true).expect("poi write ok");
    }
    s_off.sync();
    let container_poi_off = dir_size(&work.join("store-poi-off"));
    println!(
        "poi container today (elide off): {:.1} KiB (whole-mode store: field-split + blobs)",
        container_poi_off as f64 / 1024.0
    );

    // the elision: the same writes, 0 B persisted, session scratchpad only
    let s_on = Store::open_cfg(&work.join("store-poi-on"), true, Cfg::from_env()).unwrap();
    s_on.set_whole_mode();
    s_on.set_poi_elide();
    for (k, p) in &poi_payloads {
        s_on.write_chunk(*k, p, true).expect("poi write ok");
    }
    let sample: Vec<u64> = poi_keys.iter().step_by(512).copied().collect();
    for k in &sample {
        assert_eq!(
            s_on.read_chunk(*k).as_deref(),
            poi_payloads.get(k).map(|v| v.as_slice()),
            "session read-your-writes"
        );
    }
    s_on.sync();
    let container_poi_on = dir_size(&work.join("store-poi-on"));
    assert_eq!(s_on.elide_writes.load(Ordering::Relaxed), poi_chunks as u64);
    assert_eq!(
        s_on.elide_bytes.load(Ordering::Relaxed),
        poi_payloads.values().map(|v| v.len()).sum::<usize>() as u64
    );
    println!(
        "poi container elided: {:.1} KiB — 0 B persisted, session scratchpad only (staged {}, cap 64 MiB)",
        container_poi_on as f64 / 1024.0,
        s_on.elide_ram_bytes()
    );
    drop(s_on);
    let s_re = Store::open_cfg(&work.join("store-poi-on"), true, Cfg::from_env()).unwrap();
    s_re.set_poi_elide();
    let survived = sample.iter().filter(|k| s_re.read_chunk(**k).is_some()).count();
    assert_eq!(survived, 0, "the scratchpad is RAM-only — nothing may survive reopen");
    println!(
        "poi elide reopen: 0/{} payloads survive (RAM-only — vanilla rebuilds from block states)",
        sample.len()
    );
    if container_poi_on > container_poi_off {
        panic!(
            "poi elision FAIL: elided container {container_poi_on} B heavier than today's {container_poi_off} B"
        );
    }
    println!("poi elision gate: PASS (vanilla poi/ -> 0 persisted)");

    // combined arithmetic at the flagship tiers: region-assert region
    // container + elided poi vs Anvil region + poi — the WHOLE-world weight
    let total_anvil = anvil + anvil_poi;
    let total_mod = container_c2 + container_poi_on;
    let ratio_t = total_mod as f64 / total_anvil.max(1) as f64;
    println!(
        "scale total with poi: anvil {} bytes ({:.1} MiB) vs c-saves {} bytes ({:.1} KiB) = x{:.6} -> {:.3}% saved",
        total_anvil,
        total_anvil as f64 / 1048576.0,
        total_mod,
        total_mod as f64 / 1024.0,
        ratio_t,
        (1.0 - ratio_t) * 100.0
    );
    if ratio_t >= 0.0001 {
        panic!(
            "total gate FAIL: x{ratio_t:.6} of anvil region+poi (target < x0.0001 — the whole-world arithmetic)"
        );
    }
    println!("scale total gate: PASS (< x0.0001 — the whole-world arithmetic)");

    drop(s_off);
    drop(s_re);
    let _ = std::fs::remove_dir_all(&work.join("store-poi-off"));
    let _ = std::fs::remove_dir_all(&work.join("store-poi-on"));
    let _ = std::fs::remove_dir_all(&poi_dir);

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
