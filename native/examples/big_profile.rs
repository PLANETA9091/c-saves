//! big_profile — streaming size profile for LARGE anvil worlds (the
//! 512x512-chunk bigworld-survival benchmark). Unlike jj_profile it never
//! holds the whole world in RAM (that OOM-killed the CI runner: the full
//! corpus Vec + per-class byte streams = 2x the world size), and it skips
//! the per-section delta-potential pass (hours on 6M sections).
//!
//!   phase A: per-file stream — classify component weights + write into the
//!            production c-saves pipeline chunk by chunk
//!   phase B: pack pass (slab batching + cold tier)
//!   phase C: verify — re-read the anvil files per-file and semantically
//!            compare every Nth chunk (CSAVES_VERIFY_SAMPLE, default 10)
//!
//! Usage: big_profile <dim-dir>

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use flate2::read::ZlibDecoder;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
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
                if flate2::read::GzDecoder::new(&data[off + 5..off + 4 + len])
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

#[derive(Default)]
struct Comps {
    n_chunks: u64,
    n_sections: u64,
    sec_bs_data: u64,
    sec_bs_palette: u64,
    sec_skylight: u64,
    sec_blocklight: u64,
    sec_biomes: u64,
    sec_other: u64,
    t_heightmaps: u64,
    t_block_entities: u64,
    t_other: u64,
    n_entities: u64,
    ent_bytes: u64,
}

impl Comps {
    fn total(&self) -> u64 {
        self.sec_bs_data
            + self.sec_bs_palette
            + self.sec_skylight
            + self.sec_blocklight
            + self.sec_biomes
            + self.sec_other
            + self.t_heightmaps
            + self.t_block_entities
            + self.t_other
    }
}

fn comp_bytes(v: &Value) -> u64 {
    fastnbt::to_bytes(v).map(|b| b.len() as u64).unwrap_or(0)
}

fn classify_chunk(p: &[u8], c: &mut Comps) {
    let Ok(v) = fastnbt::from_bytes::<Value>(p) else { return };
    let Value::Compound(m) = v else { return };
    c.n_chunks += 1;
    match m.get("sections") {
        Some(Value::List(secs)) => {
            c.n_sections += secs.len() as u64;
            for s in secs {
                let Value::Compound(sm) = s else { continue };
                let mut known = 0u64;
                if let Some(bs) = sm.get("block_states") {
                    if let Value::Compound(bm) = bs {
                        if let Some(Value::LongArray(la)) = bm.get("data") {
                            c.sec_bs_data += la.as_ref().len() * 8;
                            known += (la.as_ref().len() * 8) as u64;
                        }
                        if let Some(pal) = bm.get("palette") {
                            let b = comp_bytes(pal);
                            c.sec_bs_palette += b;
                            known += b;
                        }
                    }
                }
                for (k, slot) in [
                    ("SkyLight", &mut c.sec_skylight),
                    ("BlockLight", &mut c.sec_blocklight),
                    ("biomes", &mut c.sec_biomes),
                ] {
                    if let Some(v) = sm.get(k) {
                        let b = match v {
                            Value::ByteArray(ba) => ba.as_ref().len() as u64,
                            _ => comp_bytes(v),
                        };
                        *slot += b;
                        known += b;
                    }
                }
                let total = comp_bytes(s);
                c.sec_other += total.saturating_sub(known);
            }
        }
        _ => {}
    }
    let mut tail_known = 0u64;
    if let Some(hm) = m.get("Heightmaps") {
        let b = comp_bytes(hm);
        c.t_heightmaps += b;
        tail_known += b;
    }
    if let Some(Value::List(bes)) = m.get("block_entities") {
        c.t_block_entities += bes
            .iter()
            .map(|be| {
                // fastnbt requires a compound root; serialize per-entity
                let b = match be {
                    Value::Compound(_) => comp_bytes(be),
                    _ => comp_bytes(be),
                };
                b
            })
            .sum::<u64>();
        // approximate: name keys overhead — fold into t_other below
    }
    let tail_total = {
        let Value::Compound(mut tm) = m.clone() else { return };
        tm.remove("sections");
        comp_bytes(&Value::Compound(tm))
    };
    c.t_other += tail_total.saturating_sub(tail_known);
}

fn classify_entities(p: &[u8], c: &mut Comps) {
    let Ok(v) = fastnbt::from_bytes::<Value>(p) else { return };
    let Value::Compound(m) = v else { return };
    if let Some(Value::List(ents)) = m.get("Entities") {
        c.n_entities += ents.len() as u64;
        c.ent_bytes += ents.iter().map(|e| comp_bytes(e)).sum::<u64>();
    }
}

fn bench_cfg() -> Cfg {
    Cfg {
        compact_ratio: 0.30,
        dict_train: 40,
        ..Cfg::default()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).cloned().unwrap_or_default());
    let region_dir = dim.join("region");
    let entity_dir = dim.join("entities");
    let sample: usize = std::env::var("CSAVES_VERIFY_SAMPLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let out = PathBuf::from("/tmp/csaves-big-profile");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let mut region_files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    region_files.sort();
    let mut ent_files: Vec<PathBuf> = std::fs::read_dir(&entity_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    ent_files.sort();

    let anvil_region = dir_bytes(&region_dir);
    let anvil_ent = dir_bytes(&entity_dir);

    // ---- phase A: streaming classify + import ----
    let store_dir = out.join("store-region");
    let _ = std::fs::remove_dir_all(&store_dir);
    let rs = Store::open_cfg(&store_dir, true, bench_cfg()).unwrap();
    let store_dir_e = out.join("store-entities");
    let _ = std::fs::remove_dir_all(&store_dir_e);
    let es = Store::open_cfg(&store_dir_e, true, bench_cfg()).unwrap();
    es.set_whole_mode();

    let mut comps = Comps::default();
    let mut n_region_chunks = 0u64;
    let mut raw_region = 0u64;
    let mut n_ent_chunks = 0u64;
    let mut raw_ent = 0u64;
    let t0 = std::time::Instant::now();
    for f in &region_files {
        if !f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            continue;
        }
        for (cx, cz, payload) in load_mca(f) {
            classify_chunk(&payload, &mut comps);
            raw_region += payload.len() as u64;
            n_region_chunks += 1;
            if rs.write_chunk(key(cx, cz), &payload, false).is_err() {
                eprintln!("write failed chunk ({cx},{cz})");
            }
        }
    }
    for f in &ent_files {
        if !f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            continue;
        }
        for (cx, cz, payload) in load_mca(f) {
            classify_entities(&payload, &mut comps);
            raw_ent += payload.len() as u64;
            n_ent_chunks += 1;
            let _ = es.write_chunk(key(cx, cz), &payload, true);
        }
    }
    rs.sync();
    es.sync();
    rs.train_dict_if_needed();
    es.train_dict_if_needed();
    rs.seal();
    es.seal();
    rs.request_pack();
    es.request_pack();
    loop {
        let a = rs.compact_step();
        let b = es.compact_step();
        if !a && !b {
            break;
        }
    }
    let rd = rs.disk_bytes();
    let ed = es.disk_bytes();
    let secs = t0.elapsed().as_secs_f64();

    println!(
        "chunks: region={} entities={} | anvil on disk: region {:.2} MiB, entities {:.2} MiB",
        n_region_chunks,
        n_ent_chunks,
        mib(anvil_region),
        mib(anvil_ent)
    );
    println!(
        "import+pack: {:.0} chunks/s over {:.1}s | c-saves: region {:.2} MiB, entities {:.2} MiB",
        (n_region_chunks + n_ent_chunks) as f64 / secs,
        secs,
        mib(rd),
        mib(ed)
    );

    println!("\n-- component weights (raw NBT bytes) --");
    let rows: Vec<(&str, u64)> = vec![
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
    rows.iter().for_each(|(n, b)| {
        let tot = comps.total().max(1);
        println!("CLASS {n:26} {b:>12} ({:5.1}%)", 100.0 * *b as f64 / tot as f64);
    });
    let tot = comps.total().max(1);
    println!(
        "CLASS {:26} {tot:>12} | chunks {} sections {}",
        "TOTAL", comps.n_chunks, comps.n_sections
    );
    println!(
        "CLASS entities                 {:>12} ({} entities)",
        comps.ent_bytes, comps.n_entities
    );

    println!(
        "\nregion : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_region),
        mib(rd),
        anvil_region as f64 / rd.max(1) as f64
    );
    println!(
        "entities: anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_ent),
        mib(ed),
        anvil_ent as f64 / ed.max(1) as f64
    );
    println!(
        "TOTAL  : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_region + anvil_ent),
        mib(rd + ed),
        (anvil_region + anvil_ent) as f64 / (rd + ed).max(1) as f64
    );

    // ---- phase C: streaming semantic verify (every Nth chunk) ----
    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut seen = 0usize;
    for f in &region_files {
        if !f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            continue;
        }
        for (cx, cz, payload) in load_mca(f) {
            seen += 1;
            if sample > 1 && seen % sample != 1 {
                continue;
            }
            match rs.read_chunk(key(cx, cz)) {
                Some(d) => {
                    if chunk_semantic(&payload, &d) {
                        ok += 1;
                    } else {
                        bad += 1;
                        if bad <= 3 {
                            eprintln!("semantic MISMATCH region chunk ({cx},{cz})");
                        }
                    }
                }
                None => {
                    bad += 1;
                    eprintln!("region MISSING chunk ({cx},{cz})");
                }
            }
        }
    }
    for f in &ent_files {
        if !f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            continue;
        }
        for (cx, cz, payload) in load_mca(f) {
            seen += 1;
            if sample > 1 && seen % sample != 1 {
                continue;
            }
            match es.read_chunk(key(cx, cz)) {
                Some(d) => {
                    let va: Value = fastnbt::from_bytes(&payload).unwrap();
                    let vb: Value = fastnbt::from_bytes(&d).unwrap();
                    if va == vb {
                        ok += 1;
                    } else {
                        bad += 1;
                        if bad <= 3 {
                            eprintln!("semantic MISMATCH entity chunk ({cx},{cz})");
                        }
                    }
                }
                None => {
                    bad += 1;
                    eprintln!("entity MISSING chunk ({cx},{cz})");
                }
            }
        }
    }
    println!("verify: ok={ok} bad={bad}");
    if bad > 0 {
        std::process::exit(1);
    }
    println!("profile: {}", rs.profile_json());
    println!("enc-modes: {}", c_saves::chunknbt::enc_mode_stats());
    println!("split stages: {}", c_saves::chunknbt::split_stage_json());
}

fn dir_bytes(dir: &Path) -> u64 {
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

fn chunk_semantic(a: &[u8], b: &[u8]) -> bool {
    let va: Value = fastnbt::from_bytes(a).unwrap();
    let vb: Value = fastnbt::from_bytes(b).unwrap();
    let Value::Compound(ma) = &va else { return va == vb };
    let Value::Compound(mb) = &vb else { return false };
    for (k, v) in ma {
        if k == "sections" {
            let (Value::List(sa), Some(Value::List(sb))) = (v, mb.get("sections")) else {
                return false;
            };
            if sa.len() != sb.len() {
                return false;
            }
            for (s1, s2) in sa.iter().zip(sb.iter()) {
                if !c_saves::chunknbt::sections_semantically_equal(s1, s2) {
                    return false;
                }
            }
        } else if v != mb.get(k).unwrap_or(&Value::Byte(0)) {
            return false;
        }
    }
    true
}
