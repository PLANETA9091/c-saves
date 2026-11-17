//! preset_probe: the LZMA2 cold-tier preset ladder priced on the REAL
//! member streams of the drained JJ container.
//!
//! The cold tier pins LZMA2_COLD_PRESET=6|EXTREME (nice_len=64). The
//! classic xz ladder says 9|EXTREME (nice_len=273, deeper BT4) buys another
//! ~1-3% at 2-4x encode wall; 6|e + nice_len=273 is the middle card. The
//! raw-LZMA2 decode only needs a dict >= the encoder's, so a preset bump
//! is wire-safe for old containers (bigger decode dict = superset).
//!
//! Usage: cargo run --release --example preset_probe -- <dim-dir>

use c_saves::store::{Cfg, Store};
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
        let comp_type = data[off + 4];
        let comp = data[off + 5..off + 4 + len].to_vec();
        let payload = match comp_type {
            1 => {
                let mut v = Vec::new();
                if flate2::read::GzDecoder::new(&comp[..]).read_to_end(&mut v).is_err() {
                    continue;
                }
                v
            }
            2 => {
                let mut v = Vec::new();
                if flate2::read::ZlibDecoder::new(&comp[..]).read_to_end(&mut v).is_err() {
                    continue;
                }
                v
            }
            3 => comp.clone(),
            _ => continue,
        };
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

/// Encode with an explicit preset + optional nice_len override (the tuned
/// lc=4/lp=0/pb=0 literal filter rides everything, matching the store).
fn lzma2_enc(data: &[u8], preset: u32, nice_len: Option<u32>) -> Option<(Vec<u8>, std::time::Duration)> {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(
        liblzma::stream::PRESET_EXTREME | preset,
    )
    .ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    if let Some(nl) = nice_len {
        opts.nice_len(nl);
    }
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let t = std::time::Instant::now();
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some((out, t.elapsed()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).expect("usage: preset_probe <dim-dir>"));
    let out_root = PathBuf::from("/tmp/preset-probe");
    let out = out_root.join("container");
    let _ = std::fs::remove_dir_all(&out_root);
    std::fs::create_dir_all(&out).unwrap();

    let mut region_chunks = Vec::new();
    let mut entity_chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        region_chunks.extend(load_mca(f));
    }
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(dim.join("entities"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        entity_chunks.extend(load_mca(f));
    }
    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        ..Cfg::from_env()
    };
    let rs = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
    let es = Store::open_cfg(&out.join("entities"), true, cfg).unwrap();
    es.set_whole_mode();
    for (cx, cz, p) in &region_chunks {
        rs.write_chunk_stripped(key(*cx, *cz), p, true).unwrap();
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
    rs.request_pack();
    es.request_pack();
    while rs.compact_step() || es.compact_step() {}
    rs.sync();
    es.sync();
    println!(
        "drained: {:.2} MiB",
        (rs.disk_bytes() + es.disk_bytes()) as f64 / 1048576.0
    );

    // two real streams: the big glob_g6 pole and the tail_nbt class
    let members = rs.slab_members_by_class();
    drop(rs);
    drop(es);
    let mut by_class: std::collections::BTreeMap<String, Vec<Vec<u8>>> = Default::default();
    for (cls, body) in members {
        by_class.entry(cls).or_default().push(body);
    }
    let mut picked: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(v) = by_class.get("glob_g6") {
        picked.push((
            "glob_g6".into(),
            v.iter().flat_map(|b| b.iter().copied()).collect(),
        ));
    }
    if let Some(v) = by_class.get("tail_nbt") {
        picked.push((
            "tail_nbt".into(),
            v.iter().flat_map(|b| b.iter().copied()).collect(),
        ));
    }

    // ladder: (preset, nice_len override) — 6e is the pinned baseline
    let ladder: Vec<(u32, Option<u32>, &str)> = vec![
        (6, None, "6e (pinned)"),
        (6, Some(273), "6e+nl273"),
        (7, None, "7e"),
        (9, None, "9e"),
    ];
    for (cls, stream) in &picked {
        println!(
            "\n-- {} : {:.2} MiB raw --",
            cls,
            stream.len() as f64 / 1048576.0
        );
        let base = lzma2_enc(stream, 6, None);
        let (base_b, base_t) = base.map(|(b, t)| (b.len(), t)).unwrap();
        println!(
            "{:>12} {:>10} {:>8.2}s  (baseline)",
            "6e (pinned)", base_b, base_t.as_secs_f64()
        );
        for (preset, nl, name) in &ladder[1..] {
            let Some((out, t)) = lzma2_enc(stream, *preset, *nl) else {
                println!("{name:>12} ENCODE FAILED");
                continue;
            };
            let d = 100.0 * (out.len() as f64 - base_b as f64) / base_b as f64;
            println!(
                "{:>12} {:>10} {:>8.2}s  ({:+.2}% vs 6e, wall x{:.1})",
                name,
                out.len(),
                t.as_secs_f64(),
                d,
                t.as_secs_f64() / base_t.as_secs_f64()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&out_root);
}
