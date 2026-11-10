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
    for ((rx, rz), keys) in regions {
        let path = out_dir.join(format!("r.{rx}.{rz}.mca"));
        let file = fs::File::create(&path)?;
        let mut w = std::io::BufWriter::new(file);
        // layout: collect entries
        let mut offsets = [0u32; 1024];
        let mut body: Vec<u8> = Vec::new();
        let mut sector_off = 2u32; // 2 header sectors
        let mut placed: Vec<(usize, u32, u32)> = Vec::new(); // idx, sector_off, sector_count
        for (cx, cz, k) in &keys {
            // STREAMED READ: the payload of this one chunk is the only
            // chunk content resident; read = None has exactly two meanings
            // and the export must not conflate them (v0.9.49). A pristine
            // contract is absent BY DESIGN (regeneration semantics);
            // anything else is a real read failure and gets counted as
            // lost.
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
        for (idx, so, sc) in placed {
            offsets[idx] = (so << 8) | (sc & 0xff);
        }
        for v in offsets {
            w.write_all(&v.to_be_bytes())?;
        }
        let _timestamps = [0u32; 1024]; // zero timestamps: vanilla accepts them
        for i in 0..1024 {
            w.write_all(&0u32.to_be_bytes())?;
        }
        w.write_all(&body)?;
        w.flush()?;
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
}
