//! jj_profile: size profile of a REAL JJThunder To The Max world.
//!
//! Reads vanilla .mca region + entity files produced by Paper 26.2 with the
//! JJThunder datapack (world height y=-64..2032, 131 sections), then:
//!   P: classifies every payload component byte-exactly and reports what
//!      weighs the most (raw NBT bytes + zstd-19 floor per component);
//!   I: imports everything through real Stores (split + dedup + dict +
//!      compaction — the production pipeline) and reports container size;
//!   V: verifies semantic NBT equality on every chunk (100% compat gate).
//!
//! Usage: cargo run --release --example jj_profile -- <dim-dir> [out-dir]
//!   <dim-dir> contains region/*.mca and entities/*.mca

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

// ------------------------------------------------------------- anvil io ---

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
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

fn mca_on_disk(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".mca"))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

// -------------------------------------------------------- classification ---

fn val_bytes(v: &Value) -> u64 {
    // fastnbt requires a compound root: bare Lists (palettes) fail and
    // silently measured 0. Wrap, then strip the 5-byte wrapper.
    match v {
        Value::Compound(_) => fastnbt::to_bytes(v).map(|b| b.len() as u64).unwrap_or(0),
        _ => {
            let mut m = std::collections::HashMap::new();
            m.insert("p".to_string(), v.clone());
            match fastnbt::to_bytes(&Value::Compound(m)) {
                Ok(b) => (b.len() as u64).saturating_sub(5),
                Err(_) => 0,
            }
        }
    }
}

fn pal_data_stats(bs: &Value) -> (u64, u64) {
    // (palette name bytes, data bytes)
    let Value::Compound(bm) = bs else { return (0, 0) };
    let mut pal = 0u64;
    if let Some(Value::List(l)) = bm.get("palette") {
        for p in l {
            if let Value::String(s) = p {
                pal += s.len() as u64;
            }
        }
    }
    let mut data = 0u64;
    if let Some(Value::LongArray(la)) = bm.get("data") {
        data = (la.as_ref().len() * 8) as u64;
    }
    (pal, data)
}

#[derive(Default)]
struct Comps {
    // sections
    sec_bs_palette: u64,
    sec_bs_data: u64,
    sec_biomes: u64,
    sec_skylight: u64,
    sec_blocklight: u64,
    sec_other: u64,
    // tail
    t_heightmaps: u64,
    t_block_entities: u64,
    t_other: u64,
    n_chunks: u64,
    n_sections: u64,
    // entity payload
    e_entities: u64,
    e_top_other: u64,
    n_entities: u64,
}

impl Comps {
    fn total(&self) -> u64 {
        self.sec_bs_palette
            + self.sec_bs_data
            + self.sec_biomes
            + self.sec_skylight
            + self.sec_blocklight
            + self.sec_other
            + self.t_heightmaps
            + self.t_block_entities
            + self.t_other
    }
}

/// Serialize every section's sub-values into per-component streams so the
/// zstd floor is measured on realistic, contiguously-stored bytes.
fn classify_chunk(payload: &[u8], c: &mut Comps, streams: &mut Map<&'static str, Vec<u8>>) {
    let Ok(root) = fastnbt::from_bytes::<Value>(payload) else { return };
    let Value::Compound(map) = root else { return };
    c.n_chunks += 1;
    for (k, v) in &map {
        if k == "sections" {
            if let Value::List(list) = v {
                for s in list {
                    c.n_sections += 1;
                    let Value::Compound(sm) = s else { continue };
                    let mut sec_other = Vec::new();
                    for (sk, sv) in sm {
                        match sk.as_str() {
                            "block_states" => {
                                let (p, d) = pal_data_stats(sv);
                                c.sec_bs_palette += p;
                                c.sec_bs_data += d;
                                if let Value::Compound(bm) = sv {
                                    if let Some(Value::List(pal)) = bm.get("palette") {
                                        let st = streams.entry("sec_bs_palette").or_default();
                                        for x in pal {
                                            if let Value::String(s) = x {
                                                st.extend_from_slice(s.as_bytes());
                                            }
                                        }
                                    }
                                }
                                let _ = val_bytes(sv);
                            }
                            "biomes" => {
                                let (p, d) = pal_data_stats(sv);
                                let _ = (p, d);
                                c.sec_biomes += val_bytes(sv);
                                streams.entry("sec_biomes").or_default().extend(fastnbt::to_bytes(sv).unwrap_or_default());
                            }
                            "SkyLight" => {
                                c.sec_skylight += val_bytes(sv);
                                if let Value::ByteArray(ba) = sv {
                                    streams
                                        .entry("sec_skylight")
                                        .or_default()
                                        .extend(ba.as_ref().iter().map(|x| *x as u8));
                                }
                            }
                            "BlockLight" => {
                                c.sec_blocklight += val_bytes(sv);
                                if let Value::ByteArray(ba) = sv {
                                    streams
                                        .entry("sec_blocklight")
                                        .or_default()
                                        .extend(ba.as_ref().iter().map(|x| *x as u8));
                                }
                            }
                            _ => {
                                c.sec_other += val_bytes(sv);
                                sec_other.extend(fastnbt::to_bytes(sv).unwrap_or_default());
                            }
                        }
                    }
                    streams.entry("sec_other").or_default().extend(sec_other);
                }
            }
        } else {
            let b = val_bytes(v);
            match k.as_str() {
                "Heightmaps" => {
                    c.t_heightmaps += b;
                    streams
                        .entry("t_heightmaps")
                        .or_default()
                        .extend(fastnbt::to_bytes(v).unwrap_or_default());
                }
                "block_entities" => {
                    c.t_block_entities += b;
                    streams
                        .entry("t_block_entities")
                        .or_default()
                        .extend(fastnbt::to_bytes(v).unwrap_or_default());
                }
                _ => {
                    c.t_other += b;
                    streams
                        .entry("t_other")
                        .or_default()
                        .extend(fastnbt::to_bytes(v).unwrap_or_default());
                }
            }
        }
    }
}

fn classify_entities(payload: &[u8], c: &mut Comps, by_kind: &mut Map<String, (u64, u64)>) {
    let Ok(root) = fastnbt::from_bytes::<Value>(payload) else { return };
    let Value::Compound(map) = root else { return };
    c.n_chunks += 1;
    for (k, v) in &map {
        let b = val_bytes(v);
        if k == "Entities" {
            c.e_entities += b;
            if let Value::List(list) = v {
                c.n_entities += list.len() as u64;
                for e in list {
                    let (id, sz) = match e {
                        Value::Compound(em) => {
                            let id = match em.get("id") {
                                Some(Value::String(s)) => s.trim_start_matches("minecraft:").to_string(),
                                _ => "?".into(),
                            };
                            (id, val_bytes(e))
                        }
                        _ => ("?".into(), val_bytes(e)),
                    };
                    let ent = by_kind.entry(id).or_default();
                    ent.0 += 1;
                    ent.1 += sz;
                }
            }
        } else {
            c.e_top_other += b;
        }
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or(
        "/home/z/jj/server/world/dimensions/minecraft/overworld",
    ));
    let out = PathBuf::from(
        args.get(2).cloned().unwrap_or_else(|| "/tmp/jjprof".to_string()),
    );
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let region_dir = dim.join("region");
    let entity_dir = dim.join("entities");
    let mut region_chunks = Vec::new();
    let mut entity_chunks = Vec::new();
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

    println!("=== JJThunder real-world size profile ===");
    println!(
        "chunks: region={} entities={} | anvil on disk: region {:.2} MiB, entities {:.2} MiB",
        region_chunks.len(),
        entity_chunks.len(),
        mib(mca_on_disk(&region_dir)),
        mib(mca_on_disk(&entity_dir))
    );

    // ---- phase P: classification ----
    let mut comps = Comps::default();
    let mut streams: Map<&'static str, Vec<u8>> = Map::new();
    let mut by_kind: Map<String, (u64, u64)> = Map::new();
    let raw_region: u64 = region_chunks.iter().map(|(_, _, p)| p.len() as u64).sum();
    let raw_ent: u64 = entity_chunks.iter().map(|(_, _, p)| p.len() as u64).sum();
    for (_, _, p) in &region_chunks {
        classify_chunk(p, &mut comps, &mut streams);
    }
    for (_, _, p) in &entity_chunks {
        classify_entities(p, &mut comps, &mut by_kind);
    }

    println!("\n-- component weights (raw NBT bytes) --");
    let mut rows: Vec<(&str, u64)> = vec![
        ("sec block_states data", comps.sec_bs_data),
        ("sec block_states palette", comps.sec_bs_palette),
        ("sec SkyLight", comps.sec_skylight),
        ("sec BlockLight", comps.sec_blocklight),
        ("sec biomes", comps.sec_biomes),
        ("sec other", comps.sec_other),
        ("tail Heightmaps", comps.t_heightmaps),
        ("tail block_entities", comps.t_block_entities),
        ("tail other", comps.t_other),
    ];
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    let tot = comps.total().max(1);
    for (n, b) in &rows {
        println!("{n:26} {b:>12} ({:5.1}%)", 100.0 * *b as f64 / tot as f64);
    }
    println!(
        "{:26} {tot:>12} | chunks {} sections {}",
        "TOTAL", comps.n_chunks, comps.n_sections
    );
    println!(
        "raw payload streams: region {:.2} MiB, entities {:.2} MiB ({} entities)",
        mib(raw_region),
        mib(raw_ent),
        comps.n_entities
    );

    println!("\n-- entities by kind (raw NBT) --");
    let mut kinds: Vec<(&String, &(u64, u64))> = by_kind.iter().collect();
    kinds.sort_by_key(|x| std::cmp::Reverse(x.1 .1));
    for (id, (n, b)) in kinds.iter().take(12) {
        println!("  {id:22} n={n:>6} avg={:.0}B total={b} ({:4.1} MiB)", *b as f64 / *n as f64, mib(*b));
    }

    println!("\n-- zstd-19 floor per component (concatenated stream) --");
    let mut frows: Vec<(&str, u64, u64)> = rows
        .iter()
        .filter_map(|(n, _)| {
            let s = streams.get(n)?;
            let comp = zstd::bulk::compress(s, 19).unwrap_or_default();
            Some((*n, s.len() as u64, comp.len() as u64))
        })
        .collect();
    let ent_stream = streams.get("t_other").cloned().unwrap_or_default();
    let _ = ent_stream;
    frows.push(("entity payload raw", raw_ent, {
        zstd::bulk::compress(
            &entity_chunks
                .iter()
                .flat_map(|(_, _, p)| p.iter().copied())
                .collect::<Vec<u8>>(),
            19,
        )
        .unwrap_or_default()
        .len() as u64
    }));
    for (n, raw, comp) in &frows {
        println!("{n:26} raw {raw:>10} -> z19 {comp:>9} (x{:.2})", *raw as f64 / (*comp).max(1) as f64);
    }

    // ---- phase D: delta potential (v0.7 feasibility) ----
    // The remaining slab bytes live in NEAR-duplicate sections (mountain
    // strata boundaries shifted by +-1). This phase quantifies what a
    // pack-time similarity-delta pass could save: group unique sections by
    // canonical palette, diff each member against its group's base stream.
    println!("\n-- delta potential (unique non-uniform sections) --");
    {
        use std::collections::HashMap as DMap;
        let mut groups: DMap<u128, Vec<(u128, Vec<u8>, usize)>> = DMap::new();
        let mut analyzed: std::collections::HashSet<u128> = Default::default();
        for (_, _, p) in &region_chunks {
            let Ok(v) = fastnbt::from_bytes::<Value>(p) else { continue };
            let Value::Compound(m) = v else { continue };
            let Some(Value::List(secs)) = m.get("sections") else { continue };
            for s in secs {
                let Some(enc) = c_saves::chunknbt::section_encode(s) else { continue };
                if !matches!(enc.first(), Some(&0xC7) | Some(&0xC8)) {
                    continue;
                }
                let h = c_saves::chunknbt::hash_of(&enc);
                if !analyzed.insert(h) {
                    continue;
                }
                let Some(dec) = c_saves::chunknbt::section_decode(&enc) else { continue };
                let Value::Compound(dm) = dec else { continue };
                let Some(Value::Compound(bs)) = dm.get("block_states") else { continue };
                let Some(Value::List(pal)) = bs.get("palette") else { continue };
                let n = pal.len();
                if n == 0 || n > 256 {
                    continue; // delta works on byte-index streams
                }
                let pal_bytes = fastnbt::to_bytes(&Value::List(pal.clone())).unwrap_or_default();
                let pal_hash = c_saves::chunknbt::hash_of(&pal_bytes);
                // uniform streams cannot delta
                let Some(Value::LongArray(la)) = bs.get("data") else { continue };
                let bits = {
                    let bf = {
                        let mut b = 1u32;
                        while (1usize << b) < n {
                            b += 1;
                        }
                        b
                    };
                    bf.max(4)
                };
                if la.as_ref().len() != (4096 + (64 / bits) as usize - 1) / (64 / bits) as usize {
                    continue;
                }
                let stream = c_saves::chunknbt::unpack_nostroddle(la.as_ref(), 4096, bits);
                let lo = *stream.iter().min().unwrap_or(&0);
                let hi = *stream.iter().max().unwrap_or(&0);
                if lo == hi {
                    continue;
                }
                groups.entry(pal_hash).or_default().push((h, stream, enc.len()));
            }
        }
        let mut covered = 0usize;
        let mut bytes_now = 0u64;
        let mut bytes_after = 0u64;
        let mut best_groups: Vec<(usize, u64, u64)> = Vec::new();
        for g in groups.values() {
            if g.len() < 2 {
                continue;
            }
            let (bh, bstream, _) = &g[0];
            let mut g_now = 0u64;
            let mut g_after = 0u64;
            let mut g_n = 0usize;
            for (h, st, cl) in g.iter().skip(1) {
                if h == bh {
                    continue;
                }
                let diff = bstream.iter().zip(st.iter()).filter(|(a, b)| a != b).count();
                // delta record ~= palette prefix (20% of csec) + 2B per diff + header
                let cost = (*cl as u64) / 5 + diff as u64 * 2 + 8;
                g_n += 1;
                g_now += *cl as u64;
                g_after += cost.min(*cl as u64);
            }
            if g_n > 0 {
                covered += g_n;
                bytes_now += g_now;
                bytes_after += g_after;
                best_groups.push((g_n, g_now, g_after));
            }
        }
        best_groups.sort_by(|a, b| (b.1 - b.2).cmp(&(a.1 - a.2)));
        println!(
            "delta candidates: {} members in multi-member palette groups",
            covered
        );
        println!(
            "delta potential: {} B -> {} B (x{:.2}, saving {:.1} KB)",
            bytes_now,
            bytes_after,
            bytes_now as f64 / bytes_after.max(1) as f64,
            (bytes_now - bytes_after) as f64 / 1024.0
        );
        for (i, (n, now, after)) in best_groups.iter().take(5).enumerate() {
            println!(
                "  top{}: {} members, {} -> {} B",
                i + 1,
                n,
                now,
                after
            );
        }
    }

    // ---- phase I: production pipeline ----
    println!("\n-- import through production pipeline --");
    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        slab_max_raw: std::env::var("CSAVES_SLAB_MAX_RAW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(Cfg::default().slab_max_raw),
        ..Cfg::default()
    };
    let rs = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
    let es = Store::open_cfg(&out.join("entities"), true, cfg.clone()).unwrap();
    es.set_whole_mode();
    let t0 = std::time::Instant::now();
    for (cx, cz, p) in &region_chunks {
        rs.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    for (cx, cz, p) in &entity_chunks {
        es.write_chunk(key(*cx, *cz), p, true).unwrap();
    }
    rs.sync();
    es.sync();
    rs.train_dict_if_needed();
    es.train_dict_if_needed();
    rs.seal();
    es.seal();
    // imported data must be slab-packed + cold-tiered (same path the server
    // module takes after an Anvil import)
    rs.request_pack();
    es.request_pack();
    while rs.compact_step() || es.compact_step() {}
    rs.sync();
    es.sync();
    let secs = t0.elapsed().as_secs_f64();

    let rd = rs.disk_bytes();
    let ed = es.disk_bytes();
    let anvil_region = mca_on_disk(&region_dir);
    let anvil_ent = mca_on_disk(&entity_dir);
    println!(
        "import+dict+compact: {:.1}s ({:.0} chunks/s)",
        secs,
        (region_chunks.len() + entity_chunks.len()) as f64 / secs
    );
    println!(
        "region : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller, {:.1}% )",
        mib(anvil_region),
        mib(rd),
        anvil_region as f64 / rd.max(1) as f64,
        100.0 * rd as f64 / anvil_region.max(1) as f64
    );
    println!(
        "entities: anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller, {:.1}% )",
        mib(anvil_ent),
        mib(ed),
        anvil_ent as f64 / ed.max(1) as f64,
        100.0 * ed as f64 / anvil_ent.max(1) as f64
    );
    println!(
        "TOTAL  : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_region + anvil_ent),
        mib(rd + ed),
        (anvil_region + anvil_ent) as f64 / (rd + ed).max(1) as f64
    );
    println!("region stats: {}", rs.stats_json());
    println!("entity stats: {}", es.stats_json());
    println!("region profile: {}", rs.profile_json());
    println!("entity profile: {}", es.profile_json());
    println!("entity blob histogram: {}", es.blob_histogram_json());
    println!("region enc-modes: {}", c_saves::chunknbt::enc_mode_stats());

    // ---- phase V: verification (semantic equality, all chunks) ----
    // canonical section re-encoding permutes palette order (semantically
    // neutral), so chunk equality = per-section expanded state vectors +
    // structural equality of everything else.
    let chunks_semantic = |a: &[u8], b: &[u8]| -> bool {
        let (Ok(Value::Compound(ma)), Ok(Value::Compound(mb))) =
            (fastnbt::from_bytes::<Value>(a), fastnbt::from_bytes::<Value>(b))
        else {
            return false;
        };
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
                    if !c_saves::chunknbt::sections_semantically_equal(sa, sb) {
                        return false;
                    }
                }
            } else if va != vb {
                return false;
            }
        }
        true
    };
    let mut ok = 0usize;
    let mut bad = 0usize;
    // CSAVES_VERIFY_SAMPLE=n: verify every n-th chunk (0 = all). The 512x512
    // CI bench verifies a sample to keep the job under the time cap.
    let sample: usize = std::env::var("CSAVES_VERIFY_SAMPLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    for (vi_, (cx, cz, p)) in region_chunks.iter().enumerate() {
        if vi_ % sample != 0 {
            continue;
        }
        let (cx, cz, p) = (*cx, *cz, p);
        match rs.read_chunk(key(cx, cz)) {
            Some(d) => {
                if chunks_semantic(p, &d) {
                    ok += 1;
                } else {
                    bad += 1;
                    if bad <= 3 {
                        eprintln!("semantic MISMATCH region chunk ({cx},{cz})");
                    }
                }
            }
            _ => {
                bad += 1;
                if bad <= 3 {
                    eprintln!("region MISSING chunk ({cx},{cz}) debug={}", rs.debug_blob_state(key(cx, cz)));
                }
            }
        }
    }
    for (ei, (cx, cz, p)) in entity_chunks.iter().enumerate() {
        if sample > 1 && ei % sample != 0 {
            continue;
        }
        let (cx, cz, p) = (*cx, *cz, p);
        match es.read_chunk(key(cx, cz)) {
            Some(d) => {
                let va: Value = fastnbt::from_bytes(p).unwrap();
                let vb: Value = fastnbt::from_bytes(&d).unwrap();
                if va == vb {
                    ok += 1;
                } else {
                    bad += 1;
                    if bad <= 3 {
                        eprintln!("ENTITY MISMATCH chunk ({cx},{cz}) raw_len={} got_len={} debug={}", p.len(), d.len(), es.debug_blob_state(key(cx, cz)));
                    }
                }
            }
            _ => {
                bad += 1;
                if bad <= 3 {
                    eprintln!("ENTITY MISSING chunk ({cx},{cz}) debug={}", es.debug_blob_state(key(cx, cz)));
                }
            }
        }
    }
    println!("verify: ok={ok} bad={bad} sampled_every={sample}");
    if bad > 0 {
        std::process::exit(1);
    }
}
