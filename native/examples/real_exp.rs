//! real_exp — attribution + lever experiments on a REAL anvil corpus
//! (dimension folder with region/ and entities/, e.g. produced by the
//! bigworld-survival workflow):
//!
//!   1. per-class raw + z19-concat weights (models slab frames)
//!   2. chunk-level exact-dedup rate (semantic chunk hashes)
//!   3. section-level uniform vs dense coverage + unique-section rate
//!   4. entity-class breakdown (which mobs weigh the most, raw + z19)
//!   5. block-entity shape breakdown (chests vs other BEs)
//!
//! Usage: real_exp <dimension_dir> [out_prefix]

use fastnbt::Value;
use std::collections::HashMap as Map;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};

fn z19(data: &[u8]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    zstd::bulk::compress(data, 19).map(|c| c.len() as u64).unwrap_or(0)
}

// ------------------------------------------------------------- anvil io ---

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let _rx: i32 = parts[1].parse().unwrap_or(0);
    let _rz: i32 = parts[2].parse().unwrap_or(0);
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
            1 => {
                let mut v = Vec::new();
                if flate2::read::GzDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            2 => {
                let mut v = Vec::new();
                if flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            3 => data[off + 5..off + 4 + len].to_vec(),
            _ => continue,
        };
        let cx = i32::from_be_bytes([data[off + 8], data[off + 9], data[off + 10], data[off + 11]]);
        let cz = i32::from_be_bytes([data[off + 12], data[off + 13], data[off + 14], data[off + 15]]);
        out.push((cx, cz, payload));
    }
    out
}

// --------------------------------------------------------- analysis ----

struct SecStats {
    sections: usize,
    uniform: usize,          // palette size <= 1
    dense: usize,
    rle_shaped: usize,       // dense sections whose vanilla-order stream RLEs 4x+
    bs_data_bytes: u64,
    palette_bytes: u64,
    biome_bytes: u64,
    light_bytes: u64,
}

fn rle_len(v: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < v.len() {
        let mut j = i + 1;
        while j < v.len() && v[j] == v[i] {
            j += 1;
        }
        let run = j - i;
        n += 1 + if run < 128 { 1 } else { 2 };
        i = j;
    }
    n
}

struct ChunkStats {
    n_chunks: usize,
    unique_chunks: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).cloned().unwrap_or_default());
    let region_dir = dim.join("region");
    let entity_dir = dim.join("entities");
    let mut region_chunks: Vec<(i32, i32, Vec<u8>)> = Vec::new();
    let mut entity_chunks: Vec<(i32, i32, Vec<u8>)> = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            region_chunks.extend(load_mca(f));
        }
    }
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(&entity_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            entity_chunks.extend(load_mca(f));
        }
    }
    println!(
        "real_exp: {} region chunks, {} entity chunks from {}",
        region_chunks.len(),
        entity_chunks.len(),
        dim.display()
    );

    let mut w = std::io::BufWriter::new(std::io::stdout());

    // ---- 1. class streams ----
    let mut streams: Map<&'static str, Vec<u8>> = Map::new();
    let mut add = |k: &'static str, b: &[u8], s: &mut Map<&'static str, Vec<u8>>| {
        s.entry(k).or_default().extend_from_slice(b);
    };
    let mut sec = SecStats {
        sections: 0,
        uniform: 0,
        dense: 0,
        rle_shaped: 0,
        bs_data_bytes: 0,
        palette_bytes: 0,
        biome_bytes: 0,
        light_bytes: 0,
    };
    let mut chunk_hashes: Vec<u64> = Vec::with_capacity(region_chunks.len());
    let mut ent_by_kind: Map<String, (u64, Vec<u8>)> = Map::new(); // kind -> (count, stream)
    let mut be_shapes: Map<&'static str, (u64, Vec<u8>)> = Map::new();
    let mut n_ent_chunks_empty = 0usize;
    let mut n_ent = 0u64;

    for (_, _, payload) in &region_chunks {
        let Ok(v) = fastnbt::from_bytes::<Value>(payload) else { continue };
        let Value::Compound(m) = v else { continue };
        // chunk content hash: ONLY the immutable-by-generation parts
        // (sections + Heightmaps). InhabitedTime/LastUpdate differ on
        // otherwise-identical chunks and would mask the dedup potential.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut content: Map<String, Value> = Map::new();
        if let Some(s) = m.get("sections") {
            content.insert("sections".to_string(), s.clone());
        }
        if let Some(hm) = m.get("Heightmaps") {
            content.insert("Heightmaps".to_string(), hm.clone());
        }
        if let Ok(canon) = fastnbt::to_bytes(&Value::Compound(content)) {
            canon.hash(&mut h);
            chunk_hashes.push(h.finish());
        }
        if let Some(hm) = m.get("Heightmaps") {
            let b = fastnbt::to_bytes(hm).unwrap_or_default();
            add("heightmaps", &b, &mut streams);
        }
        if let Some(Value::List(bes)) = m.get("block_entities") {
            for be in bes {
                let b = fastnbt::to_bytes(be).unwrap_or_default();
                let kind = match be {
                    Value::Compound(bm) => match bm.get("id") {
                        Some(Value::String(s)) => s.clone(),
                        _ => "unknown".into(),
                    },
                    _ => "unknown".into(),
                };
                let family = if kind.contains("chest") {
                    "be_chest"
                } else if kind.contains("furnace") || kind.contains("smoker") || kind.contains("blast_furnace") {
                    "be_furnace"
                } else if kind.contains("hopper") {
                    "be_hopper"
                } else if kind.contains("Sign") || kind.contains("sign") {
                    "be_sign"
                } else {
                    "be_other"
                };
                let e = be_shapes.entry(family).or_insert((0, Vec::new()));
                e.0 += 1;
                e.1.extend_from_slice(&b);
            }
        }
        if let Some(Value::List(secs)) = m.get("sections") {
            for s in secs {
                let Value::Compound(sm) = s else { continue };
                sec.sections += 1;
                if let Some(Value::Compound(bs)) = sm.get("block_states") {
                    let pb = match bs.get("palette") {
                        Some(p) => fastnbt::to_bytes(p).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    add("palette", &pb, &mut streams);
                    sec.palette_bytes += pb.len() as u64;
                    match bs.get("palette") {
                        Some(Value::List(p)) if p.len() > 1 => {
                            sec.dense += 1;
                            if let Some(Value::LongArray(la)) = bs.get("data") {
                                let raw: Vec<u8> = la
                                    .as_ref()
                                    .iter()
                                    .flat_map(|l| l.to_le_bytes())
                                    .collect();
                                sec.bs_data_bytes += raw.len() as u64;
                                add("bs_data", &raw, &mut streams);
                                // heuristic RLE shape check on 4-bit clamped
                                // data is not exact; approximate on bytes
                                if rle_len(&raw) * 3 < raw.len() {
                                    sec.rle_shaped += 1;
                                }
                            }
                        }
                        _ => {
                            sec.uniform += 1;
                        }
                    }
                }
                if let Some(bm) = sm.get("biomes") {
                    let b = fastnbt::to_bytes(bm).unwrap_or_default();
                    sec.biome_bytes += b.len() as u64;
                    add("biomes", &b, &mut streams);
                }
                for k in ["SkyLight", "BlockLight"] {
                    if let Some(Value::ByteArray(ba)) = sm.get(k) {
                        let b = ba.as_ref();
                        sec.light_bytes += b.len() as u64;
                        add("light", &b, &mut streams);
                    }
                }
            }
        }
    }
    for (_, _, payload) in &entity_chunks {
        let Ok(v) = fastnbt::from_bytes::<Value>(payload) else { continue };
        let Value::Compound(m) = v else { continue };
        if let Some(Value::List(ents)) = m.get("Entities") {
            if ents.is_empty() {
                n_ent_chunks_empty += 1;
            }
            for e in ents {
                n_ent += 1;
                let b = fastnbt::to_bytes(e).unwrap_or_default();
                let kind = match e {
                    Value::Compound(em) => match em.get("id") {
                        Some(Value::String(s)) => s.clone(),
                        _ => "unknown".into(),
                    },
                    _ => "unknown".into(),
                };
                let e = ent_by_kind.entry(kind).or_insert((0, Vec::new()));
                e.0 += 1;
                e.1.extend_from_slice(&b);
            }
        }
    }

    let mut rows: Vec<(&'static str, u64, u64)> = streams
        .iter()
        .map(|(k, v)| (*k, v.len() as u64, z19(v)))
        .collect();
    rows.push(("__entities__", {
        let t: u64 = ent_by_kind.values().map(|(_, s)| s.len() as u64).sum();
        t
    }, {
        let c: Vec<u8> = ent_by_kind.values().flat_map(|(_, s)| s.iter().cloned()).collect();
        z19(&c)
    }));
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let total_raw: u64 = rows.iter().map(|r| r.1).sum();
    let total_z: u64 = rows.iter().map(|r| r.2).sum();
    writeln!(w, "\n== per-class weights (raw / z19-concat) ==").unwrap();
    for (k, raw, z) in &rows {
        writeln!(
            w,
            "CLASS {:14} raw {:>10} ({:5.1}%)  z19 {:>9} ({:5.1}%)",
            k,
            raw,
            100.0 * *raw as f64 / total_raw.max(1) as f64,
            z,
            100.0 * *z as f64 / total_z.max(1) as f64
        )
        .unwrap();
    }
    writeln!(w, "CLASS {:14} raw {:>10}            z19 {:>9}", "TOTAL", total_raw, total_z).unwrap();

    // ---- 2. chunk dedup ----
    let mut uh: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for h in &chunk_hashes {
        uh.insert(*h);
    }
    let cs = ChunkStats {
        n_chunks: chunk_hashes.len(),
        unique_chunks: uh.len(),
    };
    writeln!(
        w,
        "\nCHUNK-DEDUP: {}/{} chunks semantically unique ({:.1}% dup rate)",
        cs.unique_chunks,
        cs.n_chunks,
        100.0 * (cs.n_chunks - cs.unique_chunks) as f64 / cs.n_chunks.max(1) as f64
    )
    .unwrap();

    // ---- 3. section coverage ----
    writeln!(
        w,
        "SECTIONS: {} total, uniform {}, dense {}, rle-shaped {}",
        sec.sections, sec.uniform, sec.dense, sec.rle_shaped
    )
    .unwrap();

    // ---- 4. entities by kind ----
    writeln!(w, "\n== entities: {} total, {} empty entity chunks ==", n_ent, n_ent_chunks_empty).unwrap();
    let mut erows: Vec<(String, u64, u64, u64)> = ent_by_kind
        .iter()
        .map(|(k, (n, s))| (k.clone(), *n, s.len() as u64, z19(s)))
        .collect();
    erows.sort_by(|a, b| b.3.cmp(&a.3));
    for (k, n, raw, z) in erows.iter().take(12) {
        writeln!(
            w,
            "ENT {:34} n {:>7}  raw {:>9}  z19 {:>9}  ({:.0} B/ent)",
            k,
            n,
            raw,
            z,
            *raw as f64 / n.max(1) as f64
        )
        .unwrap();
    }

    // ---- 5. block entities by family ----
    writeln!(w, "\n== block entities ==").unwrap();
    let mut brows: Vec<(&str, u64, u64, u64)> = be_shapes
        .iter()
        .map(|(k, (n, s))| (*k, *n, s.len() as u64, z19(s)))
        .collect();
    brows.sort_by(|a, b| b.3.cmp(&a.3));
    for (k, n, raw, z) in brows {
        writeln!(
            w,
            "BE {:12} n {:>7}  raw {:>9}  z19 {:>9}  ({:.0} B/be)",
            k,
            n,
            raw,
            z,
            raw as f64 / n.max(1) as f64
        )
        .unwrap();
    }

    w.flush().unwrap();
    println!("real_exp done");
}
