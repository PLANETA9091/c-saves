//! Fingerprint every encoded section member across the corpus: split each
//! region payload, collect (structural_hash -> member bytes) for all
//! sections, sort, and hash. Byte-identical encoders MUST produce the same
//! fingerprint. Usage: enc_fp <dim-dir>

use c_saves::chunknbt::{self, SecBytes, Split};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

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

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: enc_fp <dim-dir>"));
    let mut members: BTreeMap<u128, (Vec<u8>, u64)> = BTreeMap::new();
    let (mut n_sec, mut n_split) = (0usize, 0usize);
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
                n_split += 1;
                for (h, sb) in &sections {
                    n_sec += 1;
                    let bytes: Vec<u8> = match sb {
                        SecBytes::Shared(a) => (**a).clone(),
                        SecBytes::Owned(b) => b.clone(),
                    };
                    members.entry(*h).or_insert((bytes, 0)).1 += 1;
                }
            }
        }
    }
    let mut h: u64 = 0xcbf29ce484222325;
    let mut n_members = 0u64;
    let mut total_bytes = 0u64;
    for (k, (v, cnt)) in &members {
        n_members += 1;
        total_bytes += v.len() as u64 * cnt;
        let mut feed = |slice: &[u8], h: &mut u64| {
            for x in slice {
                *h ^= *x as u64;
                *h = h.wrapping_mul(0x100000001b3);
            }
        };
        feed(&k.to_le_bytes(), &mut h);
        feed(&v.len().to_le_bytes(), &mut h);
        feed(&cnt.to_le_bytes(), &mut h);
        let heads: Vec<u8> = v
            .iter()
            .take(24)
            .chain(v.iter().rev().take(24))
            .copied()
            .collect();
        feed(&heads, &mut h);
    }
    println!(
        "ENC_FP split_chunks={} sections={} members={} bytes={} fp={h:016x}",
        n_split, n_sec, n_members, total_bytes
    );
}
