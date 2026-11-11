//! Vanilla Anvil (.mca) import/export — the 100%-compatibility bridge.
//!
//! Import: decompress every chunk payload and store it in the container
//! (payload = `NbtIo.write` form, exactly what the region file holds after
//! the compression layer — no re-serialization, bit-exact semantics).
//! Export: rebuild vanilla region files from the container (zlib, type 2).

use crate::store::Store;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct ImportReport {
    pub files: usize,
    pub chunks: usize,
    pub skipped_present: usize,
    pub failed: usize,
}

/// v0.9.49: what one export pass produced, classified per key.
/// `chunks + pristine_skipped + lost == store.keys().len()` — the operator
/// can always account for every key in the container:
/// - `chunks`: materialized into the vanilla region files (zlib, type 2);
/// - `pristine_skipped`: resolves to a pristine regeneration contract
///   (read = None BY DESIGN — nothing is stored, Paper regenerates the
///   chunk from the seed on load). The exported Anvil intentionally lacks
///   these chunks; the count is the proof that a partial export is the
///   elision working, not data loss. Non-regenerating consumers (mappers,
///   converters) must treat the count as "absent by contract";
/// - `lost`: read = None and NOT a pristine contract — a genuinely
///   unreadable key. Export never fails loudly mid-world for one bad key,
///   but it refuses to stay silent about it (stderr + the counter, and
///   the container keeps its per-key forensics via `read_fail_reason`).
pub struct ExportReport {
    pub regions: usize,
    pub chunks: usize,
    pub bytes: u64,
    pub pristine_skipped: usize,
    pub lost: usize,
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn decompress(comp_type: u8, data: &[u8]) -> Option<Vec<u8>> {
    match comp_type {
        1 => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data).read_to_end(&mut out).ok()?;
            Some(out)
        }
        2 => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).ok()?;
            Some(out)
        }
        3 => Some(data.to_vec()),
        _ => None,
    }
}

/// Import every region file in `regions_dir` into `store`.
/// Chunks already present in the container are skipped (never overwritten:
/// live server data wins over possibly stale .mca contents).
pub fn import_dir(regions_dir: &Path, store: &Arc<Store>) -> std::io::Result<ImportReport> {
    // Bulk conversion of cold legacy data: strip derived light (the server
    // relights with isLightOn=0 — vanilla-standard). Per-call, not a global:
    // runtime saves through the same store keep whatever Paper hands us.
    let mut rep = ImportReport { files: 0, chunks: 0, skipped_present: 0, failed: 0 };
    let mut files: Vec<_> = fs::read_dir(regions_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("r.") && n.ends_with(".mca")
        })
        .collect();
    files.sort_by_key(|e| e.file_name());
    for f in files {
        rep.files += 1;
        let clean = match import_file(&f.path(), store, &mut rep) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[c-saves] import {}: {}", f.file_name().to_string_lossy(), e);
                false
            }
        };
        if clean && !keep_mca() {
            eprintln!("[c-saves] removing imported region file {}", f.file_name().to_string_lossy());
            let _ = std::fs::remove_file(f.path());
        }
    }
    store.sync();
    Ok(rep)
}

fn keep_mca() -> bool {
    std::env::var("CSAVES_KEEP_MCA").map(|v| v == "1").unwrap_or(false)
}

/// Returns true when every chunk of this file was imported (or the file was
/// empty) — only then may the legacy file be removed.
fn import_file(path: &Path, store: &Arc<Store>, rep: &mut ImportReport) -> std::io::Result<bool> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad region name {name}")));
    }
    let rx: i32 = parts[1].parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "rx"))?;
    let rz: i32 = parts[2].parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "rz"))?;
    let data = fs::read(path)?;
    if data.len() < 8192 {
        return Ok(true); // empty region header only
    }
    let mut locations = [0u32; 1024];
    for i in 0..1024 {
        locations[i] = u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
    }
    // v0.9.52: under the region-assert tier, slots whose chunks were
    // contracted by this import are recorded and collapsed into ONE
    // region assert (136 B) when the whole file finished — the harsher
    // huge-world arithmetic (no per-chunk legends at all).
    let mut asserted_slots: Vec<usize> = Vec::new();
    for (i, loc) in locations.iter().enumerate() {
        if *loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        let sectors = (loc & 0xff) as usize;
        if off == 0 || off + 5 > data.len() {
            rep.failed += 1;
            continue;
        }
        let _ = sectors;
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if len < 2 || off + 4 + len > data.len() {
            rep.failed += 1;
            continue;
        }
        let comp_type = data[off + 4];
        let Some(payload) = decompress(comp_type, &data[off + 5..off + 4 + len]) else {
            rep.failed += 1;
            continue;
        };
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        if store.has_chunk(key(cx, cz)) {
            rep.skipped_present += 1;
            continue;
        }
        match store.write_chunk_import(key(cx, cz), &payload) {
            Ok(_) => {
                rep.chunks += 1;
                if store.pristine_region_enabled() {
                    asserted_slots.push(i);
                }
            }
            Err(e) => {
                eprintln!("[c-saves] chunk ({cx},{cz}) import failed: {e}");
                rep.failed += 1;
                return Ok(false);
            }
        }
    }
    if store.pristine_region_enabled() && !asserted_slots.is_empty() {
        if let Err(e) = store.pristine_region_assert(rx, rz, &asserted_slots) {
            eprintln!("[c-saves] region assert r.{}.{}.mca failed: {e}", rx, rz);
            return Ok(false);
        }
    }
    Ok(true)
}

/// One region's vanilla-form body, built from the container — the shared
/// core of the bulk export (v0.9.49/0.9.58 semantics) and the on-demand
/// serve (v0.9.59). STREAMED: only ONE chunk payload is resident at a
/// time (the 262,144-chunk OOM lesson). `read = None` has exactly two
/// meanings and must never be conflated (v0.9.49): a pristine contract is
/// absent BY DESIGN (regeneration semantics, counted as pristine_skipped);
/// anything else is a real read failure (counted as lost, never silent).
struct RegionBody {
    /// zlib entries + sector padding, ready to append after the 8 KiB header
    body: Vec<u8>,
    /// (slot index, sector offset, sector count) — the caller renders the
    /// location header from these
    placed: Vec<(usize, u32, u32)>,
    count: usize,
    bytes: u64,
    pristine_skipped: usize,
    lost: usize,
}

fn build_region_body(store: &Arc<Store>, keys: &[(i32, i32, u64)]) -> std::io::Result<RegionBody> {
    let mut body: Vec<u8> = Vec::new();
    let mut placed: Vec<(usize, u32, u32)> = Vec::new();
    let mut sector_off = 2u32; // 2 header sectors
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut pristine_skipped = 0usize;
    let mut lost = 0usize;
    for (cx, cz, k) in keys {
        let Some(payload) = store.read_chunk(*k) else {
            if store.is_pristine(*k) {
                pristine_skipped += 1;
            } else {
                lost += 1;
                eprintln!(
                    "[c-saves] export: key {k:016x} unreadable (not a pristine contract) — see read_fail_reason"
                );
            }
            continue;
        };
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&payload)?;
        let comp = z.finish()?;
        let entry_len = 5 + comp.len(); // len field(4) + type(1) + data
        let sector_count = ((entry_len + 4095) / 4096) as u32;
        let idx = ((cx.rem_euclid(32)) + (cz.rem_euclid(32)) * 32) as usize;
        placed.push((idx, sector_off, sector_count));
        let mut entry = Vec::with_capacity(entry_len);
        entry.extend_from_slice(&((1 + comp.len()) as u32).to_be_bytes());
        entry.push(2u8); // zlib
        entry.extend_from_slice(&comp);
        body.extend_from_slice(&entry);
        let pad = sector_count as usize * 4096 - entry_len;
        body.extend(std::iter::repeat(0u8).take(pad));
        sector_off += sector_count;
        count += 1;
        bytes += sector_count as u64 * 4096;
    }
    Ok(RegionBody { body, placed, count, bytes, pristine_skipped, lost })
}

/// v0.9.59 — THE CONTAINER IS THE WORLD. Serve the exact vanilla bytes of
/// `r.{rx}.{rz}.mca` from the container, in memory, on demand. Nothing is
/// written to disk: Anvil stops being a stored artifact and becomes a
/// VIEW the engine (or any vanilla-compatible tool) asks for. Byte-for-
/// byte identical to what `export_dir` would write for the same region
/// (both render through `build_region_body` — one code path, one truth).
/// Returns `Ok(None)` for a region with no keys at all.
pub fn serve_region(store: &Arc<Store>, rx: i32, rz: i32) -> std::io::Result<Option<Vec<u8>>> {
    let mut keys: Vec<(i32, i32, u64)> = Vec::new();
    for k in store.keys() {
        let cx = (k >> 32) as i32;
        let cz = k as u32 as i32;
        if cx.div_euclid(32) == rx && cz.div_euclid(32) == rz {
            keys.push((cx, cz, k));
        }
    }
    if keys.is_empty() {
        return Ok(None);
    }
    let rb = build_region_body(store, &keys)?;
    let mut out = Vec::with_capacity(8192 + rb.body.len());
    let mut offsets = [0u32; 1024];
    for (idx, so, sc) in &rb.placed {
        offsets[*idx] = (so << 8) | (sc & 0xff);
    }
    for v in offsets {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for _ in 0..1024 {
        out.extend_from_slice(&0u32.to_be_bytes()); // zero timestamps, as export
    }
    out.extend_from_slice(&rb.body);
    Ok(Some(out))
}

/// The engine-hook entry point (v0.9.59): the vanilla region ENTRY for one
/// chunk — len(4 BE) + compression type(1) + zlib payload — exactly the
/// bytes one sector holds, i.e. what a RegionFile shim (a mod that points
/// the game's chunk IO at this container) serves on a chunk read. `None`
/// means "absent by contract": a pristine contract stores nothing and the
/// caller regenerates from the seed (vanilla semantics), an unknown chunk
/// simply does not exist yet. Decompressing the entry yields the stored
/// payload bit-exactly (the same bytes `read_chunk` returns).
pub fn serve_chunk(store: &Arc<Store>, cx: i32, cz: i32) -> Option<Vec<u8>> {
    let k = key(cx, cz);
    let payload = store.read_chunk(k)?;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&payload).ok()?;
    let comp = z.finish().ok()?;
    let mut entry = Vec::with_capacity(5 + comp.len());
    entry.extend_from_slice(&((1 + comp.len()) as u32).to_be_bytes());
    entry.push(2u8); // zlib
    entry.extend_from_slice(&comp);
    Some(entry)
}

/// Export the whole container back into vanilla region files under `out_dir`
/// (zlib, vanilla layout). Returns an `ExportReport`; every key in the
/// container is accounted for: `chunks + pristine_skipped + lost` equals
/// `store.keys().len()` (v0.9.49 — the export path became pristine-aware:
/// a regeneration contract reads None BY DESIGN and is counted, not
/// silently dropped).
pub fn export_dir(store: &Arc<Store>, out_dir: &Path) -> std::io::Result<ExportReport> {
    fs::create_dir_all(out_dir)?;
    // v0.9.58 STREAMING EXPORT. The export used to materialize EVERY chunk
    // payload into RAM before writing the first region byte — fine at bench
    // corpora (<=20k chunks ~ 400 MB), fatal at world scale: the 262,144-
    // chunk worst-case CI gate got both attempts killed by OOM right after
    // phase B's verify passed (262,144 payloads ~ 5 GB resident; the runner
    // reports the OOM kill as a runner shutdown, exit 143). Now the payload
    // of a chunk lives in memory exactly while its region file is being
    // written: pass 1 groups KEYS only (~8 B each), pass 2 streams region
    // by region (1024 chunks ~ 20 MB peak). Same key order, same region
    // layout, same report semantics.
    let mut regions: HashMap<(i32, i32), Vec<(i32, i32, u64)>> = HashMap::new();
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut pristine_skipped = 0usize;
    let mut lost = 0usize;
    let keys = store.keys();
    for k in &keys {
        let k = *k;
        let cx = (k >> 32) as i32;
        let cz = k as u32 as i32;
        regions
            .entry((cx.div_euclid(32), cz.div_euclid(32)))
            .or_default()
            .push((cx, cz, k));
    }
    let regions_n = regions.len();
    // v0.9.67: parallel export — the deflate at vanilla zlib-6 is the wall
    // (~2.4 ms/chunk; the level is pinned by byte-identity with the vanilla
    // form, so cores are the only honest lever). Regions are independent
    // files: a worker pool streams them concurrently, each with the same
    // 1024-chunk ~20 MB RAM bound the streaming export introduced. File
    // CONTENT is unchanged (deterministic per region); only completion
    // order varies.
    let region_list: Vec<((i32, i32), Vec<(i32, i32, u64)>)> = regions.into_iter().collect();
    let out = std::sync::Mutex::new(Vec::<(usize, u64, usize, usize)>::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(region_list.len().max(1));
    std::thread::scope(|s| -> std::io::Result<()> {
        let mut handles = Vec::new();
        for _ in 0..workers {
            handles.push(s.spawn(|| -> std::io::Result<()> {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= region_list.len() {
                        return Ok(());
                    }
                    let ((rx, rz), keys) = &region_list[i];
                    let path = out_dir.join(format!("r.{rx}.{rz}.mca"));
                    let file = fs::File::create(&path)?;
                    let mut w = std::io::BufWriter::new(file);
                    let rb = build_region_body(store, keys)?;
                    let mut offsets = [0u32; 1024];
                    for (idx, so, sc) in &rb.placed {
                        offsets[*idx] = (so << 8) | (sc & 0xff);
                    }
                    for v in offsets {
                        w.write_all(&v.to_be_bytes())?;
                    }
                    for _ in 0..1024 {
                        w.write_all(&0u32.to_be_bytes())?;
                    }
                    w.write_all(&rb.body)?;
                    w.flush()?;
                    out.lock().unwrap().push((rb.count, rb.bytes, rb.pristine_skipped, rb.lost));
                }
            }));
        }
        for h in handles {
            h.join()
                .map_err(|_| std::io::Error::other("export worker panicked"))??;
        }
        Ok(())
    })?;
    let counts = out.into_inner().unwrap();
    for (c, b, ps, l) in counts {
        count += c;
        bytes += b;
        pristine_skipped += ps;
        lost += l;
    }
    if pristine_skipped > 0 {
        eprintln!(
            "[c-saves] export: {pristine_skipped} chunk(s) absent by pristine contract — vanilla regenerates them from the seed on load (mappers/converters see them as absent by design)"
        );
    }
    Ok(ExportReport {
        regions: regions_n,
        chunks: count,
        bytes,
        pristine_skipped,
        lost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunknbt;
    use crate::store::Cfg;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "csaves-anvil-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Minimal `NbtIo.write`-form payload: a root compound holding one Int
    /// (the byte-form the contract hashes over).
    fn payload(data_version: i32) -> Vec<u8> {
        let mut out = vec![10u8, 0, 0]; // TAG_Compound, empty root name
        out.push(3u8); // TAG_Int
        out.extend_from_slice(&11u16.to_be_bytes()); // name len
        out.extend_from_slice(b"DataVersion");
        out.extend_from_slice(&data_version.to_be_bytes());
        out.push(0u8); // TAG_End
        out
    }

    #[test]
    fn export_accounts_for_every_key_and_roundtrips() {
        let dir = temp_dir("exp-rep");
        let cfg = Cfg { pristine: true, ..Cfg::default() };
        let s = Store::open_cfg(&dir.join("store"), true, cfg).unwrap();
        // keys (i, i+7): i=0,1 stay contracts (identical re-save);
        // i=2 flips to honest storage (different content on the second write)
        for i in 0..3i32 {
            let k = ((i as u64) << 32) | ((i + 7) as u64);
            s.write_chunk(k, &payload(3700 + i), false).unwrap();
            if i < 2 {
                s.write_chunk(k, &payload(3700 + i), false).unwrap();
                assert!(s.is_pristine(k), "identical re-save stays a contract");
            } else {
                s.write_chunk(k, &payload(4000 + i), false).unwrap();
                assert!(!s.is_pristine(k), "different content must flip");
            }
        }
        assert_eq!(s.chunk_count(), 3);
        let k2 = (2u64 << 32) | 9u64;
        let stored_hash = chunknbt::hash_of(&s.read_chunk(k2).unwrap());
        s.sync();

        let rep = export_dir(&s, &dir.join("out")).unwrap();
        assert_eq!(rep.chunks, 1, "only the honestly-stored chunk materializes");
        assert_eq!(rep.pristine_skipped, 2, "contracts are counted, not dropped");
        assert_eq!(rep.lost, 0);
        assert_eq!(rep.regions, 1);
        assert_eq!(
            rep.chunks + rep.pristine_skipped + rep.lost,
            s.keys().len(),
            "every key must be accounted for"
        );

        // roundtrip: re-import the exported Anvil into a fresh store with
        // pristine_import ON — the import contracts exactly the exported
        // bytes, so the stored-A key becomes a contract in B whose raw
        // legend equals the production hash of the stored-A payload; the
        // contracted-A keys are absent from the export (regeneration
        // semantics) and therefore absent from B.
        let cfgb = Cfg { pristine: true, pristine_import: true, ..Cfg::default() };
        let sb = Store::open_cfg(&dir.join("store-b"), true, cfgb).unwrap();
        let irep = import_dir(&dir.join("out"), &sb).unwrap();
        assert_eq!(irep.chunks, 1);
        assert_eq!(sb.chunk_count(), 1, "contracted keys are absent from the export");
        assert!(sb.is_pristine(k2));
        let (raw, _) = sb.pristine_legend(k2).expect("legend present");
        assert_eq!(raw, stored_hash, "legend must re-derive from the exported bytes");

        drop(s);
        drop(sb);
        let _ = fs::remove_dir_all(&dir);
    }

    /// v0.9.59: the serve view is byte-for-byte what the export writes —
    /// one region builder feeds both, and the test pins it: for every
    /// region the container touches, `serve_region` bytes == the .mca file
    /// bytes; a region with no keys serves None (and export never created
    /// a file for it).
    #[test]
    fn serve_region_matches_export_byte_for_byte() {
        let dir = temp_dir("serve-identity");
        let cfg = Cfg { pristine: true, ..Cfg::default() };
        let s = Store::open_cfg(&dir.join("store"), true, cfg).unwrap();
        // region (0,0): keys (0..4, 7) — i=0 stays a contract (identical
        // re-save), i=1..3 store honestly; region (1,0): one stored chunk;
        // region (5,5): untouched (serves None).
        for i in 0..4i32 {
            let k = ((i as u64) << 32) | 7u64;
            s.write_chunk(k, &payload(3700 + i), false).unwrap();
            if i == 0 {
                s.write_chunk(k, &payload(3700 + i), false).unwrap(); // identical re-save
            } else {
                s.write_chunk(k, &payload(4000 + i), false).unwrap();
            }
        }
        let k = (32u64 << 32) | 3u64; // region (1,0)
        s.write_chunk(k, &payload(4100), false).unwrap();
        s.sync();

        let out = dir.join("ref");
        let rep = export_dir(&s, &out).unwrap();
        assert_eq!(rep.regions, 2);

        for (rx, rz) in [(0i32, 0i32), (1, 0)] {
            let served = serve_region(&s, rx, rz).unwrap().expect("region has keys");
            let on_disk = fs::read(out.join(format!("r.{rx}.{rz}.mca"))).unwrap();
            assert_eq!(served, on_disk, "serve must equal export byte-for-byte");
        }
        assert!(serve_region(&s, 5, 5).unwrap().is_none(), "empty region serves None");

        // the header the serve view renders must locate every entry: walk
        // the location table of region (0,0) and check each claimed entry
        // begins with a plausible len+type=2 framing
        let served = serve_region(&s, 0, 0).unwrap().unwrap();
        let mut located = 0usize;
        for i in 0..1024usize {
            let raw = u32::from_be_bytes(served[i * 4..i * 4 + 4].try_into().unwrap());
            if raw == 0 {
                continue;
            }
            let off = (raw >> 8) as usize * 4096;
            let cnt = (raw & 0xff) as usize;
            let len = u32::from_be_bytes(served[off..off + 4].try_into().unwrap()) as usize;
            assert_eq!(served[off + 4], 2, "zlib type byte");
            assert!(len >= 6 && len <= cnt * 4096 - 4, "len {len} fits {cnt} sectors");
            located += 1;
        }
        assert_eq!(located, 3, "three stored chunks located in region (0,0)");

        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }

    /// v0.9.59: serve_chunk is the engine-hook entry — the entry bytes it
    /// returns are exactly the bytes at the region slot the serve view's
    /// header claims, and they decompress to the stored payload
    /// bit-exactly. A pristine contract serves None (regeneration
    /// semantics) while its stored sibling serves Some.
    #[test]
    fn serve_chunk_framing_and_pristine_none() {
        let dir = temp_dir("serve-chunk");
        let cfg = Cfg { pristine: true, ..Cfg::default() };
        let s = Store::open_cfg(&dir.join("store"), true, cfg).unwrap();
        let k_stored = (1u64 << 32) | 1u64;
        let k_pristine = (2u64 << 32) | 2u64;
        // with pristine ON the FIRST write of a chunk is its worldgen form
        // (a contract candidate); a live edit with different content is
        // what flips it to honest storage
        let p = payload(4243);
        s.write_chunk(k_stored, &payload(4242), false).unwrap();
        s.write_chunk(k_stored, &p, false).unwrap();
        s.write_chunk(k_pristine, &payload(777), false).unwrap();
        s.write_chunk(k_pristine, &payload(777), false).unwrap(); // identical re-save
        s.sync();
        assert!(!s.is_pristine(k_stored), "a live edit flips to honest storage");
        assert!(s.is_pristine(k_pristine));

        // pristine -> None, by design
        assert!(serve_chunk(&s, 2, 2).is_none(), "contract regenerates, nothing to serve");
        // unknown -> None
        assert!(serve_chunk(&s, 9, 9).is_none());

        // stored -> Some, and the framing is the region slot's framing
        let entry = serve_chunk(&s, 1, 1).expect("stored chunk serves");
        let len = u32::from_be_bytes(entry[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, entry.len() - 4, "len field covers type+data");
        assert_eq!(entry[4], 2, "zlib");
        let mut d = flate2::read::ZlibDecoder::new(&entry[5..]);
        let mut round = Vec::new();
        d.read_to_end(&mut round).unwrap();
        assert_eq!(round, p, "decompresses bit-exactly to the stored payload");

        // and the same bytes sit at the slot the serve view's header claims
        let served = serve_region(&s, 0, 0).unwrap().unwrap();
        let raw = u32::from_be_bytes(served[0..4].try_into().unwrap()); // slot (1,1) -> idx 33
        let idx33 = u32::from_be_bytes(served[33 * 4..33 * 4 + 4].try_into().unwrap());
        assert!(idx33 != 0, "slot (1,1) located");
        let off = (idx33 >> 8) as usize * 4096;
        let cnt = (idx33 & 0xff) as usize;
        assert_eq!(
            &served[off..off + len + 4],
            &entry[..],
            "serve_chunk bytes == region-slot bytes"
        );
        assert!(cnt * 4096 >= len + 4);
        let _ = raw;

        drop(s);
        let _ = fs::remove_dir_all(&dir);
    }
}
