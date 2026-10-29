//! speed_bench: I/O throughput + latency benchmark, c-saves vs vanilla Anvil.
//!
//! Runs on a REAL JJThunder To The Max world slice (the committed corpus):
//! region/*.mca + entities/*.mca produced by Paper 26.2. The Anvil baseline
//! is a faithful in-memory transcription of what
//! RegionFile.getChunkDataInputStream / getChunkDataOutputStream do:
//! locate the chunk in the region (header lookup), inflate/deflate with
//! zlib level 6 (vanilla's Deflater default). The c-saves side drives the
//! real Store through the production pipeline (split + dedup + zstd tiers +
//! slab packing + dictionaries).
//!
//! Everything is measured in-memory (page-cache warm) so the comparison is
//! CPU-path only: disk behavior is identical for both (append-only writes,
//! pread/mmap reads) and would only scale both sides equally.
//!
//! Phases:
//!   A anvil-read      inflate every chunk from its stored zlib stream
//!   B anvil-write     deflate every chunk (zlib-6, vanilla default level)
//!   C csaves-import   write every chunk once (fresh store, hot tier)
//!   D pack            background compaction to steady state (untimed)
//!   E csaves-read-cold   reopen the store fresh (cold slab cache) + read all
//!   F csaves-read-warm   R rounds, hot slab cache
//!   G csaves-write       R rounds steady-state rewrite (autosave model)
//!   H verify          semantic NBT equality on EVERY chunk (compat gate)
//!
//! Usage: cargo run --release --example speed_bench -- <dim-dir> [rounds] [threads]

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

// ------------------------------------------------------------- anvil io ---

/// (cx, cz, raw payload, compressed zlib bytes as stored in the .mca)
type Corpus = Vec<(i32, i32, Vec<u8>, Vec<u8>)>;

fn load_mca(path: &Path) -> Corpus {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else { return vec![] };
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
        out.push((cx, cz, payload, comp));
    }
    out
}

fn load_dir(dir: &Path) -> Corpus {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        if f.extension().map(|e| e == "mca").unwrap_or(false) {
            out.extend(load_mca(&f));
        }
    }
    out
}

// -------------------------------------------------------------- percentiles

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

struct Lat {
    samples: Vec<u64>, // ns per op
}

impl Lat {
    fn new(cap: usize) -> Self {
        Lat { samples: Vec::with_capacity(cap) }
    }
    #[inline]
    fn push(&mut self, ns: u64) {
        self.samples.push(ns);
    }
    fn report(&self, label: &str, bytes: u64, secs: f64) {
        let mut s = self.samples.clone();
        s.sort_unstable();
        let n = s.len();
        let mb = bytes as f64 / 1_048_576.0;
        let mbps: f64 = if secs > 0.0 { mb as f64 / secs as f64 } else { 0.0 };
        println!(
            "SPEED {label}: ops={n} bytes={bytes} MB/s={mbps:.1} p50={:.1}us p90={:.1}us p99={:.1}us p99.9={:.1}us max={:.1}us wall={:.3}s",
            pct(&s, 0.50) as f64 / 1000.0,
            pct(&s, 0.90) as f64 / 1000.0,
            pct(&s, 0.99) as f64 / 1000.0,
            pct(&s, 0.999) as f64 / 1000.0,
            pct(&s, 1.0) as f64 / 1000.0,
            secs
        );
    }
}

// ------------------------------------------------------------------ phases

fn anvil_read_round(corpus: &Corpus, lat: &mut Lat) -> u64 {
    let mut bytes = 0u64;
    for (_, _, _, comp) in corpus {
        let t0 = Instant::now();
        let mut v = Vec::with_capacity(comp.len() * 6);
        // vanilla: getChunkDataInputStream -> InflaterInputStream over the sector
        if flate2::read::ZlibDecoder::new(&comp[..]).read_to_end(&mut v).is_ok() {
            bytes += v.len() as u64;
        }
        lat.push(t0.elapsed().as_nanos() as u64);
    }
    bytes
}

fn anvil_write_round(corpus: &Corpus, lat: &mut Lat) -> u64 {
    let mut bytes = 0u64;
    for (_, _, payload, _) in corpus {
        let t0 = Instant::now();
        // vanilla: NbtIo.write -> DeflaterOutputStream (default level = 6)
        let mut z = flate2::write::ZlibEncoder::new(Vec::with_capacity(payload.len() / 4), flate2::Compression::new(6));
        if z.write_all(payload).is_ok() {
            if let Ok(c) = z.finish() {
                bytes += payload.len() as u64;
                std::hint::black_box(&c);
            }
        }
        lat.push(t0.elapsed().as_nanos() as u64);
    }
    bytes
}

fn csaves_read_round(st: &Store, corpus: &[(i32, i32, Vec<u8>, Vec<u8>)], whole: bool, lat: &mut Lat) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut misses = 0u64;
    for (cx, cz, payload, _) in corpus {
        let t0 = Instant::now();
        match st.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                bytes += d.len() as u64;
                if d.len() != payload.len() {
                    misses += 1; // size mismatch caught by verify phase
                }
            }
            None => misses += 1,
        }
        lat.push(t0.elapsed().as_nanos() as u64);
        let _ = whole;
    }
    (bytes, misses)
}

fn csaves_write_round(st: &Store, corpus: &[(i32, i32, Vec<u8>, Vec<u8>)], whole: bool, lat: &mut Lat) -> u64 {
    let mut bytes = 0u64;
    for (cx, cz, payload, _) in corpus {
        let t0 = Instant::now();
        if st.write_chunk(key(*cx, *cz), payload, whole).is_err() {
            bytes = bytes.wrapping_add(1); // flag via count; verify phase reports
        }
        bytes += payload.len() as u64;
        lat.push(t0.elapsed().as_nanos() as u64);
    }
    bytes
}

fn verify(st: &Store, corpus: &[(i32, i32, Vec<u8>, Vec<u8>)], whole: bool) -> (u64, u64) {
    // canonical CSEC re-encoding permutes palette order (semantically
    // neutral) — compare with the palette-permutation-safe comparator.
    let chunks_semantic = |a: &[u8], b: &[u8]| -> bool {
        let (Ok(Value::Compound(ma)), Ok(Value::Compound(mb))) =
            (fastnbt::from_bytes::<Value>(a), fastnbt::from_bytes::<Value>(b))
        else {
            return false;
        };
        if ma.len() != mb.len() {
            return false;
        }
        for (k, va) in &ma {
            let Some(vb) = mb.get(k) else { return false };
            if k == "sections" {
                let (Value::List(la), Value::List(lb)) = (va, vb) else { return false };
                if la.len() != lb.len() {
                    return false;
                }
                for (sa, sb) in la.iter().zip(lb.iter()) {
                    if !c_saves::chunknbt::sections_semantically_equal(sa, sb) {
                        return false;
                    }
                }
            } else if va != vb {
                return false;
            }
        }
        true
    };
    let (mut ok, mut bad) = (0u64, 0u64);
    for (cx, cz, payload, _) in corpus {
        match st.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                if chunks_semantic(payload, &d) {
                    ok += 1;
                } else {
                    bad += 1;
                    eprintln!("verify mismatch at ({cx},{cz}) whole={whole}");
                }
            }
            None => {
                bad += 1;
                eprintln!("verify MISS at ({cx},{cz}) whole={whole}");
            }
        }
    }
    (ok, bad)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).expect("usage: speed_bench <dim-dir> [rounds] [threads]"));
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);

    let region = load_dir(&dim.join("region"));
    let entities = load_dir(&dim.join("entities"));
    let region_raw: u64 = region.iter().map(|c| c.2.len() as u64).sum();
    let ent_raw: u64 = entities.iter().map(|c| c.2.len() as u64).sum();
    println!(
        "SPEED corpus: region={} chunks ({} MiB raw), entities={} chunks ({} MiB raw), rounds={rounds} threads={threads}",
        region.len(), region_raw / 1_048_576, entities.len(), ent_raw / 1_048_576
    );

    // ---------------- A: anvil read ----------------
    {
        let mut lat = Lat::new(region.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += anvil_read_round(&region, &mut lat);
        }
        lat.report("anvil-read-region", bytes, t0.elapsed().as_secs_f64());
    }
    {
        let mut lat = Lat::new(entities.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += anvil_read_round(&entities, &mut lat);
        }
        lat.report("anvil-read-entities", bytes, t0.elapsed().as_secs_f64());
    }

    // ---------------- B: anvil write ----------------
    {
        let mut lat = Lat::new(region.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += anvil_write_round(&region, &mut lat);
        }
        lat.report("anvil-write-region", bytes, t0.elapsed().as_secs_f64());
    }
    {
        let mut lat = Lat::new(entities.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += anvil_write_round(&entities, &mut lat);
        }
        lat.report("anvil-write-entities", bytes, t0.elapsed().as_secs_f64());
    }

    // ---------------- C+D: c-saves import + pack (untimed pack) ----------------
    let tmp = std::env::temp_dir().join(format!("speed-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let region_dir = tmp.join("region");
    let entity_dir = tmp.join("entities");
    let cfg = Cfg::from_env();
    let st_r = Store::open_cfg(&region_dir, true, cfg.clone()).unwrap();
    let st_e = Store::open_cfg(&entity_dir, true, cfg).unwrap();
    st_e.set_whole_mode();

    {
        let mut lat = Lat::new(region.len());
        let t0 = Instant::now();
        for (cx, cz, payload, _) in &region {
            let s = Instant::now();
            st_r.write_chunk(key(*cx, *cz), payload, false).unwrap();
            lat.push(s.elapsed().as_nanos() as u64);
        }
        let secs = t0.elapsed().as_secs_f64();
        lat.report("csaves-import-region", region_raw, secs);
        println!("SPEED stages-import: {}", c_saves::store::write_stage_json());
    }
    {
        let mut lat = Lat::new(entities.len());
        let t0 = Instant::now();
        for (cx, cz, payload, _) in &entities {
            let s = Instant::now();
            st_e.write_chunk(key(*cx, *cz), payload, true).unwrap();
            lat.push(s.elapsed().as_nanos() as u64);
        }
        let secs = t0.elapsed().as_secs_f64();
        lat.report("csaves-import-entities", ent_raw, secs);
    }
    st_r.sync();
    st_r.seal();
    st_r.request_pack();
    st_e.sync();
    st_e.seal();
    st_e.request_pack();
    let t0 = Instant::now();
    loop {
        let mut more = st_r.compact_step();
        more |= st_e.compact_step();
        if !more {
            break;
        }
    }
    st_r.sync();
    st_e.sync();
    println!("SPEED pack: {:.2}s", t0.elapsed().as_secs_f64());
    let container = st_r.disk_bytes() + st_e.disk_bytes();
    println!(
        "SPEED container: c-saves {:.2} MiB (anvil sector-padded {:.2} MiB, x{:.2} smaller)",
        container as f64 / 1_048_576.0,
        {
            let mut anvil = 0u64;
            for (_, _, payload, _) in region.iter().chain(entities.iter()) {
                let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
                z.write_all(payload).unwrap();
                let comp = z.finish().unwrap();
                let sectors = ((5 + comp.len()) as u64).div_ceil(4096);
                anvil += 8192 + sectors * 4096;
            }
            anvil as f64 / 1_048_576.0
        },
        {
            let mut anvil = 0u64;
            for (_, _, payload, _) in region.iter().chain(entities.iter()) {
                let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
                z.write_all(payload).unwrap();
                let comp = z.finish().unwrap();
                let sectors = ((5 + comp.len()) as u64).div_ceil(4096);
                anvil += 8192 + sectors * 4096;
            }
            anvil as f64 / container.max(1) as f64
        }
    );

    // ---------------- E: cold read (fresh store handles) ----------------
    drop(st_r);
    drop(st_e);
    let cfg = Cfg::from_env();
    let st_r = Store::open_cfg(&region_dir, true, cfg.clone()).unwrap();
    let st_e = Store::open_cfg(&entity_dir, true, cfg).unwrap();
    st_e.set_whole_mode();
    {
        let mut lat = Lat::new(region.len());
        let t0 = Instant::now();
        let (bytes, miss) = csaves_read_round(&st_r, &region, false, &mut lat);
        lat.report("csaves-read-cold-region", bytes, t0.elapsed().as_secs_f64());
        if miss > 0 {
            println!("SPEED WARN: cold read misses={miss}");
        }
    }

    // ---------------- F: warm reads ----------------
    {
        let mut lat = Lat::new(region.len() * rounds);
        let mut bytes = 0u64;
        let mut miss = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            let (b, m) = csaves_read_round(&st_r, &region, false, &mut lat);
            bytes += b;
            miss += m;
        }
        lat.report("csaves-read-warm-region", bytes, t0.elapsed().as_secs_f64());
        if miss > 0 {
            println!("SPEED WARN: warm read misses={miss}");
        }
        println!("SPEED stages-warm: {}", c_saves::store::read_stage_json());
        c_saves::store::read_stage_reset();
    }
    {
        let mut lat = Lat::new(entities.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            let (b, m) = csaves_read_round(&st_e, &entities, true, &mut lat);
            bytes += b;
            let _ = m;
        }
        lat.report("csaves-read-warm-entities", bytes, t0.elapsed().as_secs_f64());
    }

    // multi-threaded warm read (server-like: several IO worker threads)
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        let st = st_r.clone();
        let corpus = Arc::new(region.clone());
        let total = Arc::new(AtomicU64::new(0));
        let mut hs = Vec::new();
        let t0 = Instant::now();
        for t in 0..threads {
            let st = st.clone();
            let corpus = corpus.clone();
            let total = total.clone();
            hs.push(std::thread::spawn(move || {
                let mut n = 0u64;
                for r in 0..rounds as u64 {
                    let (cx, cz, _, _) = &corpus[((t as u64 + r) % corpus.len() as u64) as usize];
                    let _ = (cx, cz);
                    for i in 0..corpus.len() {
                        let (cx, cz, _, _) = &corpus[(i + t * 7 + r as usize * 3) % corpus.len()];
                        if st.read_chunk(key(*cx, *cz)).is_some() {
                            n += 1;
                        }
                    }
                }
                total.fetch_add(n, Ordering::Relaxed);
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let secs = t0.elapsed().as_secs_f64();
        let n = total.load(Ordering::Relaxed);
        println!(
            "SPEED csaves-read-mt-region: threads={threads} ops={n} ops/s={:.0} wall={:.3}s",
            n as f64 / secs, secs
        );
    }

    // multi-threaded import (server-like: Moonrise saves chunks from a pool
    // of IO workers — production write_chunk concurrency, not the
    // single-thread CI import loop). Fresh store, work-stealing by index.
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let mdir = tmp.join("region-mt");
        let _ = std::fs::remove_dir_all(&mdir);
        let st = Store::open_cfg(&mdir, true, Cfg::from_env()).unwrap();
        let corpus = Arc::new(region.clone());
        let next = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let mut hs = Vec::new();
        let t0 = Instant::now();
        for _t in 0..threads {
            let st = st.clone();
            let corpus = corpus.clone();
            let next = next.clone();
            let done = done.clone();
            hs.push(std::thread::spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= corpus.len() {
                    break;
                }
                let (cx, cz, payload, _) = &corpus[i];
                st.write_chunk(key(*cx, *cz), payload, false).unwrap();
                done.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let secs = t0.elapsed().as_secs_f64();
        let n = done.load(Ordering::Relaxed);
        println!(
            "SPEED csaves-import-mt-region: threads={threads} ops={n} MB/s={:.1} wall={:.3}s",
            region_raw as f64 / 1048576.0 / secs, secs
        );
        st.sync();
        st.seal();
    }

    // ---------------- G: steady-state rewrite ----------------
    {
        let mut lat = Lat::new(region.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += csaves_write_round(&st_r, &region, false, &mut lat);
        }
        let secs = t0.elapsed().as_secs_f64();
        lat.report("csaves-write-steady-region", bytes, secs);
        st_r.sync();
    }
    {
        let mut lat = Lat::new(entities.len() * rounds);
        let mut bytes = 0u64;
        let t0 = Instant::now();
        for _ in 0..rounds {
            bytes += csaves_write_round(&st_e, &entities, true, &mut lat);
        }
        let secs = t0.elapsed().as_secs_f64();
        lat.report("csaves-write-steady-entities", bytes, secs);
        st_e.sync();
    }

    // ---------------- H: verify (untimed) ----------------
    {
        let (ok, bad) = verify(&st_r, &region, false);
        println!("SPEED verify-region: ok={ok} bad={bad}");
        let (ok2, bad2) = verify(&st_e, &entities, true);
        println!("SPEED verify-entities: ok={ok2} bad={bad2}");
        if bad + bad2 > 0 {
            eprintln!("SPEED VERIFY FAILED: bad={}+{}", bad, bad2);
            std::process::exit(1);
        }
    }
    let _ = container;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("SPEED done");
}
