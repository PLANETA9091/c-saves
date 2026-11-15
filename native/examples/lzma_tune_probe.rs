//! LZMA2 filter tuning on the REAL post-dedup member stream: same corpus
//! pipeline as lzma_preset_probe (first-occurrence order, structural dedup,
//! 8 MiB slab groups, 24 B entry rows), then an lc/lp/pb + delta-filter
//! ladder at the shipped preset (6|EXTREME). The shipped chain is
//! `lzma2(preset 6|E)` (lc=3 lp=0 pb=2 implicit); every candidate here must
//! beat that to earn a new raw codec stamp (the raw stream carries no props
//! — the stamp pins the exact filter chain on both ends).
//!
//! Usage: lzma_tune_probe <dim-dir>

use c_saves::chunknbt::{self, SecBytes, Split};
use liblzma::stream::{Filters, LzmaOptions, Stream, PRESET_EXTREME};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

type Corpus = Vec<(i32, i32, Vec<u8>)>;

fn load_mca(path: &Path) -> Corpus {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else {
        return vec![];
    };
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

/// Encode with an explicit filter chain: optional delta(dist) then lzma2
/// with explicit (lc, lp, pb, dict, nice_len). Mirrors what a new codec
/// stamp would pin on both ends.
fn lzma2_tuned(
    data: &[u8],
    lc: u32,
    lp: u32,
    pb: u32,
    dict: u32,
    nice: u32,
    delta: Option<u32>,
) -> Option<Vec<u8>> {
    let mut opts = LzmaOptions::new_preset(6 | PRESET_EXTREME).ok()?;
    opts.literal_context_bits(lc)
        .literal_position_bits(lp)
        .position_bits(pb)
        .dict_size(dict)
        .nice_len(nice);
    let mut f = Filters::new();
    if let Some(d) = delta {
        // liblzma delta property: one byte, dist = byte + 1 (single filter!)
        f.delta_properties(&[(d - 1) as u8]).ok()?;
    }
    f.lzma2(&opts);
    let mut s = match Stream::new_raw_encoder(&f) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[tune] raw_encoder err: {e}");
            return None;
        }
    };
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    if let Err(e) = s.process_vec(data, &mut out, liblzma::stream::Action::Run) {
        eprintln!("[tune] process err: {e}");
        return None;
    }
    if let Err(e) = s.process_vec(&[], &mut out, liblzma::stream::Action::Finish) {
        eprintln!("[tune] finish err: {e}");
        return None;
    }
    Some(out)
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: lzma_tune_probe <dim-dir>"));
    let mut seen: HashSet<u128> = HashSet::new();
    let mut members: Vec<Vec<u8>> = Vec::new();
    let mut rd = std::fs::read_dir(dim.join("region")).expect("region dir");
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        if f.extension().map(|e| e != "mca").unwrap_or(true) {
            continue;
        }
        for (_cx, _cz, payload) in load_mca(&f) {
            if let Ok(Split::Split { sections, .. }) =
                chunknbt::split(&payload, false, None, true)
            {
                for (h, sb) in &sections {
                    if seen.insert(*h) {
                        let bytes: Vec<u8> = match sb {
                            SecBytes::Shared(a) => (**a).clone(),
                            SecBytes::Owned(b) => b.clone(),
                        };
                        members.push(bytes);
                    }
                }
            }
        }
    }
    let raw_total: u64 = members.iter().map(|m| 24 + m.len() as u64).sum();
    println!(
        "unique members: {}  stream raw: {:.2} MiB",
        members.len(),
        raw_total as f64 / (1 << 20) as f64
    );

    // single group (matches the local corpus; the shipped 8 MiB cap holds it)
    let mut group: Vec<u8> = Vec::new();
    for m in &members {
        group.extend_from_slice(&(0u32).to_le_bytes());
        group.extend_from_slice(&(m.len() as u32).to_le_bytes());
        group.extend_from_slice(&[0u8; 16]);
        group.extend_from_slice(m);
    }

    struct Row {
        name: &'static str,
        lc: u32,
        lp: u32,
        pb: u32,
        dict: u32,
        nice: u32,
        delta: Option<u32>,
    }
    let d8 = 8 << 20;
    let rows = [
        Row { name: "shipped 6e (lc3 lp0 pb2 nice273)", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: None },
        Row { name: "pb0 (lc3 lp0 pb0)", lc: 3, lp: 0, pb: 0, dict: d8, nice: 273, delta: None },
        Row { name: "pb1", lc: 3, lp: 0, pb: 1, dict: d8, nice: 273, delta: None },
        Row { name: "lc4 (lc4 lp0 pb0)", lc: 4, lp: 0, pb: 0, dict: d8, nice: 273, delta: None },
        Row { name: "lc4 pb2", lc: 4, lp: 0, pb: 2, dict: d8, nice: 273, delta: None },
        Row { name: "lc2 lp2 (pb0)", lc: 2, lp: 2, pb: 0, dict: d8, nice: 273, delta: None },
        Row { name: "lc1 lp0 pb0", lc: 1, lp: 0, pb: 0, dict: d8, nice: 273, delta: None },
        Row { name: "nice 64", lc: 3, lp: 0, pb: 2, dict: d8, nice: 64, delta: None },
        Row { name: "nice 256", lc: 3, lp: 0, pb: 2, dict: d8, nice: 256, delta: None },
        Row { name: "delta1 + shipped", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: Some(1) },
        Row { name: "delta2 + shipped", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: Some(2) },
        Row { name: "delta4 + shipped", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: Some(4) },
        Row { name: "delta8 + shipped", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: Some(8) },
        Row { name: "delta16 + shipped", lc: 3, lp: 0, pb: 2, dict: d8, nice: 273, delta: Some(16) },
    ];
    let mut base = 0u64;
    for r in rows.iter() {
        let t = Instant::now();
        let c = lzma2_tuned(&group, r.lc, r.lp, r.pb, r.dict, r.nice, r.delta).expect("encode");
        let wall = t.elapsed().as_secs_f64();
        if r.name.starts_with("shipped") {
            base = c.len() as u64;
        }
        println!(
            "{:<36} {:>10} B  vs shipped {:+6.2}%  wall {:5.1}s",
            r.name,
            c.len(),
            (c.len() as f64 - base as f64) / base as f64 * 100.0,
            wall
        );
    }
}
