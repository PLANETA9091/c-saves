//! bench_size: max-compression size benchmark for c-saves vs vanilla Anvil.
//!
//! World model: Tectonic-like terrain — fBm value-noise heightmap with
//! continentalness (ocean/land), 24 sections per chunk, packed block-state
//! data (no-straddle 1.16+ packing), ore-noise clusters, per-section biomes,
//! 4 heightmaps, SkyLight nibble arrays, block entities, blending_data,
//! PostProcessing, plus entity payloads (whole-mode store).
//!
//! Phases:
//!   A fresh world   — write N chunks + entities, measure container
//!   baseline        — same payloads through vanilla Anvil (zlib, sectors)
//!   B edits         — R rounds of localized rewrites (player edits)
//!   C compaction    — background compact until done, measure container
//!   D verification  — semantic NBT equality on a deterministic sample
//!
//! Usage: cargo run --release --example bench_size [grid] [rounds]

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::collections::HashSet;
use std::io::Write;
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;
use wgen::*;

// ---------------------------------------------------------------- anvil ----

fn anvil_bytes(payloads: &[(i32, i32, Vec<u8>)]) -> u64 {
    // vanilla layout with 4KiB sector padding, zlib-6
    let mut regions: Map<(i32, i32), Vec<u32>> = Map::new();
    for (cx, cz, p) in payloads.iter() {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
        z.write_all(p).unwrap();
        let comp = z.finish().unwrap();
        let entry_len = 5 + comp.len();
        let sectors = ((entry_len + 4095) / 4096) as u32;
        let idx = ((cx.rem_euclid(32)) + (cz.rem_euclid(32)) * 32) as usize;
        regions
            .entry((cx.div_euclid(32), cz.div_euclid(32)))
            .or_default()
            .push(sectors);
    }
    let mut total = 0u64;
    for (_r, ents) in regions {
        total += 8192; // location + timestamp headers
        for sectors in ents {
            total += sectors as u64 * 4096;
        }
    }
    total
}

// ----------------------------------------------------------------- bench ---

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn semantic_eq(a: &[u8], b: &[u8]) -> bool {
    let va: Value = fastnbt::from_bytes(a).unwrap();
    let vb: Value = fastnbt::from_bytes(b).unwrap();
    va == vb
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let grid = args.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(96);
    let rounds = args.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(5);

    let n = (grid * grid) as usize;
    let w = World::new(0xC0FFEE);
    let dir = std::env::temp_dir().join(format!(
        "csaves-bench-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let half = (grid as i32) / 2;
    let coords: Vec<(i32, i32)> = (-half..half)
        .flat_map(|cx| (-half..half).map(move |cz| (cx, cz)))
        .collect();
    assert_eq!(coords.len(), n);

    println!("=== c-saves size benchmark: {n} chunks, {rounds} edit rounds ===");

    // ---- Phase A: fresh world ----
    let cfg = Cfg { compact_min: 256 * 1024, compact_ratio: 0.10, dict_train: 400, ..Cfg::default() };
    let region_store = Store::open_cfg(&dir.join("region"), true, cfg.clone()).unwrap();
    let entity_store = Store::open_cfg(&dir.join("entities"), true, cfg.clone()).unwrap();
    let t0 = Instant::now();
    for &(cx, cz) in &coords {
        let p = chunk_payload(&w, cx, cz, 0);
        region_store.write_chunk(key(cx, cz), &p, false).unwrap();
        let e = entity_payload(cx, cz, 0);
        entity_store.write_chunk(key(cx, cz), &e, true).unwrap();
    }
    let write_secs = t0.elapsed().as_secs_f64();
    region_store.sync();
    entity_store.sync();
    region_store.train_dict_if_needed();
    entity_store.train_dict_if_needed();

    let logical_region = region_store.logical();
    let disk_fresh = region_store.disk_bytes();
    let disk_ent_fresh = entity_store.disk_bytes();
    let logical_ent = entity_store.logical();

    println!(
        "A write: {write_secs:.1} s ({:.0} chunks/s) | region logical {:.1} MiB -> disk {:.1} MiB (x{:.2}) | entities disk {:.1} MiB",
        n as f64 / write_secs,
        logical_region as f64 / 1048576.0,
        disk_fresh as f64 / 1048576.0,
        logical_region as f64 / disk_fresh.max(1) as f64,
        disk_ent_fresh as f64 / 1048576.0,
    );

    // ---- Anvil baseline (fresh payloads) ----
    let mut all = Vec::with_capacity(n);
    for &(cx, cz) in &coords {
        all.push((cx, cz, chunk_payload(&w, cx, cz, 0)));
    }
    let anvil = anvil_bytes(&all);
    let logical: u64 = all.iter().map(|(_, _, p)| p.len() as u64).sum();
    println!(
        "baseline: vanilla anvil (zlib-6, sectors) {:.1} MiB (x{:.2} vs logical) | entities logical {:.1} MiB",
        anvil as f64 / 1048576.0,
        logical as f64 / anvil.max(1) as f64,
        logical_ent as f64 / 1048576.0,
    );
    drop(all);

    // ---- Phase B: edit rounds ----
    let mut edited: Map<u64, u64> = Map::new(); // key -> latest round
    let mut edited_ent: Map<u64, u64> = Map::new();
    let per_round = n / 8; // 12.5% of chunks per round
    let t1 = Instant::now();
    for round in 1..=rounds as u64 {
        for i in 0..per_round {
            let ci = (splitmix((round as u64) << 40 ^ i as u64) as usize) % n;
            let (cx, cz) = coords[ci];
            let p = chunk_payload(&w, cx, cz, round);
            region_store.write_chunk(key(cx, cz), &p, false).unwrap();
            edited.insert(key(cx, cz), round);
            if ci % 4 == 0 {
                let e = entity_payload(cx, cz, round);
                entity_store.write_chunk(key(cx, cz), &e, true).unwrap();
                edited_ent.insert(key(cx, cz), round);
            }
        }
    }
    let edit_secs = t1.elapsed().as_secs_f64();
    region_store.sync();
    entity_store.sync();
    region_store.train_dict_if_needed();
    entity_store.train_dict_if_needed();
    region_store.seal();
    entity_store.seal();
    let disk_edited = region_store.disk_bytes();
    let disk_ent_edited = entity_store.disk_bytes();
    println!(
        "B edits: {edit_secs:.1} s | region disk after {} edits {:.1} MiB (bloat x{:.2} vs fresh) | entities {:.1} MiB",
        per_round * rounds as usize,
        disk_edited as f64 / 1048576.0,
        disk_edited as f64 / disk_fresh.max(1) as f64,
        disk_ent_edited as f64 / 1048576.0,
    );

    // ---- Phase C: compaction ----
    let t2 = Instant::now();
    let mut steps = 0;
    while steps < 500 {
        let a = region_store.compact_step();
        let b = entity_store.compact_step();
        if a { eprintln!("[bench] region compacted (step {})", steps + 1); }
        if b { eprintln!("[bench] entity compacted (step {})", steps + 1); }
        if !a && !b {
            break;
        }
        steps += a as usize + b as usize;
    }
    let compact_secs = t2.elapsed().as_secs_f64();
    let disk_final = region_store.disk_bytes();
    let disk_ent_final = entity_store.disk_bytes();
    let total_final = disk_final + disk_ent_final;
    println!(
        "C compaction: {compact_secs:.1} s, {steps} steps | region disk {:.1} MiB (x{:.2} vs logical) | entities {:.1} MiB",
        disk_final as f64 / 1048576.0,
        disk_final as f64 / logical_region.max(1) as f64,
        disk_ent_final as f64 / 1048576.0,
    );

    // ---- Phase D: verification (semantic) ----
    let t3 = Instant::now();
    let mut verified = 0;
    for (i, &(cx, cz)) in coords.iter().enumerate() {
        if i % 61 != 0 && !edited.contains_key(&key(cx, cz)) {
            continue;
        }
        let round = *edited.get(&key(cx, cz)).unwrap_or(&0);
        let want = chunk_payload(&w, cx, cz, round);
        let got = region_store.read_chunk(key(cx, cz)).expect("chunk present");
        assert!(semantic_eq(&got, &want), "chunk ({cx},{cz}) mismatch");
        let want_e = entity_payload(cx, cz, *edited_ent.get(&key(cx, cz)).unwrap_or(&0));
        let got_e = match entity_store.read_chunk(key(cx, cz)) {
            Some(g) => g,
            None => {
                eprintln!("MISSING entity chunk ({cx},{cz}) k={:016x} stats={}", key(cx, cz), entity_store.stats_json());
                panic!("entity present ({cx},{cz})");
            }
        };
        assert!(semantic_eq(&got_e, &want_e), "entity chunk ({cx},{cz}) mismatch");
        verified += 1;
    }
    let verify_secs = t3.elapsed().as_secs_f64();
    println!("D verify: {verified} chunks semantic-equal in {verify_secs:.2} s");

    // ---- reopen persistence check ----
    let rs2 = Store::open_cfg(&dir.join("region"), true, cfg.clone()).unwrap();
    let es2 = Store::open_cfg(&dir.join("entities"), true, cfg.clone()).unwrap();
    assert_eq!(rs2.chunk_count(), n, "region chunks survive reopen");
    assert_eq!(es2.chunk_count(), n, "entity chunks survive reopen");
    let sample = coords[n / 2];
    let got = rs2.read_chunk(key(sample.0, sample.1)).expect("read after reopen");
    let want = chunk_payload(&w, sample.0, sample.1, 0);
    assert!(semantic_eq(&got, &want), "reopen semantic equality");

    // ---- summary ----
    let total_logical = logical_region + logical_ent;
    let disk_full = disk_fresh + disk_ent_fresh;
    println!("=== summary ===");
    println!("logical          {:>10.2} MiB", total_logical as f64 / 1048576.0);
    println!(
        "anvil (zlib-6)   {:>10.2} MiB   (x{:.2} vs logical)",
        anvil as f64 / 1048576.0,
        total_logical as f64 / anvil.max(1) as f64
    );
    println!(
        "c-saves fresh    {:>10.2} MiB   (x{:.2} vs logical, x{:.2} vs anvil)",
        disk_full as f64 / 1048576.0,
        total_logical as f64 / disk_full.max(1) as f64,
        disk_full as f64 / anvil.max(1) as f64
    );
    println!(
        "c-saves compact  {:>10.2} MiB   (x{:.2} vs logical, x{:.2} vs anvil)",
        total_final as f64 / 1048576.0,
        total_logical as f64 / total_final.max(1) as f64,
        total_final as f64 / anvil.max(1) as f64
    );
    println!("stats region: {}", rs2.stats_json());
    println!("profile region: {}", rs2.profile_json());
    println!("profile entities: {}", es2.profile_json());
    let _ = std::fs::remove_dir_all(&dir);
}
