//! lat99: the live-path read tail, priced locally.
//! Writes the region corpus through the REAL store path (like a live server's
//! autosave), seals + packs, then reads cold (random order, first-touch slab
//! misses) and warm (same order again) with per-op percentiles — the exact
//! shape the live meters (fn/fp50/fp90/fp99) report from Paper.
use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;
use std::io::Read;
use std::time::Instant;

type Corpus = Vec<(i32, i32, Vec<u8>)>; // (cx, cz, uncompressed NBT)

fn load_mca(path: &std::path::Path) -> Corpus {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    let (rx, rz): (i32, i32) = (parts[1].parse().unwrap(), parts[2].parse().unwrap());
    let data = std::fs::read(path).unwrap();
    let mut out = vec![];
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([data[i*4], data[i*4+1], data[i*4+2], data[i*4+3]]);
        if loc == 0 { continue; }
        let off = (loc >> 8) as usize * 4096;
        if off + 5 > data.len() { continue; }
        let len = u32::from_be_bytes([data[off], data[off+1], data[off+2], data[off+3]]) as usize;
        let mut payload = Vec::new();
        flate2::read::ZlibDecoder::new(&data[off+5..off+4+len.min(data.len()-off)]).read_to_end(&mut payload).ok();
        if payload.is_empty() { continue; }
        out.push((rx * 32 + (i as i32 & 31), rz * 32 + (i as i32 >> 5), payload));
    }
    out
}

fn load_dir(dir: &std::path::Path) -> Corpus {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        if f.extension().map(|e| e == "mca").unwrap_or(false) {
            out.extend(load_mca(&f));
        }
    }
    out
}

fn key(cx: i32, cz: i32) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(label: &str, lat: &[u64], bytes: &mut u64, per_read: &[u64]) {
    let mut s = lat.to_vec();
    s.sort_unstable();
    let n = s.len();
    let avg = s.iter().sum::<u64>() / n.max(1) as u64;
    let mut pr = per_read.to_vec();
    pr.sort_unstable();
    let pb = pct(&pr, 0.99);
    println!(
        "LAT99 {label}: n={n} avg={avg}us p50={}us p90={}us p99={}us p999={}us max={}us | payload-p99={pb}us",
        pct(&s, 0.50), pct(&s, 0.90), pct(&s, 0.99), pct(&s, 0.999), *s.last().unwrap_or(&0)
    );
    *bytes = pr.iter().sum::<u64>() / n.max(1) as u64;
}

fn main() {
    let dim = std::path::PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| "bench-corpus/dim".into()),
    );
    let tmp = std::env::temp_dir().join("lat99-store");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut cfg = Cfg::default();
    if let Ok(v) = std::env::var("CSAVES_SLAB_CACHE") {
        if let Ok(n) = v.parse::<u64>() { cfg.slab_cache = n; }
    }
    let st = Store::open_cfg(&tmp, true, cfg).unwrap();
    st.set_whole_mode();
    let corpus = load_dir(&dim.join("region"));
    let total_raw: usize = corpus.iter().map(|c| c.2.len()).sum();
    println!("corpus: {} chunks, {:.1} MiB raw", corpus.len(), total_raw as f64 / 1048576.0);
    for (cx, cz, p) in &corpus {
        st.write_chunk(key(*cx, *cz), p, true).unwrap();
    }
    st.sync(); st.seal(); st.request_pack();
    while st.compact_step() {}
    st.sync();
    let sum_dir = |d: &std::path::Path| -> u64 {
        std::fs::read_dir(d).map(|rd| rd.flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()).unwrap_or(0)
    };
    let container = sum_dir(&tmp) + sum_dir(&tmp.join("segs"));
    println!("container: {:.1} MiB on disk", container as f64 / 1048576.0);

    // COLD: fresh random order (prime stride), like a boot's spiral fetch
    let mut order: Vec<usize> = (0..corpus.len()).collect();
    let ln = order.len();
    for i in 0..ln { order.swap(i, (i * 7919 + 13) % ln); }
    let mut lat = Vec::with_capacity(ln);
    let mut per_read = Vec::with_capacity(ln);
    let mut served = 0usize;
    for &i in &order {
        let t0 = Instant::now();
        let r = st.read_chunk(key(corpus[i].0, corpus[i].1));
        lat.push(t0.elapsed().as_nanos() as u64 / 1000);
        if r.is_some() { served += 1; per_read.push(r.unwrap().len() as u64); }
    }
    let mut avg_payload = 0u64;
    report("cold", &lat, &mut avg_payload, &per_read);
    // PREFETCH: warm the slab cache (the v0.9.65 attach hook), then re-measure
    let tp = Instant::now();
    let (nb, ns, bytes) = st.prefetch_slabs();
    println!(
        "prefetch: {nb} batches + {ns} slabs, {:.1} MiB in {:.0} ms",
        bytes as f64 / 1048576.0,
        tp.elapsed().as_millis()
    );
    let mut lat = Vec::with_capacity(ln);
    let mut sink = 0u64;
    for &i in &order {
        let t0 = Instant::now();
        st.read_chunk(key(corpus[i].0, corpus[i].1)).unwrap();
        lat.push(t0.elapsed().as_nanos() as u64 / 1000);
    }
    report("prefetched", &lat, &mut sink, &per_read);
    // WARM: same order again — everything cached
    let mut lat = Vec::with_capacity(ln);
    for &i in &order {
        let t0 = Instant::now();
        st.read_chunk(key(corpus[i].0, corpus[i].1)).unwrap();
        lat.push(t0.elapsed().as_nanos() as u64 / 1000);
    }
    let mut sink = 0u64;
    report("warm", &lat, &mut sink, &per_read);
    // stage stats for the cold phase are folded into the global counters;
    // print the accumulated stage picture
    let stages = c_saves::store::read_stage_json();
    println!("STAGES {stages}");
    let _ = std::fs::remove_dir_all(&tmp);
}
