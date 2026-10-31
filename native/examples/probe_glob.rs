//! probe_glob: bisect the GLOB natural-choice divergence (verify bad=32).
//!
//! Runs the EXACT jj_profile region pipeline (same Cfg, strip, seal, pack
//! drain) and after every compact_step checks:
//!   1. slab_invariant_scan — blobs map vs on-disk slab entry tables;
//!   2. full semantic verify of every chunk (count + first bad keys).
//! The step where violations/mismatches first appear localizes the corrupting
//! pass; the printed details (hash, slab loc, table neighborhood) identify
//! the mechanism.
//!
//! Usage: cargo run --release --example probe_glob -- <dim-dir> [out-dir]

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
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
                let mut d = std::io::Cursor::new(&data[off + 5..off + 4 + len]);
                let mut out = Vec::new();
                if flate2::read::GzDecoder::new(&mut d).read_to_end(&mut out).is_ok() {
                    out
                } else {
                    continue;
                }
            }
            2 => {
                let mut d = std::io::Cursor::new(&data[off + 5..off + 4 + len]);
                let mut out = Vec::new();
                if flate2::read::ZlibDecoder::new(&mut d).read_to_end(&mut out).is_ok() {
                    out
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let idx = i as i32;
        let cx = rx * 32 + (idx & 31);
        let cz = rz * 32 + (idx >> 5);
        out.push((cx, cz, payload));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim =
        PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));
    let out =
        PathBuf::from(args.get(2).cloned().unwrap_or_else(|| "/tmp/probe_glob".to_string()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let region_dir = dim.join("region");
    let mut region_chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            region_chunks.extend(load_mca(f));
        }
    }
    println!("loaded {} region chunks", region_chunks.len());

    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        ..Cfg::default()
    };
    let rs = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
    let es = Store::open_cfg(&out.join("entities"), true, cfg.clone()).unwrap();
    let strip = !matches!(std::env::var("CSAVES_KEEP_LIGHT").as_deref(), Ok("1"));
    // entity chunks from the corpus (whole mode) — exact jj_profile flow
    let entity_dir = dim.join("entities");
    let mut entity_chunks = Vec::new();
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(&entity_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            entity_chunks.extend(load_mca(f));
        }
    }
    for (cx, cz, p) in &region_chunks {
        rs.write_chunk_stripped(key(*cx, *cz), p, strip).unwrap();
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
    es.set_whole_mode();

    // exact jj_profile comparator
    let chunks_semantic = |a: &[u8], b: &[u8]| -> bool {
        let (Ok(mut ma), Ok(Value::Compound(mb))) =
            (fastnbt::from_bytes::<Value>(a), fastnbt::from_bytes::<Value>(b))
        else {
            return false;
        };
        if strip {
            c_saves::chunknbt::strip_derived_chunk(&mut ma);
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

    let mut step: usize = 0;
    // between-step probe: invariant scan ONLY (no reads — reads populate
    // caches and were masking the divergence in the first probe version)
    let scan_only = |step: usize, tag: &str| -> bool {
        let inv = rs.slab_invariant_scan();
        println!("[step {step:03}] {tag}: inv={}", inv.len());
        for line in inv.iter().take(8) {
            println!("    INV: {line}");
        }
        !inv.is_empty()
    };

    let mut ever_broken = scan_only(step, "after import (pre-drain)");
    step += 1;
    rs.request_pack();
    es.request_pack();
    while rs.compact_step() || es.compact_step() {
        let tag = format!("compact_step #{step}");
        let b = scan_only(step, &tag);
        step += 1;
        if b {
            ever_broken = true;
        }
    }
    rs.sync();
    es.sync();
    scan_only(step, "after sync");

    // full verify — exact jj_profile phase V
    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut bad_keys = Vec::new();
    for (cx, cz, p) in &region_chunks {
        match rs.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                if chunks_semantic(p, &d) {
                    ok += 1;
                } else {
                    bad += 1;
                    if bad_keys.len() < 6 {
                        bad_keys.push(format!("({cx},{cz})"));
                    }
                }
            }
            None => {
                bad += 1;
                if bad_keys.len() < 6 {
                    bad_keys.push(format!("({cx},{cz}):NONE {}", rs.debug_blob_state(key(*cx, *cz))));
                }
            }
        }
    }
    println!("verify: ok={ok} bad={bad} first={bad_keys:?}");
    let inv = rs.slab_invariant_scan();
    println!("post-verify inv={}", inv.len());
    for line in inv.iter().take(8) {
        println!("    INV: {line}");
    }
    if bad == 0 && inv.is_empty() && !ever_broken {
        println!("LIVE RUN CLEAN — checking reopen-from-disk");
        drop(rs);
        drop(es);
        let rs2 = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
        let inv2 = rs2.slab_invariant_scan();
        println!("reopen inv={}", inv2.len());
        for line in inv2.iter().take(8) {
            println!("    INV: {line}");
        }
        let mut ok2 = 0usize;
        let mut bad2 = 0usize;
        let mut bk2 = Vec::new();
        for (cx, cz, p) in &region_chunks {
            match rs2.read_chunk(key(*cx, *cz)) {
                Some(d) => {
                    if chunks_semantic(p, &d) {
                        ok2 += 1;
                    } else {
                        bad2 += 1;
                        if bk2.len() < 6 {
                            bk2.push(format!("({cx},{cz})"));
                        }
                    }
                }
                None => {
                    bad2 += 1;
                    if bk2.len() < 6 {
                        bk2.push(format!("({cx},{cz}):NONE {}", rs2.debug_blob_state(key(*cx, *cz))));
                    }
                }
            }
        }
        println!("reopen verify: ok={ok2} bad={bad2} first={bk2:?}");
    }
    println!("drain steps={step}, ever_broken={ever_broken}");
}
