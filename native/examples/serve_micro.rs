use c_saves::store::{Cfg, Store};
use std::io::Read;
use std::time::Instant;

fn load_mca(path: &std::path::Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    let (rx, rz): (i32, i32) = (parts[1].parse().unwrap(), parts[2].parse().unwrap());
    let data = std::fs::read(path).unwrap();
    let mut out = vec![];
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([data[i*4], data[i*4+1], data[i*4+2], data[i*4+3]]);
        if loc == 0 { continue; }
        let off = (loc >> 8) as usize * 4096;
        let len = u32::from_be_bytes([data[off], data[off+1], data[off+2], data[off+3]]) as usize;
        let mut payload = Vec::new();
        flate2::read::ZlibDecoder::new(&data[off+5..off+4+len]).read_to_end(&mut payload).ok();
        if payload.is_empty() { continue; }
        out.push((rx * 32 + (i as i32 & 31), rz * 32 + (i as i32 >> 5), payload));
    }
    out
}

fn main() {
    let dim = std::path::PathBuf::from("../bench-corpus/dim");
    let tmp = std::env::temp_dir().join("serve-micro");
    let _ = std::fs::remove_dir_all(&tmp);
    let st = Store::open_cfg(&tmp, true, Cfg::default()).unwrap();
    let mut chunks = Vec::new();
    for e in std::fs::read_dir(dim.join("region")).unwrap().flatten() {
        chunks.extend(load_mca(&e.path()));
    }
    for (cx, cz, p) in &chunks { st.write_chunk(key(*cx, *cz), p, false).unwrap(); }
    st.sync(); st.seal(); st.request_pack();
    while st.compact_step() {}
    st.sync();
    // serve: all regions, stage timers on
    let keys = st.keys();
    let t0 = Instant::now();
    let mut regions = 0usize;
    let mut served = 0u64;
    for rx in -2..2 { for rz in -2..2 {
        if let Ok(Some(bytes)) = c_saves::anvil::serve_region(&st, rx, rz) {
            regions += 1;
            served += bytes.len() as u64;
        }
    }}
    let wall = t0.elapsed();
    println!(
        "serve-micro: {regions} regions, {:.1} MiB in {:.2?} = {:.2} MB/s",
        served as f64 / 1048576.0,
        wall.as_secs_f64(),
        served as f64 / 1048576.0 / wall.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

fn key(cx: i32, cz: i32) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }
