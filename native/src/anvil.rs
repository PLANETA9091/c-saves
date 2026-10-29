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
    // relights with isLightOn=0 — vanilla-standard). Runtime saves are NOT
    // affected: the runtime write path keeps whatever Paper hands us.
    crate::chunknbt::set_strip_light(true);
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
        match store.write_chunk(key(cx, cz), &payload, false) {
            Ok(_) => rep.chunks += 1,
            Err(e) => {
                eprintln!("[c-saves] chunk ({cx},{cz}) import failed: {e}");
                rep.failed += 1;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Export the whole container back into vanilla region files under `out_dir`
/// (zlib, vanilla layout). `count` returns (chunks_written, bytes_written).
pub fn export_dir(store: &Arc<Store>, out_dir: &Path) -> std::io::Result<(usize, u64)> {
    fs::create_dir_all(out_dir)?;
    // group chunk keys by region
    let mut regions: HashMap<(i32, i32), Vec<(i32, i32, Vec<u8>)>> = HashMap::new();
    let mut count = 0usize;
    let mut bytes = 0u64;
    let keys = store.keys();
    for k in keys {
        let cx = (k >> 32) as i32;
        let cz = k as u32 as i32;
        let Some(payload) = store.read_chunk(k) else { continue };
        regions.entry((cx.div_euclid(32), cz.div_euclid(32))).or_default().push((cx, cz, payload));
    }
    for ((rx, rz), chunks) in regions {
        let path = out_dir.join(format!("r.{rx}.{rz}.mca"));
        let file = fs::File::create(&path)?;
        let mut w = std::io::BufWriter::new(file);
        // layout: collect entries
        let mut offsets = [0u32; 1024];
        let mut body: Vec<u8> = Vec::new();
        let mut sector_off = 2u32; // 2 header sectors
        let mut placed: Vec<(usize, u32, u32)> = Vec::new(); // idx, sector_off, sector_count
        for (cx, cz, payload) in &chunks {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(payload)?;
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
    Ok((count, bytes))
}
