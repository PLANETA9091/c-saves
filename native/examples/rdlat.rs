//! rdlat: entity-chunk read latency through the real store (cold vs warm slab).
use c_saves::store::Store;
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
        out.push((rx * 32 + (i as i32 & 31), rz * 32 + (i as i32 >> 5), payload));
    }
    out
}

fn key(cx: i32, cz: i32) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }

fn main() {
    let dim = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let tmp = std::env::temp_dir().join("rdlat-store");
    let _ = std::fs::remove_dir_all(&tmp);
    let st = Store::open_cfg(&tmp, true, c_saves::store::Cfg::default()).unwrap();
    st.set_whole_mode();
    let mut chunks = Vec::new();
    for e in std::fs::read_dir(dim.join("entities")).unwrap().flatten() {
        chunks.extend(load_mca(&e.path()));
    }
    for (cx, cz, p) in &chunks { st.write_chunk(key(*cx, *cz), p, true).unwrap(); }
    st.sync(); st.seal(); st.request_pack();
    while st.compact_step() {}
    st.sync();

    // COLD reads: random order, no cache warm-up
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    // deterministic shuffle by prime stride
    let ln = order.len(); for i in 0..ln { order.swap(i, (i * 7919 + 13) % ln); }
    let t0 = Instant::now();
    let mut n = 0u64;
    for &i in &order {
        if st.read_chunk(key(chunks[i].0, chunks[i].1)).is_some() { n += 1; }
    }
    let cold = t0.elapsed();
    // WARM reads: same order again
    let t0 = Instant::now();
    for &i in &order {
        st.read_chunk(key(chunks[i].0, chunks[i].1)).unwrap();
    }
    let warm = t0.elapsed();
    println!(
        "entity reads: cold {} reads in {:.1?} ({:.0} us/read avg), warm {:.1?} ({:.2} us/read), n={}",
        n, cold.as_secs_f64(), cold.as_micros() as f64 / n as f64,
        warm.as_secs_f64(), warm.as_micros() as f64 / n as f64, n
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
