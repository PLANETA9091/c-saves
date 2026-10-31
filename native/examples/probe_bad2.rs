//! probe_bad: surgical diff of the GLOB-divergence verify failures.
//!
//! For the first bad chunks: locate the first semantically-different section
//! (by Y), then compare the source vs got block_states palettes and data,
//! plus the stored member bytes (canonical vs 0xCB GLOB form) and a manual
//! glob roundtrip. Pinpoints the exact codec asymmetry.
//!
//! Usage: cargo run --release --example probe_bad2 -- <dim-dir> [out-dir]

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
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
        let payload = match data[off + 4] {
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

fn secs(v: &Value) -> Vec<(i8, Value)> {
    let Value::Compound(c) = v else { return vec![] };
    let Some(Value::List(l)) = c.get("sections") else { return vec![] };
    l.iter()
        .map(|s| {
            let y = match s {
                Value::Compound(sc) => match sc.get("Y") {
                    Some(Value::Byte(y)) => *y,
                    _ => 0,
                },
                _ => 0,
            };
            (y, s.clone())
        })
        .collect()
}

// expanded state vector of block_states (the semantic comparator's core)
fn states_of(sec: &Value) -> Option<Vec<String>> {
    let Value::Compound(c) = sec else { return None };
    let Value::Compound(bs) = c.get("block_states")? else { return None };
    let Value::List(pal) = bs.get("palette")? else { return None };
    let mut out = Vec::new();
    for p in pal {
        out.push(format!("{p:?}"));
    }
    Some(out)
}

fn data_len_of(sec: &Value) -> usize {
    let Value::Compound(c) = sec else { return 0 };
    let Some(Value::Compound(bs)) = c.get("block_states") else { return 0 };
    match bs.get("data") {
        Some(Value::LongArray(la)) => la.len(),
        _ => 0,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));
    let out = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| "/tmp/probe_bad2".to_string()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let mut region_chunks = Vec::new();
    let rdir = if dim.join("region").exists() { dim.join("region") } else { PathBuf::from("../bench-corpus/dim/region") };
    let edir = if dim.join("entities").exists() { dim.join("entities") } else { PathBuf::from("../bench-corpus/dim/entities") };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&rdir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            region_chunks.extend(load_mca(f));
        }
    }
    let mut entity_chunks = Vec::new();
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(&edir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            entity_chunks.extend(load_mca(f));
        }
    }

    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        ..Cfg::default()
    };
    let rs = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
    let es = Store::open_cfg(&out.join("entities"), true, cfg.clone()).unwrap();
    let strip = !matches!(std::env::var("CSAVES_KEEP_LIGHT").as_deref(), Ok("1"));
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
    rs.request_pack();
    es.request_pack();
    while rs.compact_step() || es.compact_step() {}
    rs.sync();
    es.sync();

    // find bad chunks
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

    let mut bad_list = Vec::new();
    for (cx, cz, p) in &region_chunks {
        match rs.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                if !chunks_semantic(p, &d) {
                    bad_list.push((*cx, *cz));
                }
            }
            None => bad_list.push((*cx, *cz)),
        }
        if bad_list.len() >= 2 {
            break;
        }
    }
    println!("first bad chunks: {bad_list:?}");

    for (cx, cz) in bad_list {
        println!("\n===== chunk ({cx},{cz}) =====");
        let Some((_, _, src)) = region_chunks.iter().find(|(x, z, _)| *x == cx && *z == cz)
        else {
            continue;
        };
        let got = rs.read_chunk(key(cx, cz));
        let Some(got) = got else {
            println!("  read_chunk NONE: {}", rs.debug_blob_state(key(cx, cz)));
            continue;
        };
        let (Ok(mut sv), Ok(gv)) =
            (fastnbt::from_bytes::<Value>(src), fastnbt::from_bytes::<Value>(&got))
        else {
            println!("  parse fail");
            continue;
        };
        if strip {
            c_saves::chunknbt::strip_derived_chunk(&mut sv);
        }
        let ss = secs(&sv);
        let gs = secs(&gv);
        println!("  sections: src={} got={}", ss.len(), gs.len());
        for ((sy, ssec), (gy, gsec)) in ss.iter().zip(gs.iter()) {
            if sy != gy {
                println!("  Y MISMATCH src={sy} got={gy}");
                break;
            }
            if c_saves::chunknbt::sections_semantically_equal(ssec, gsec) {
                continue;
            }
            println!("  BAD SECTION Y={sy}");
            let (sp, gp) = (states_of(ssec), states_of(gsec));
            match (&sp, &gp) {
                (Some(sp), Some(gp)) => {
                    println!("    palette: src={} got={}", sp.len(), gp.len());
                    for i in 0..sp.len().max(gp.len()).min(8) {
                        let a = sp.get(i).map(|s| s.as_str()).unwrap_or("-");
                        let b = gp.get(i).map(|s| s.as_str()).unwrap_or("-");
                        let mark = if a == b { " " } else { "*" };
                        println!("      {mark} [{i}] src={a}");
                        println!("      {mark} [{i}] got={b}");
                    }
                }
                _ => println!("    palette: src={sp:?} got={gp:?}"),
            }
            println!("    data longs: src={} got={}", data_len_of(ssec), data_len_of(gsec));
            // storage form of this section's member
            if let Some(hashes) = rs.debug_section_hashes(key(cx, cz)) {
                // sections are stored in Y order in the recipe; find the
                // matching one by decoding each and checking Y
                for (i, h) in hashes.iter().enumerate() {
                    if let Some(bytes) = rs.read_blob_by_hash(*h) {
                        let magic = bytes.first().copied().unwrap_or(0);
                        if magic == 0xC7 || magic == 0xC8 {
                            if let Some(v) = c_saves::chunknbt::section_decode(&bytes) {
                                let y = match &v {
                                    Value::Compound(c) => match c.get("Y") {
                                        Some(Value::Byte(y)) => *y,
                                        _ => 0,
                                    },
                                    _ => 0,
                                };
                                if y as i8 == *sy {
                                    println!(
                                        "    member[{i}] hash={h:016x} magic={magic:#04x} len={} CANONICAL",
                                        bytes.len()
                                    );
                                }
                            }
                        } else {
                            println!(
                                "    member[{i}] hash={h:016x} magic={magic:#04x} len={} (0xCB=GLOB)",
                                bytes.len()
                            );
                        }
                    }
                }
            } else {
                println!("    debug_section_hashes: NONE");
            }
            break; // first bad section is enough
        }
    }
}
