//! probe_skip — minimal repro: import corpus once, sync+seal, reopen,
//! read every key back and compare. Isolates whether the SKIP path
//! corrupts a plain write-once-import (or whether import+reopen was
//! already broken and full rewrites used to mask it).

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use flate2::read::ZlibDecoder;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::Path;

fn load_mca(path: &Path) -> Vec<(usize, Vec<u8>)> {
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
        let payload = match data[off + 4] {
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
        out.push((i, payload));
    }
    out
}

fn semantically_equal(a: &[u8], b: &[u8]) -> bool {
    let (Ok(Value::Compound(ma)), Ok(Value::Compound(mb))) = (
        fastnbt::from_bytes::<Value>(a),
        fastnbt::from_bytes::<Value>(b),
    ) else {
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
}

fn first_semantic_diff(a: &[u8], b: &[u8]) -> String {
    let (Ok(Value::Compound(ma)), Ok(Value::Compound(mb))) = (
        fastnbt::from_bytes::<Value>(a),
        fastnbt::from_bytes::<Value>(b),
    ) else {
        return "parse fail".into();
    };
    for (k, va) in &ma {
        let Some(vb) = mb.get(k) else {
            return format!("missing key {k}");
        };
        if k == "sections" {
            let (Value::List(la), Value::List(lb)) = (va, vb) else {
                return format!("sections shape {k}");
            };
            for (i, (sa, sb)) in la.iter().zip(lb.iter()).enumerate() {
                if !c_saves::chunknbt::sections_semantically_equal(sa, sb) {
                    let (Value::Compound(x), Value::Compound(y)) = (sa, sb) else {
                        return format!("section {i} shape");
                    };
                    let xk: Vec<&String> = x.keys().collect();
                    let yk: Vec<&String> = y.keys().collect();
                    return format!("section {i}: src {xk:?} got {yk:?}");
                }
            }
        } else if va != vb {
            return format!("key {k}: src {va:?} got {vb:?}");
        }
    }
    "none".into()
}

fn main() {
    let region = std::env::var("PROBE_ENT").as_deref() != Ok("1");
    let base = if region {
        "../bench-corpus/dim/region"
    } else {
        "../bench-corpus/dim/entities"
    };
    let out = std::env::temp_dir().join(format!("probe-skip-{}-{}", std::process::id(), region));
    let _ = std::fs::remove_dir_all(&out);
    let st = Store::open_cfg(&out, true, Cfg::default()).unwrap();
    if !region {
        st.set_whole_mode();
    }
    let mut corpus: Map<u64, Vec<u8>> = Map::new();
    for f in std::fs::read_dir(base).unwrap().flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        if !name.ends_with(".mca") {
            continue;
        }
        let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
        let (rx, rz): (i32, i32) = (parts[1].parse().unwrap(), parts[2].parse().unwrap());
        for (slot, payload) in load_mca(&f.path()) {
            let cx = rx * 32 + (slot & 31) as i32;
            let cz = rz * 32 + (slot >> 5) as i32;
            let k = ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff);
            corpus.insert(k, payload);
        }
    }
    let n = corpus.len();
    for (k, p) in &corpus {
        st.write_chunk(*k, p, !region).unwrap();
    }
    st.sync();
    st.seal();
    // phase 1: verify with the SAME handle
    let mut ok = 0;
    let mut bad = 0;
    for (k, p) in &corpus {
        match st.read_chunk(*k) {
            Some(d) => {
                if semantically_equal(p, &d) {
                    ok += 1;
                } else {
                    bad += 1;
                }
            }
            None => bad += 1,
        }
    }
    println!("same-handle: ok={ok} bad={bad} of {n}");
    // phase P: pack pass (speed_bench D model)
    st.request_pack();
    loop {
        if !st.compact_step() {
            break;
        }
    }
    st.sync();
    let mut pok = 0;
    let mut pbad = 0;
    let mut first_pack_bad: Option<u64> = None;
    for (k, p) in &corpus {
        match st.read_chunk(*k) {
            Some(d) => {
                if semantically_equal(p, &d) {
                    pok += 1;
                } else {
                    pbad += 1;
                    if first_pack_bad.is_none() {
                        first_pack_bad = Some(*k);
                    }
                }
            }
            None => pbad += 1,
        }
    }
    println!("post-pack:   ok={pok} bad={pbad} of {n}");
    if let Some(k) = first_pack_bad {
        let p = &corpus[&k];
        if let Some(d) = st.read_chunk(k) {
            println!("  pack-diff: {}", first_semantic_diff(p, &d));
        } else {
            println!("  pack-diff: key MISSING");
        }
    }
    // phase REOPEN (speed_bench E model): fresh handles BEFORE the rewrite
    // rounds — scan() rebuilds recipes + refcounts from disk
    drop(st);
    let st = Store::open_cfg(&out, true, Cfg::default()).unwrap();
    if !region {
        st.set_whole_mode();
    }
    // phase R: cold read round (speed_bench E model) + MT reads (F model)
    {
        let mut ok = 0;
        let mut bad = 0;
        for (k, p) in &corpus {
            match st.read_chunk(*k) {
                Some(d) => if semantically_equal(p, &d) { ok += 1 } else { bad += 1 },
                None => bad += 1,
            }
        }
        println!("cold-read:  ok={ok} bad={bad}");
    }
    {
        let corpus2 = std::sync::Arc::new(corpus.clone());
        let st2 = std::sync::Arc::new(st.clone());
        let mut hs = Vec::new();
        for _t in 0..2 {
            let st2 = st2.clone();
            let corpus2 = corpus2.clone();
            hs.push(std::thread::spawn(move || {
                for (k, p) in corpus2.iter() {
                    let _ = st2.read_chunk(*k);
                    let _ = p;
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        println!("mt-reads done");
    }
    // phase 1.5: 3 rewrite rounds (speed_bench G model)
    let skip_hits = std::env::var("PROBE_NOSKIP").as_deref() != Ok("1");
    let _ = skip_hits;
    for round in 0..3 {
        for (k, p) in &corpus {
            st.write_chunk(*k, p, !region).unwrap();
        }
        // integrity after every round
        let mut intact = 0;
        let mut broken = 0;
        for k in corpus.keys() {
            if st.chunk_intact(*k) { intact += 1 } else { broken += 1 }
        }
        println!("round {round}: intact={intact} broken={broken}");
        if broken > 0 { break; }
    }
    st.sync();
    // phase 2: reopen and verify again
    drop(st);
    let st2 = Store::open_cfg(&out, true, Cfg::default()).unwrap();
    if !region {
        st2.set_whole_mode();
    }
    let mut ok2 = 0;
    let mut bad2 = 0;
    let mut first_bad: Option<u64> = None;
    for (k, p) in &corpus {
        match st2.read_chunk(*k) {
            Some(d) => {
                if semantically_equal(p, &d) {
                    ok2 += 1;
                } else {
                    bad2 += 1;
                    if first_bad.is_none() {
                        first_bad = Some(*k);
                    }
                }
            }
            None => bad2 += 1,
        }
    }
    println!("reopened:    ok={ok2} bad={bad2} of {n}");
    if let Some(k) = first_bad {
        let p = &corpus[&k];
        if let Some(d) = st2.read_chunk(k) {
            println!("sample bad key {k}: src_len={} got_len={}", p.len(), d.len());
            println!("  diff: {}", first_semantic_diff(p, &d));
        } else {
            println!("sample bad key {k}: MISSING");
        }
    }
    let _ = std::fs::remove_dir_all(&out);
}
