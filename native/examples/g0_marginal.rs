//! g0_marginal: is the class-split saving real? lzma2(mixed all) vs
//! lzma2(mixed minus g0) prices the g0 marginal cost inside the mixed
//! stream; compare against lzma2(g0 alone). If marginal > alone, a
//! dedicated section stream saves the delta; if equal, the split was
//! member-order luck.
use c_saves::store::{Cfg, Store};
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
fn lzma2_cold(data: &[u8]) -> Vec<u8> {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(6 | liblzma::stream::PRESET_EXTREME).expect("preset");
    opts.literal_context_bits(4).literal_position_bits(0).position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut s = liblzma::stream::Stream::new_raw_encoder(&f).expect("encoder");
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).expect("run");
    s.process_vec(&[], &mut out, liblzma::stream::Action::Finish).expect("finish");
    out
}
fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).unwrap());
    let corpus = load_corpus(&dim);
    let dir = std::env::temp_dir().join("csaves-g0marg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in &corpus { st.write_chunk(key(*cx, *cz), p, false).unwrap(); }
    st.sync(); st.seal();
    let mut steps = 0;
    loop { let did = st.compact_step(); steps += did as usize; if !did || steps > 400 { break; } }
    // DISK-ORDER members (bid order = real stream order): use the walk on
    // the live store; slab_members_by_class returns members in slab order
    // which IS the disk order within each slab; slabs in map order.
    let members = st.slab_members_by_class();
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);
    let mut mixed = Vec::new();
    let mut g0_only = Vec::new();
    let mut rest_only = Vec::new();
    for (cls, b) in &members {
        mixed.extend_from_slice(b);
        if cls == "glob_g0" { g0_only.extend_from_slice(b); } else { rest_only.extend_from_slice(b); }
    }
    println!("mixed raw={} g0={} rest={}", mixed.len(), g0_only.len(), rest_only.len());
    let t = std::time::Instant::now();
    let lz_all = lzma2_cold(&mixed).len();
    let lz_rest = lzma2_cold(&rest_only).len();
    let lz_g0 = lzma2_cold(&g0_only).len();
    let marg = lz_all as i64 - lz_rest as i64;
    println!(
        "lzma2 all={} rest={} g0-alone={} | g0-marginal={} | split-saving={:+} B ({:+.2}%) enc={:.1}s",
        lz_all, lz_rest, lz_g0, marg, lz_g0 as i64 - marg,
        (lz_g0 as i64 - marg) as f64 / lz_all as f64 * 100.0, t.elapsed().as_secs_f64()
    );
}
