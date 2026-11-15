//! glob_par: byte-exactness probe for the hashless-GLOB identity recompute.
//! For every canonical CSEC member of the corpus: convert to the GLOB form
//! (shared registry, pack order), convert BACK with glob_any_to_section,
//! and require the result to equal the original canonical bytes BIT FOR BIT
//! — that is the property a scan-side identity recompute for hashless GLOB
//! slabs would rely on (identity = hash_of(glob_any_to_section(stored))).
//! Reports exact / diverged / bail counts and the first divergence dump.
//!
//! Usage: glob_par <dim-dir>

use c_saves::chunknbt::{self, SecBytes, Split, StateRegistry};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
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

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: glob_par <dim-dir>"));
    let mut seen: HashSet<u128> = HashSet::new();
    let mut members: Vec<(u128, Vec<u8>)> = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .expect("region dir")
        .flatten()
        .map(|e| e.path())
        .collect();
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
                        let bytes = match sb {
                            SecBytes::Shared(a) => (**a).clone(),
                            SecBytes::Owned(b) => b.clone(),
                        };
                        members.push((*h, bytes));
                    }
                }
            }
        }
    }
    println!("unique members: {}", members.len());

    let mut reg = StateRegistry::new();
    let (mut exact, mut diverged, mut bail_fwd, mut bail_back) = (0u64, 0u64, 0u64, 0u64);
    let mut first_div: Option<(usize, usize)> = None; // (pos, len)
    for (i, (h, csec)) in members.iter().enumerate() {
        let Some(glob) = chunknbt::section_to_glob(csec, &mut reg) else {
            bail_fwd += 1;
            continue;
        };
        let Some(back) = chunknbt::glob_any_to_section(&glob, &reg) else {
            bail_back += 1;
            continue;
        };
        if &back == csec && chunknbt::hash_of(&back) == *h {
            exact += 1;
        } else {
            diverged += 1;
            if first_div.is_none() {
                let n = back.len().min(csec.len()).min(64);
                first_div = back
                    .iter()
                    .zip(csec.iter())
                    .position(|(a, b)| a != b)
                    .map(|p| (p, n));
            }
        }
        if i % 3000 == 0 {
            eprintln!("[par] {i}/{} reg={}", members.len(), reg.len());
        }
    }
    println!(
        "exact={exact} diverged={diverged} bail_fwd={bail_fwd} bail_back={bail_back} reg={}",
        reg.len()
    );
    if let Some((p, n)) = first_div {
        println!("first divergence at byte {p} (dump window {n})");
    }
}
