//! live_repro: the paper-bench boot->run1 cycle — live-path writes (light
//! kept), close WITHOUT pack (kill TERM semantics), reopen, prefetch count,
//! read-everything. Any slab parse failure at reopen truncates the segment
//! and shows up as missing chunks here.
//!
//! Modes: live_repro [tectonic]  (default: the JJ .mca corpus)
#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::store::{Cfg, Store};
use flate2::read::ZlibDecoder;
use std::io::Read;
use std::path::Path;

fn read_region(rx: i32, rz: i32, dir: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let path = dir.join(format!("r.{}.{}.mca", rx, rz));
    let Ok(data) = std::fs::read(&path) else { return Vec::new() };
    let mut out = Vec::new();
    for i in 0..1024usize {
        let e = i * 4;
        let off = ((data[e] as usize) << 16) | ((data[e + 1] as usize) << 8) | data[e + 2] as usize;
        if off == 0 || data[e + 3] == 0 { continue; }
        let a = off * 4096;
        if a + 5 > data.len() { continue; }
        let len = u32::from_be_bytes([data[a], data[a + 1], data[a + 2], data[a + 3]]) as usize;
        if len == 0 || a + 5 + len > data.len() { continue; }
        if data[a + 4] == 2 {
            let mut d = Vec::with_capacity(len);
            if ZlibDecoder::new(&data[a + 5..a + 5 + len]).read_to_end(&mut d).is_ok() {
                let cx = rx * 32 + (i as i32 & 31);
                let cz = rz * 32 + (i as i32 >> 5);
                out.push((cx, cz, d));
            }
        }
    }
    out
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let use_tc = arg == "tectonic" || arg == "midpack-tectonic";
    let midpack = arg.starts_with("midpack");
    let dir = std::path::PathBuf::from("/tmp/live_repro_store");
    let _ = std::fs::remove_dir_all(&dir);
    let st = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();

    // phase 1: live-path writes (the boot server's worldgen+autosave)
    let mut payloads: Vec<(u64, Vec<u8>)> = Vec::new();
    if use_tc {
        let f = 48i32;
        let mut b = tc::Builder::new(f);
        for cz in 0..f {
            for cx in 0..f {
                let (p, _) = b.payload(cx, cz);
                let k = key(cx, cz);
                st.write_chunk(k, &p, false).unwrap();
                payloads.push((k, p));
            }
        }
    } else {
        let dim = Path::new("../bench-corpus/dim");
        let mut regions: Vec<(i32, i32)> = std::fs::read_dir(dim.join("region"))
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let p: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
                if p.len() == 3 && p[0] == "r" {
                    Some((p[1].parse().ok()?, p[2].parse().ok()?))
                } else {
                    None
                }
            })
            .collect();
        regions.sort();
        for (rx, rz) in &regions {
            for (cx, cz, payload) in read_region(*rx, *rz, &dim.join("region")) {
                let k = key(cx, cz);
                st.write_chunk(k, &payload, false).unwrap();
                payloads.push((k, payload));
            }
        }
    }
    println!("written: {}", payloads.len());

    // phase 1.5: the MAINTENANCE pack mid-boot (autosave semantics): pack
    // the first half of the writes, then keep writing (live churn), close
    if midpack {
        st.sync();
        st.seal();
        st.request_pack();
        for _ in 0..4 {
            while st.compact_step() {}
        }
        println!("mid pack done, disk {} B", st.disk_bytes());
    }
    // phase 2: the BOOT close — NO pack (kill TERM, no autosave pack fired)
    st.sync();
    st.seal();
    st.sync();
    drop(st);

    // phase 3: reopen — the run1 server
    let st2 = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();
    println!("chunks after reopen: {}", st2.chunk_count());
    let (pf_batches, pf_slabs, pf_bytes) = st2.prefetch_slabs();
    println!("prefetch: {pf_batches} batches + {pf_slabs} slabs, {:.1} MiB", pf_bytes as f64 / 1048576.0);
    let inv = st2.slab_invariant_scan();
    println!("invariant scan: {} violations", inv.len());
    let (mut ok, mut miss) = (0usize, 0usize);
    for (k, _) in &payloads {
        match st2.read_chunk(*k) {
            Some(d) if !d.is_empty() => ok += 1,
            _ => miss += 1,
        }
    }
    println!("read sweep: ok={ok} miss={miss}");
    if miss > 0 {
        println!("=== live_repro: FAIL — reopen lost {} chunks ===", miss);
        std::process::exit(1);
    }

    // phase 4: MULTI-GENERATION — the run2 session writes MORE chunks (the
    // relight rewrites + new worldgen), the maintenance packs sealed
    // segments (hybrid mints over an already-hybrid container), close,
    // reopen, read EVERYTHING again. Repeat 3 generations.
    let mut gen_st = st2;
    for gen in 0..3usize {
        // live writes: rewrite every 3rd chunk (relight semantics) + 200 new
        for (i, (k, p)) in payloads.iter().enumerate() {
            if i % 3 == gen % 3 {
                gen_st.write_chunk(*k, p, false).unwrap();
            }
        }
        for cz in 0..20i32 {
            for cx in 0..20i32 {
                let k = key(cx + 1000, cz + 1000 + gen as i32 * 100);
                let (p, _) = tc::Builder::new(24).payload(cx, cz);
                gen_st.write_chunk(k, &p, false).unwrap();
            }
        }
        gen_st.sync();
        gen_st.seal();
        gen_st.request_pack();
        for _ in 0..8 {
            while gen_st.compact_step() {}
        }
        gen_st.sync();
        let disk = gen_st.disk_bytes();
        drop(gen_st);
        let st3 = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();
        let inv = st3.slab_invariant_scan();
        let (mut ok, mut miss) = (0usize, 0usize);
        for (k, _) in &payloads {
            match st3.read_chunk(*k) {
                Some(d) if !d.is_empty() => ok += 1,
                _ => miss += 1,
            }
        }
        println!(
            "gen {gen}: reopen chunks={} invariant={} read ok={ok} miss={miss} disk={disk} B",
            st3.chunk_count(),
            inv.len()
        );
        if miss > 0 {
            println!("=== live_repro: FAIL at gen {gen} ===");
            std::process::exit(1);
        }
        gen_st = st3;
    }
    drop(gen_st);
    println!("=== live_repro: PASS ===");
}
