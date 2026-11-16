use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 { return vec![]; }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else { return vec![]; };
    if data.len() < 8192 { return vec![]; }
    let mut out = Vec::new();
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([data[i*4], data[i*4+1], data[i*4+2], data[i*4+3]]);
        if loc == 0 { continue; }
        let off = (loc >> 8) as usize * 4096;
        if off + 5 > data.len() { continue; }
        let len = u32::from_be_bytes([data[off], data[off+1], data[off+2], data[off+3]]) as usize;
        if len < 2 || off + 4 + len > data.len() { continue; }
        let ct = data[off + 4];
        let comp = data[off + 5..off + 4 + len].to_vec();
        let payload = match ct {
            1 => { let mut v = Vec::new(); if flate2::read::GzDecoder::new(&comp[..]).read_to_end(&mut v).is_err() { continue; } v }
            2 => { let mut v = Vec::new(); if flate2::read::ZlibDecoder::new(&comp[..]).read_to_end(&mut v).is_err() { continue; } v }
            3 => comp.clone(),
            _ => continue,
        };
        out.push((rx*32 + (i as i32 & 31), rz*32 + (i as i32 >> 5), payload));
    }
    out
}
fn load_corpus(dim: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let mut c = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region")).expect("region dir").flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files { if f.extension().map(|e| e != "mca").unwrap_or(true) { continue; } c.extend(load_mca(&f)); }
    c
}
fn key(cx: i32, cz: i32) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).unwrap());
    let corpus = load_corpus(&dim);
    let dir = std::env::temp_dir().join("csaves-secanat");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in &corpus { st.write_chunk(key(*cx, *cz), p, false).unwrap(); }
    st.sync(); st.seal();
    let mut steps = 0;
    loop { let did = st.compact_step(); steps += did as usize; if !did || steps > 400 { break; } }
    let members = st.slab_members_by_class();
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);
    // anatomy of csec3 members
    let mut lens: Vec<usize> = Vec::new();
    let mut shown = 0;
    for (cls, b) in &members {
        if cls != "csec3" { continue; }
        lens.push(b.len());
        if shown < 6 {
            println!("csec3 len={} head={:02x?}", b.len(), &b[..b.len().min(120)]);
            // decode: [0xC7][f1][f2][y]
            println!("   f1={:02x} f2={:02x} y={}", b[1], b[2], b[3] as i8);
            shown += 1;
        }
    }
    lens.sort();
    let n = lens.len();
    let avg = lens.iter().sum::<usize>() / n;
    println!("csec3 n={n} avg={avg} p10={} p50={} p90={} max={}", lens[n/10], lens[n/2], lens[9*n/10], lens[n-1]);
}
