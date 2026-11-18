//! batch_rec_probe: two jobs.
//!
//! 1) The CROSS-RECORD body-duplication question (the last unshipped lever
//!    from the 0.9.102 NEXT list) — VERDICT recorded here: the drain itself
//!    collapses all batch records (the pack move re-queues every entry and
//!    the flush splits by 192 KiB groups with per-group dedup), so the
//!    drained container's cross-record waste is only the group-boundary
//!    effect (~0.3% of the container on the Tectonic corpus). The lever is
//!    MOOT for shipped weight; the live window between drains keeps the
//!    per-round records, but that is transient material the next drain
//!    rewrites anyway.
//!
//! 2) The v0.9.107 dict/weight accounting: the HONEST container weight is
//!    segments + dict files + state files (full_disk_bytes) — disk_bytes
//!    missed the dicts (measured 18.6 KB of a 289.6 KB Tectonic drain).
//!    Prints the tail census (inline/blob kinds + dict-version references)
//!    and the A/B of CSAVES_TAIL_BLOB_MIN (512 base / 64 / 0): tails below
//!    the threshold ride recipes as per-tail zstd+dict INLINE bytes — they
//!    miss the tail slab stream's shared window AND pin the dict file into
//!    the shipped container.
//!
//! Usage: cargo run --release --example batch_rec_probe [-- <cx> <cz>] [rounds]

#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::chunknbt::hash_of;
use c_saves::store::{Cfg, Store};
use flate2::read::GzDecoder;
use flate2::read::ZlibDecoder;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

struct RecStats {
    records: usize,
    entries: usize,
    uniq_slots: usize,
    body_raw: u64,
    frame_bytes: u64,
    stream_raw: u64,
    wasted: u64,
    global_uniq: usize,
}

fn scan_records(dir: &Path) -> RecStats {
    let mut records = 0usize;
    let mut sum_n = 0usize;
    let mut sum_m = 0usize;
    let mut body_raw = 0u64;
    let mut frame_bytes = 0u64;
    let mut stream_raw = 0u64;
    let mut global: Map<u128, u64> = Map::new();
    let mut wasted = 0u64;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with("seg-") && s.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    for p in paths {
        let data = std::fs::read(&p).unwrap();
        let mut q = 0usize;
        while q + 8 <= data.len() {
            let bl = u32::from_le_bytes(data[q..q + 4].try_into().unwrap()) as usize;
            let body = &data[q + 8..(q + 8 + bl).min(data.len())];
            if body.first() == Some(&0x0B) && body.len() >= 17 {
                let n = u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize;
                let m = u32::from_le_bytes(body[5..9].try_into().unwrap()) as usize;
                let cl = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
                let raw_total = u32::from_le_bytes(body[13..17].try_into().unwrap()) as usize;
                let comp = &body[17..17 + cl];
                let mut d = zstd::Decoder::new(comp).unwrap();
                let mut raw = Vec::with_capacity(raw_total);
                std::io::Read::read_to_end(&mut d, &mut raw).unwrap();
                assert_eq!(raw.len(), raw_total, "record stream length");
                let mut lens = Vec::with_capacity(m);
                let mut p2 = 8 * n + 4 * n;
                for _ in 0..m {
                    lens.push(u16::from_le_bytes(raw[p2..p2 + 2].try_into().unwrap()) as usize);
                    p2 += 2;
                }
                let base = p2;
                records += 1;
                sum_n += n;
                sum_m += m;
                stream_raw += raw_total as u64;
                frame_bytes += (8 + 17 + cl) as u64;
                let mut off = base;
                for l in &lens {
                    let b = &raw[off..off + *l];
                    let h = hash_of(b);
                    match global.get(&h) {
                        Some(_) => wasted += *l as u64,
                        None => {
                            global.insert(h, *l as u64);
                        }
                    }
                    off += *l;
                }
                body_raw += (off - base) as u64;
            }
            q += 8 + bl;
        }
    }
    RecStats {
        records,
        entries: sum_n,
        uniq_slots: sum_m,
        body_raw,
        frame_bytes,
        stream_raw,
        wasted,
        global_uniq: global.len(),
    }
}

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

fn dir_bytes(dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        let mut total = 0u64;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    total += walk(&path);
                } else {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        total
    }
    walk(dir)
}

/// One import+drain leg: returns (segments, full dir bytes after close).
/// live=true drives write_chunk (the production save path, no pos-strip —
/// the CI bench shape); live=false drives write_chunk_import (the Anvil
/// drain path, pos-free tails).
fn run_leg(
    tag: &str,
    dir: &Path,
    payloads: &[(u64, Vec<u8>)],
    keys: &[u64],
    by_key: &Map<u64, usize>,
    cfg: Cfg,
    rounds: usize,
    live: bool,
) -> (u64, u64) {
    let _ = std::fs::remove_dir_all(dir);
    let st = Store::open_cfg(dir, true, cfg).unwrap();
    let t0 = std::time::Instant::now();
    if rounds <= 1 {
        for &k in keys {
            let p = &payloads[by_key[&k]].1;
            if live {
                st.write_chunk(k, p, false).unwrap();
            } else {
                st.write_chunk_import(k, p).unwrap();
            }
        }
    } else {
        let per = keys.len().div_ceil(rounds);
        for chunk in keys.chunks(per) {
            for &k in chunk {
                let p = &payloads[by_key[&k]].1;
                if live {
                    st.write_chunk(k, p, false).unwrap();
                } else {
                    st.write_chunk_import(k, p).unwrap();
                }
            }
            st.sync();
        }
    }
    let import_t = t0.elapsed();
    drain(&st);
    let segs = st.disk_bytes();
    let full_open = st.full_disk_bytes();
    let census = st.tail_census_json();
    println!("[{tag}] import {:.1?} | segs {} | full(open) {} | dir(open) {}",
        import_t, segs, full_open, dir_bytes(dir));
    println!("[{tag}] census {}", census);
    let (rs, _, _, _, _, _, wasted) = {
        let s = scan_records(dir);
        (s.records, s.entries, s.uniq_slots, s.body_raw, s.frame_bytes, s.stream_raw, s.wasted)
    };
    drop(st);
    let full_closed = dir_bytes(dir);
    println!(
        "[{tag}] records {} | cross-record body waste {} B | DIR(closed) {} B",
        rs, wasted, full_closed
    );
    (segs, full_closed)
}

fn main() {
    let cx_w: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(96);
    let cz_w: i32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(96);
    let rounds: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(64);
    let out = PathBuf::from(
        std::env::var("CSAVES_TECTONIC_OUT").unwrap_or_else(|_| "/tmp/tectonic_world".to_string()),
    );
    let corpus_dir = PathBuf::from(
        std::env::var("CSAVES_JJ_CORPUS")
            .unwrap_or_else(|_| "../bench-corpus/dim/region".to_string()),
    );

    // ---------- Tectonic corpus ----------
    let mut b = tc::Builder::new(cx_w);
    let mut payloads: Vec<(u64, Vec<u8>)> = Vec::with_capacity((cx_w * cz_w) as usize);
    for cz in 0..cz_w {
        for cx in 0..cx_w {
            let (p, _) = b.payload(cx, cz);
            payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p));
        }
    }
    let mut keys: Vec<u64> = payloads.iter().map(|(k, _)| *k).collect();
    keys.sort();
    let by_key: Map<u64, usize> =
        payloads.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect();
    let n = payloads.len();

    println!("== Tectonic {}x{} = {} chunks; cross-record verdict ==", cx_w, cz_w, n);
    let (segs_a, full_a) = run_leg(
        "tect 512",
        &out.join("store_batch_a"),
        &payloads,
        &keys,
        &by_key,
        Cfg::from_env(),
        1,
        false,
    );
    let (_, _) = run_leg(
        "tect rounds",
        &out.join("store_batch_b"),
        &payloads,
        &keys,
        &by_key,
        Cfg::from_env(),
        rounds,
        false,
    );
    let (_, _) = run_leg(
        "tect live",
        &out.join("store_batch_live"),
        &payloads,
        &keys,
        &by_key,
        Cfg::from_env(),
        1,
        true,
    );

    // ---------- tail_blob_min A/B (Tectonic) ----------
    println!("== tail_blob_min A/B (Tectonic) ==");
    for min in [64usize, 0usize] {
        let mut cfg = Cfg::from_env();
        cfg.tail_blob_min = min;
        run_leg(
            &format!("tect tbmin={min}"),
            &out.join(format!("store_tbmin_{min}")),
            &payloads,
            &keys,
            &by_key,
            cfg,
            1,
            false,
        );
    }
    println!("tect baseline: segs {segs_a}, full {full_a}");

    // ---------- JJ corpus (real survival data) ----------
    let jj = load_region_dir(&corpus_dir);
    if jj.is_empty() {
        println!("== JJ corpus not found at {} — skipped ==", corpus_dir.display());
        return;
    }
    let mut jj_keys: Vec<u64> = jj.iter().map(|(k, _)| *k).collect();
    jj_keys.sort();
    let jj_by: Map<u64, usize> =
        jj.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect();
    println!("== JJ region corpus = {} chunks ==", jj.len());
    let (segs_j, full_j) = run_leg(
        "jj live",
        &out.join("store_jj_live"),
        &jj,
        &jj_keys,
        &jj_by,
        Cfg::from_env(),
        1,
        true,
    );
    let (_, _) = run_leg(
        "jj import",
        &out.join("store_jj_a"),
        &jj,
        &jj_keys,
        &jj_by,
        Cfg::from_env(),
        1,
        false,
    );
    println!("== tail_blob_min A/B (JJ) ==");
    for min in [64usize, 0usize] {
        let mut cfg = Cfg::from_env();
        cfg.tail_blob_min = min;
        run_leg(
            &format!("jj tbmin={min}"),
            &out.join(format!("store_jj_{min}")),
            &jj,
            &jj_keys,
            &jj_by,
            cfg,
            1,
            true,
        );
    }
    println!("jj baseline: segs {segs_j}, full {full_j}");
}

// -------------------------------------------------- JJ region loader

fn load_region_dir(dir: &Path) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        if !f.extension().map(|e| e == "mca").unwrap_or(false) {
            continue;
        }
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(rx) = parts[1].parse::<i32>() else { continue };
        let Ok(rz) = parts[2].parse::<i32>() else { continue };
        let Ok(data) = std::fs::read(&f) else { continue };
        if data.len() < 8192 {
            continue;
        }
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
            let len =
                u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    as usize;
            if len < 2 || off + 4 + len > data.len() {
                continue;
            }
            let comp_type = data[off + 4];
            let comp = &data[off + 5..off + 4 + len];
            let payload = match comp_type {
                1 => {
                    let mut v = Vec::new();
                    if GzDecoder::new(comp).read_to_end(&mut v).is_err() {
                        continue;
                    }
                    v
                }
                2 => {
                    let mut v = Vec::new();
                    if ZlibDecoder::new(comp).read_to_end(&mut v).is_err() {
                        continue;
                    }
                    v
                }
                3 => comp.to_vec(),
                _ => continue,
            };
            let cx = rx * 32 + (i as i32 & 31);
            let cz = rz * 32 + (i as i32 >> 5);
            out.push((((cx as u64) << 32) | (cz as u64 & 0xffff_ffff), payload));
        }
    }
    out
}
