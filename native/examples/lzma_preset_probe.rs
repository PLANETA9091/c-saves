//! LZMA2 preset ladder on the REAL post-dedup member stream (the 0xFE slab
//! bulk): collect canonical section members in first-occurrence (chunk)
//! order — pack's group order — dedup by structural hash, group at the
//! 8 MiB slab cap with 24 B entry rows, compress each group with presets
//! {6=shipped, 9, 9e} and report size + wall. Encode-only knob: the raw
//! LZMA2 decoder is pinned to the preset's dict on both ends, so a preset
//! bump moves the decode window const too (documented, both ends rebuilt).
//! Usage: lzma_preset_probe <dim-dir>

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

fn lzma2_raw(data: &[u8], preset: u32, extreme: bool, dict_override: Option<u32>) -> Option<Vec<u8>> {
    let flag = if extreme { PRESET_EXTREME } else { 0 };
    let mut opts = LzmaOptions::new_preset(preset | flag).ok()?;
    if let Some(d) = dict_override {
        opts.dict_size(d);
    }
    let mut f = Filters::new();
    f.lzma2(&opts);
    let mut s = Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    s.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: lzma_preset_probe <dim-dir>"));
    // first-occurrence order (pack-like locality), dedup by structural hash
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
        "unique members: {}  stream raw (24B rows + bodies): {:.2} MiB",
        members.len(),
        raw_total as f64 / (1 << 20) as f64
    );

    // group at the 8 MiB slab cap (24 B entry + body), pack order
    let groups: Vec<Vec<u8>> = {
        let mut out = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_raw = 24usize;
        for m in &members {
            let need = 24 + m.len();
            if !cur.is_empty() && cur_raw + need > 8 * 1024 * 1024 {
                out.push(std::mem::take(&mut cur));
                cur_raw = 24;
            }
            cur.extend_from_slice(&(0u32).to_le_bytes());
            cur.extend_from_slice(&(m.len() as u32).to_le_bytes());
            cur.extend_from_slice(&[0u8; 16]);
            cur.extend_from_slice(m);
            cur_raw += need;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };
    println!("groups at 8 MiB cap: {}", groups.len());

    struct Row<'a> {
        name: &'a str,
        preset: u32,
        extreme: bool,
        dict: Option<u32>,
    }
    let rows = [
        Row { name: "preset 6 (shipped, dict 8 MiB)", preset: 6, extreme: false, dict: None },
        Row { name: "preset 6e (dict 8 MiB)", preset: 6, extreme: true, dict: None },
        Row { name: "preset 8e (dict 64 MiB)", preset: 8, extreme: true, dict: None },
        Row { name: "preset 8e + dict 8 MiB", preset: 8, extreme: true, dict: Some(8 << 20) },
        Row { name: "preset 9e + dict 8 MiB", preset: 9, extreme: true, dict: Some(8 << 20) },
        Row { name: "preset 9e (dict 64 MiB) [recheck]", preset: 9, extreme: true, dict: None },
    ];
    let mut base = 0u64;
    for r in rows.iter() {
        let t = Instant::now();
        let mut total = 0u64;
        for g in &groups {
            let c = lzma2_raw(g, r.preset, r.extreme, r.dict).expect("encode");
            total += c.len() as u64;
        }
        let wall = t.elapsed().as_secs_f64();
        if r.preset == 6 && !r.extreme && r.dict.is_none() {
            base = total;
        }
        println!(
            "{:<42} {:>10.0} B  vs p6 {:+6.2}%  wall {:6.2}s",
            r.name,
            total,
            (total as f64 - base as f64) / base as f64 * 100.0,
            wall
        );
    }
}
