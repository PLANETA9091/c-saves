//! slab_attrib: drained-container per-class member byte attribution.
//! Drives the tuned drain (same recipe as tune_ab's tuned leg), then walks
//! every on-disk slab and sums member raw bytes per content class:
//! csec4 dense/RLE/RLE-transposed x light-collapse bits, glob gforms,
//! shift-ref, tail NBT, other; plus the entry-table split (hashed vs
//! hashless rows) and the compressed cost per stamp family.
//!
//! Usage: slab_attrib <dim-dir>

use c_saves::store::{Cfg, Store};
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

fn load_corpus(dim: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let mut corpus = Vec::new();
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
        corpus.extend(load_mca(&f));
    }
    corpus
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: slab_attrib <dim-dir>"));
    let corpus = load_corpus(&dim);
    let raw: u64 = corpus.iter().map(|(_, _, p)| p.len() as u64).sum();
    println!(
        "corpus: {} chunks, {:.2} MiB raw",
        corpus.len(),
        raw as f64 / 1048576.0
    );

    let dir = std::env::temp_dir().join(format!(
        "csaves-attrib-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in &corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    st.sync();
    st.seal();
    let mut steps = 0usize;
    loop {
        let did = st.compact_step();
        steps += did as usize;
        if !did || steps > 400 {
            break;
        }
    }
    println!(
        "drained: chunks={} pack_steps={} disk={} B",
        st.chunk_count(),
        steps,
        st.disk_bytes()
    );
    println!("profile: {}", st.profile_json());
    println!("attribution: {}", st.slab_member_attribution_json());
    let viol = st.slab_invariant_scan();
    println!("invariant violations: {}", viol.len());
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);
}
