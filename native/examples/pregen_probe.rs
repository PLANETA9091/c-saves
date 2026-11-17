//! pregen_probe: prices the two huge-world levers on a REAL corpus.
//!   A) chunk-level recipe dedup — a chunk's recipe (id-encoded section
//!      refs + tail) is byte-identical for chunks that differ only in
//!      coordinates. The probe hashes payload-minus-Position-minus-
//!      Heightmaps-minus-sections as the recipe-identity proxy and counts
//!      the duplicate mass the run lever would collapse.
//!   B) heightmap columnar tier — Heightmaps move out of per-chunk tails
//!      into a key-ordered big-window stream. The probe concatenates the
//!      heightmap wire in key order and prices tuned raw LZMA2 against
//!      per-chunk zstd-19 (the inline-tail treatment today).
//!
//! Self-contained: imports <corpus>/region + <corpus>/entities .mca files
//! into a fresh store (the bench flow), reads payloads back, analyzes.
//!
//! Usage: cargo run --release --example pregen_probe -- <dim-dir>

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use flate2::read::{GzDecoder, ZlibDecoder};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
    let mut opts =
        liblzma::stream::LzmaOptions::new_preset(liblzma::stream::PRESET_EXTREME | 6).ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream
        .process_vec(data, &mut out, liblzma::stream::Action::Run)
        .ok()?;
    stream
        .process_vec(&[], &mut out, liblzma::stream::Action::Finish)
        .ok()?;
    Some(out)
}

fn zstd_of(data: &[u8]) -> usize {
    let mut c = zstd::Encoder::new(Vec::new(), 19).unwrap();
    c.write_all(data).unwrap();
    c.finish().unwrap().len()
}

fn fold(b: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(b);
    h.finish()
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u32) as u64
}

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else { return vec![] };
    if data.len() < 8192 {
        return vec![];
    }
    let mut out = Vec::new();
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
        if off + 5 > data.len() {
            continue;
        }
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        if len < 2 || off + 4 + len > data.len() {
            continue;
        }
        let comp_type = data[off + 4];
        let payload = match comp_type {
            2 => {
                let mut v = Vec::new();
                if ZlibDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            1 => {
                let mut v = Vec::new();
                if GzDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            _ => continue,
        };
        let slot = i;
        let cx = rx * 32 + (slot % 32) as i32;
        let cz = rz * 32 + (slot / 32) as i32;
        out.push((cx, cz, payload));
    }
    out
}

fn main() {
    let dim = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "../bench-corpus/dim".to_string()),
    );
    let store_dir = PathBuf::from("/tmp/pregen_probe_store");
    let _ = std::fs::remove_dir_all(&store_dir);
    std::fs::create_dir_all(&store_dir).unwrap();

    let mut region_chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        if f.to_string_lossy().ends_with(".mca") {
            region_chunks.extend(load_mca(f));
        }
    }
    println!("corpus: {} region chunks", region_chunks.len());

    let cfg = Cfg { dict_train: 400, ..Cfg::from_env() };
    let st = Store::open_cfg(&store_dir, true, cfg).unwrap();
    let mut raw_total = 0u64;
    for (cx, cz, p) in &region_chunks {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
        raw_total += p.len() as u64;
    }
    st.durability_tick();
    println!("imported: {} chunks, {:.2} MiB raw payloads", region_chunks.len(), raw_total as f64 / 1048576.0);

    // analysis: recipe identity + heightmap column simulation
    let mut keys: Vec<u64> = region_chunks.iter().map(|(cx, cz, _)| key(*cx, *cz)).collect();
    keys.sort();
    let mut pay_uniq: HashMap<u64, u32> = HashMap::new();
    let mut rec_uniq: HashMap<u64, u32> = HashMap::new();
    let mut hm_col: Vec<u8> = Vec::new();
    let mut hm_per_chunk_z = 0usize;
    let mut hm_n = 0usize;
    let mut hm_uniq: HashMap<u64, u32> = HashMap::new();
    let mut tail_rest_total = 0usize;
    let mut miss = 0usize;

    for k in &keys {
        let Some(p) = st.read_chunk(*k) else {
            miss += 1;
            continue;
        };
        *pay_uniq.entry(fold(&p)).or_default() += 1;
        let Ok(mut root) = fastnbt::from_bytes::<Value>(&p) else { continue };
        let Value::Compound(map) = &mut root else { continue };
        map.remove("xPos");
        map.remove("zPos");
        map.remove("Position");
        let hm = map.remove("Heightmaps");
        let sections = map.remove("sections");
        let Ok(stripped) = fastnbt::to_bytes(&root) else { continue };
        *rec_uniq.entry(fold(&stripped)).or_default() += 1;
        if let Ok(b) = fastnbt::to_bytes(&root) {
            tail_rest_total += b.len();
        }
        if let Some(hm) = hm {
            if let Ok(b) = fastnbt::to_bytes(&hm) {
                hm_col.extend_from_slice(&b);
                hm_per_chunk_z += zstd_of(&b);
                *hm_uniq.entry(fold(&b)).or_default() += 1;
                hm_n += 1;
            }
        }
        let _ = sections;
    }
    let n = keys.len();
    let uniq_pay = pay_uniq.len();
    let uniq_rec = rec_uniq.len();
    let dup_rec = n - miss - uniq_rec;
    println!("A) payload identity: unique {uniq_pay}/{n} (miss {miss})");
    println!(
        "A) recipe identity (pos+hmap+sec free): unique {uniq_rec}/{n} -> {dup_rec} duplicate recipes (~{} B raw recipe mass at ~104 B each)",
        dup_rec * 104
    );
    let col_lz = lzma2_raw(&hm_col).map(|b| b.len()).unwrap_or(usize::MAX);
    println!(
        "B) heightmaps: {hm_n} chunks carry them, {raw} B raw; unique {uq}/{n}; per-chunk z19 {hm_per_chunk_z} B vs columnar lzma2 {col_lz} B ({ratio:.2}x)",
        raw = hm_col.len(),
        uq = hm_uniq.len(),
        ratio = hm_per_chunk_z as f64 / col_lz.max(1) as f64
    );
    println!("B) tail minus heightmaps (raw proxy): {tail_rest_total} B");
}
