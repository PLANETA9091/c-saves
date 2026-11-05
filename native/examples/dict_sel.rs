//! dict_sel: offline study — which sample-selection strategy recovers a
//! healthy shared dictionary from the FULL corpus sample stream?
//!
//! Reads the dump produced by CSAVES_DICT_DUMP (u32 len + bytes, repeated),
//! then trains dictionaries with several selections and reports sizes:
//!   first200     — the old arrival-order behavior (pre-0.9.30)
//!   largest200   — the 0.9.30 size-preferring behavior
//!   rand200_<s>  — pseudo-random 200 (3 seeds)
//!   stride200    — evenly spaced over hash-sorted pool (content spread)
//!   unique-first200 — exact-dup-removed first 200
//!   all-up-to800 — big training set (trainer decides)

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;

fn load(path: &str) -> Vec<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path).unwrap().read_to_end(&mut buf).unwrap();
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 4 <= buf.len() {
        let n = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        if p + n > buf.len() { break; }
        out.push(buf[p..p + n].to_vec());
        p += n;
    }
    out
}

fn h64(v: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn train(s: &[Vec<u8>], tag: &str, dict_max: usize) {
    let slices: Vec<&[u8]> = s.iter().map(|v| v.as_slice()).collect();
    match zstd::dict::from_samples(&slices, dict_max) {
        Ok(d) => println!("{tag:<18} n={:<4} dict={} B", s.len(), d.len()),
        Err(e) => println!("{tag:<18} n={:<4} ERR {e}", s.len()),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dict_sel <dump>");
    let dict_max: usize = std::env::args()
        .nth(2).and_then(|v| v.parse().ok()).unwrap_or(112 * 1024);
    let all = load(&path);
    let total: usize = all.iter().map(|v| v.len()).sum();
    println!("stream: {} samples, {} B total", all.len(), total);

    if all.len() >= 200 {
        train(&all[..200], "first200(old)", dict_max);
        let mut by_len = all.clone();
        by_len.sort_by(|a, b| b.len().cmp(&a.len()).then(b.cmp(a)));
        train(&by_len[..200], "largest200(new)", dict_max);
        for seed in [1u64, 42, 7777] {
            let mut idx: Vec<usize> = (0..all.len()).collect();
            let mut x = seed | 1;
            // Fisher-Yates with xorshift
            for i in (1..idx.len()).rev() {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                let j = (x as usize) % (i + 1);
                idx.swap(i, j);
            }
            let sel: Vec<Vec<u8>> = idx.iter().take(200).map(|&i| all[i].clone()).collect();
            train(&sel, &format!("rand200(seed{seed})"), dict_max);
        }
        let mut hsorted: Vec<&Vec<u8>> = all.iter().collect();
        hsorted.sort_by_key(|v| h64(v));
        let stride = hsorted.len() / 200;
        let sel: Vec<Vec<u8>> = hsorted
            .iter().step_by(stride.max(1)).take(200)
            .map(|v| (*v).clone()).collect();
        train(&sel, "stride200(hashespr)", dict_max);

        // exact-dup-removed first 200
        let mut seen = std::collections::HashSet::new();
        let uniq: Vec<&Vec<u8>> = all.iter().filter(|v| seen.insert(h64(v))).collect();
        let sel: Vec<Vec<u8>> = uniq.iter().take(200).map(|v| (*v).clone()).collect();
        train(&sel, "uniqfirst200", dict_max);
        println!("(unique in stream: {}/{}))", uniq.len(), all.len());

        let n800 = all.len().min(800);
        train(&all[..n800], &format!("first{n800}(big)"), dict_max);
    }

    // ---- deterministic content-spread strategies ----
    if all.len() >= 200 {
        // bucket200: one per h64 % 200 bucket, largest in bucket
        let mut buckets: std::collections::HashMap<u64, &Vec<u8>> =
            std::collections::HashMap::new();
        for v in &all {
            let b = h64(v) % 200;
            buckets.entry(b)
                .and_modify(|cur| if v.len() > cur.len() { *cur = v })
                .or_insert(v);
        }
        let mut sel: Vec<Vec<u8>> = buckets.values().map(|v| (*v).clone()).collect();
        sel.sort_by(|a, b| b.len().cmp(&a.len()));
        train(&sel, "bucket200", dict_max);

        // all samples in hash order (trainer decides)
        let mut hall: Vec<&Vec<u8>> = all.iter().collect();
        hall.sort_by_key(|v| h64(v));
        let selall: Vec<Vec<u8>> = hall.into_iter().cloned().collect();
        train(&selall, "all-hashorder", dict_max);

        // ---- REAL utility test: compress the corpus material itself ----
        // material = all samples concatenated in hash order (the corpus's
        // own slab-frame-like payload stream, ~3.9 MB)
        let mut mat: Vec<u8> = Vec::new();
        for v in &selall {
            mat.extend_from_slice(v);
        }
        // use the dict from rand200(seed7777) — the biggest found — and
        // from first200 — the old-style — and none
        let mk = |s: &[Vec<u8>]| {
            let sl: Vec<&[u8]> = s.iter().map(|v| v.as_slice()).collect();
            zstd::dict::from_samples(&sl, dict_max).ok()
        };
        let d_big = mk(&{
            let mut idx: Vec<usize> = (0..all.len()).collect();
            let mut x = 7777u64 | 1;
            for i in (1..idx.len()).rev() {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                let j = (x as usize) % (i + 1);
                idx.swap(i, j);
            }
            idx.iter().take(200).map(|&i| all[i].clone()).collect::<Vec<_>>()
        });
        let d_first = mk(&all[..200]);
        let mut buckets2: std::collections::HashMap<u64, &Vec<u8>> =
            std::collections::HashMap::new();
        for v in &all {
            let b = h64(v) % 200;
            buckets2.entry(b)
                .and_modify(|cur| if v.len() > cur.len() { *cur = v })
                .or_insert(v);
        }
        let mut db: Vec<Vec<u8>> = buckets2.values().map(|v| (*v).clone()).collect();
        db.sort_by(|a, b| b.len().cmp(&a.len()));
        let d_buck = mk(&db);
        for (tag, d) in [
            ("no-dict", None),
            ("dict-first200", d_first.as_ref()),
            ("dict-rand7777", d_big.as_ref()),
            ("dict-bucket200", d_buck.as_ref()),
        ] {
            let mut c = zstd::bulk::Compressor::with_dictionary(6, match d {
                Some(dd) => dd, None => &[],
            }).unwrap();
            let out = c.compress(&mat).unwrap();
            println!("utility {tag:<16} mat={} -> {} B ({:.1}%)",
                mat.len(), out.len(), out.len() as f64 / mat.len() as f64 * 100.0);
        }
    }
}
