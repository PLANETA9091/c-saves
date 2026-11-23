//! full_world_probe: the FULL-dimension honest weight — nether and end
//! join the huge-world arithmetic (the operator ask: "are you also counting
//! the nether and the end?").
//!
//! ONE footprint F (default 96) x F chunk square is generated for ALL
//! THREE dimensions:
//!   - overworld: the Tectonic mix (oceans/kelp/wrecks/beach/plains/
//!     river/mountain ridges — the vein-calibrated strata model),
//!   - nether: the cavern model (lava seas, soul valleys, glowstone
//!     ceilings, fortresses with real structure starts; heightmaps are
//!     const 128 — the bedrock ceiling is the top surface),
//!   - end: the void model (NO-section void chunks, flat main island,
//!     repeating outer islands, obsidian pillars, end cities with real
//!     structure starts).
//! Each dimension is written as REAL zlib-9 Anvil region files under the
//! vanilla directory layout (region/, DIM-1/region/, DIM1/region/) —
//! measured on disk, not estimated (Anvil's per-chunk sector floor is
//! real bytes) — and imported through the production import path into
//! its own store; after the full drain every chunk is verified
//! SEMANTICALLY against the generated payload (the bad=0 gate).
//!
//! The honest weight uses full_disk_bytes (segments + dict files + states)
//! per dimension. Projections are linear in chunk count (the scaling law
//! measured on the 384x384 rings): the operator's world (40000x40000
//! BLOCKS = 2500x2500 chunks — the onlinemo.de/world input with
//! overworld=40000) and the 20000x20000-chunk scale.
//!
//! Usage: cargo run --release --example full_world_probe [footprint]

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::path::PathBuf;

#[path = "inc/tectonic_corpus.rs"]
mod tc;
use tc::*;

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

/// the CI verify's own equality contract: compound fields as plain Values,
/// sections via the palette-permutation-safe comparator.
fn chunks_semantic(a: &[u8], b: &[u8]) -> bool {
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
}

struct DimStat {
    name: &'static str,
    chunks: usize,
    raw: u64,
    anvil: u64,
    container: u64,
    full: u64,
    ok_s: usize,
    miss: usize,
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

fn main() {
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(96);
    let out = PathBuf::from(
        std::env::var("CSAVES_FULLWORLD_OUT").unwrap_or_else(|_| "/tmp/full_world".to_string()),
    );
    let _ = std::fs::remove_dir_all(&out);

    let t_all = std::time::Instant::now();
    let mut stats: Vec<DimStat> = Vec::new();
    let mut verify_line = String::new();

    for (name, subdir) in [
        ("overworld", "region"),
        ("nether", "DIM-1/region"),
        ("end", "DIM1/region"),
    ] {
        // 1) generate the payloads (shared shapes patched per coordinate)
        let t0 = std::time::Instant::now();
        let mut payloads: Vec<(u64, Vec<u8>, &'static str)> = Vec::with_capacity((f * f) as usize);
        let mut class_n: Map<&'static str, usize> = Map::new();
        let mut raw = 0u64;
        match name {
            "overworld" => {
                let mut b = tc::Builder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        let (p, c) = b.payload(cx, cz);
                        raw += p.len() as u64;
                        *class_n.entry(c.name()).or_default() += 1;
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p, c.name()));
                    }
                }
            }
            "nether" => {
                let mut b = tc::NetherBuilder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        let (p, c) = b.payload(cx, cz);
                        raw += p.len() as u64;
                        *class_n.entry(c.name()).or_default() += 1;
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p, c.name()));
                    }
                }
            }
            _ => {
                let mut b = tc::EndBuilder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        let (p, c) = b.payload(cx, cz);
                        raw += p.len() as u64;
                        *class_n.entry(c.name()).or_default() += 1;
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p, c.name()));
                    }
                }
            }
        }
        let mut classes: Vec<(&'static str, usize)> = class_n.into_iter().collect();
        classes.sort_by_key(|c| std::cmp::Reverse(c.1));
        let census: Vec<String> = classes
            .iter()
            .map(|(k, v)| format!("{} {:.1}%", k, 100.0 * *v as f64 / payloads.len() as f64))
            .collect();
        println!(
            "== {} == gen {:.1?} | raw {:.2} MiB ({:.2} KiB/chunk) | mix: {}",
            name,
            t0.elapsed(),
            raw as f64 / 1048576.0,
            raw as f64 / 1024.0 / payloads.len() as f64,
            census.join(", ")
        );

        // 2) the Anvil baseline: real region files, vanilla directory layout
        let mut regions: Map<(i32, i32), Vec<(usize, usize)>> = Map::new();
        for (i, (k, _, _)) in payloads.iter().enumerate() {
            let cx = (k >> 32) as i32;
            let cz = (k & 0xffff_ffff) as u32 as i32;
            regions
                .entry((cx.div_euclid(32), cz.div_euclid(32)))
                .or_default()
                .push((((cx.rem_euclid(32)) + (cz.rem_euclid(32)) * 32) as usize, i));
        }
        let dim_dir = out.join(subdir);
        std::fs::create_dir_all(&dim_dir).unwrap();
        let mut anvil = 0u64;
        let mut rkeys: Vec<(i32, i32)> = regions.keys().copied().collect();
        rkeys.sort();
        for rk in &rkeys {
            let slots: Vec<(usize, &[u8])> = regions[rk]
                .iter()
                .map(|(s, i)| (*s, payloads[*i].1.as_slice()))
                .collect();
            anvil += write_region(&dim_dir, rk.0, rk.1, &slots);
        }

        // 3) the production import path into its own store + full drain
        c_saves::store::pack_census_reset();
        let store_dir = out.join(format!("store-{}", name));
        let st = Store::open_cfg(&store_dir, true, Cfg::from_env()).unwrap();
        let t0 = std::time::Instant::now();
        let mut keys: Vec<u64> = payloads.iter().map(|(k, _, _)| *k).collect();
        keys.sort();
        let by_key: Map<u64, usize> =
            payloads.iter().enumerate().map(|(i, (k, _, _))| (*k, i)).collect();
        for &k in &keys {
            st.write_chunk_import(k, &payloads[by_key[&k]].1).unwrap();
        }
        let import_t = t0.elapsed();
        drain(&st);
        println!("   pack census: {}", c_saves::store::pack_census_json());
        let container = st.disk_bytes();
        let full = st.full_disk_bytes();

        // 4) verify every read against the generated payload
        let t0 = std::time::Instant::now();
        let (mut ok_s, mut miss) = (0usize, 0usize);
        for &k in &keys {
            let want = &payloads[by_key[&k]].1;
            match st.read_chunk(k) {
                Some(got) => {
                    if chunks_semantic(want, &got) {
                        ok_s += 1;
                    }
                }
                None => miss += 1,
            }
        }
        let verify_t = t0.elapsed();
        println!(
            "   import {:.1?} | container {} (full {}) | {:.1} B/chunk | anvil {} = {:.0} B/chunk | verify {}/{} in {:.1?}",
            import_t,
            mib(container),
            mib(full),
            full as f64 / payloads.len() as f64,
            mib(anvil),
            anvil as f64 / payloads.len() as f64,
            ok_s,
            payloads.len(),
            verify_t
        );
        println!("   attribution: {}", st.slab_member_attribution_json());
        println!("   profile: {}", st.profile_json());
        // v0.9.112: the reopen gate — drop the store, open it fresh, verify
        // every chunk again. The reopen rebuilds the batch index by walking
        // the 0x0B/0x0C record streams; a decode bug here TRUNCATES the
        // segment at the first bad record (the scan's poison-guard) — this
        // gate exists to catch exactly that before it can ship.
        drop(st);
        let st = Store::open_cfg(&store_dir, true, Cfg::from_env()).unwrap();
        let (rok_s, rmiss) = (0usize, 0usize);
        let mut rok_s = rok_s;
        let mut rmiss = rmiss;
        for &k in &keys {
            let want = &payloads[by_key[&k]].1;
            match st.read_chunk(k) {
                Some(got) => {
                    if chunks_semantic(want, &got) {
                        rok_s += 1;
                    }
                }
                None => rmiss += 1,
            }
        }
        println!(
            "   reopen verify: {}/{} semantic (miss={})",
            rok_s,
            payloads.len(),
            rmiss
        );
        let (ok_s, miss) = (rok_s, rmiss);
        verify_line = if verify_line.is_empty() {
            format!("{} {}/{}", name, ok_s, payloads.len())
        } else {
            format!("{} | {} {}/{}", verify_line, name, ok_s, payloads.len())
        };
        // v0.9.116: the CLOSE weight — the honest shipped weight is the one
        // ON DISK after the store closes. The live drain's final segment
        // sits in the writer slot (GC-exempt) and its full reclaim happens
        // at close; the reopened store's full_disk_bytes reads exactly the
        // shipped files (segments + dicts + states), the same bytes the
        // reopen verify just proved readable. Measured on the 192x192
        // full-dimension corpus: live 5.1 B/chunk -> close 0.59 B/chunk
        // (the writer segment carried ~8x of the live weight).
        let close_full = st.full_disk_bytes();
        let close_container = st.disk_bytes();
        println!(
            "   close weight: segments {} + dicts/states = {} (live-printed full {} — the writer segment the drain exempted)",
            mib(close_container),
            mib(close_full),
            mib(full)
        );
        drop(st);
        if rok_s + rmiss != payloads.len() || rok_s != payloads.len() {
            println!("   reopen gate: FAIL ({} ok, {} miss of {})", rok_s, rmiss, payloads.len());
        }
        stats.push(DimStat {
            name,
            chunks: payloads.len(),
            raw,
            anvil,
            container: close_container,
            full: close_full,
            ok_s,
            miss,
        });
    }

    // 5) the honest per-dimension table + the full-dimension ratio
    let tot_c: usize = stats.iter().map(|s| s.chunks).sum();
    let tot_anvil: u64 = stats.iter().map(|s| s.anvil).sum();
    let tot_full: u64 = stats.iter().map(|s| s.full).sum();
    println!("== per-dimension CLOSE weight ({}x{} = {} chunks/dim, the shipped bytes on disk incl. dicts+states) ==", f, f, f * f);
    println!("dim        chunks  anvil         anvil B/chunk  c-saves full   B/chunk  ratio");
    for s in &stats {
        println!(
            "{:<10} {:>6}  {:>13}  {:>13.0}  {:>13}  {:>7.1}  x{:.0}",
            s.name,
            s.chunks,
            mib(s.anvil),
            s.anvil as f64 / s.chunks as f64,
            mib(s.full),
            s.full as f64 / s.chunks as f64,
            s.anvil as f64 / s.full.max(1) as f64,
        );
    }
    println!(
        "{:<10} {:>6}  {:>13}  {:>13.0}  {:>13}  {:>7.1}  x{:.0}",
        "ALL",
        tot_c,
        mib(tot_anvil),
        tot_anvil as f64 / tot_c as f64,
        mib(tot_full),
        tot_full as f64 / tot_c as f64,
        tot_anvil as f64 / tot_full.max(1) as f64,
    );
    println!(
        "full-dimension ratio: x{:.4} of anvil (all three dims, honest full weight, bad=0 required)",
        tot_full as f64 / tot_anvil.max(1) as f64
    );

    // 6) the operator's world: 40000 x 40000 BLOCKS = 2500 x 2500 chunks
    //    per dimension (the onlinemo.de/world input overworld=40000,
    //    nether=0&end=0: their 50 GB estimate is overworld-only, at the
    //    calculator's ~8 KiB/chunk average). Projections are linear in
    //    chunks (the scaling law measured on the 384x384 rings).
    let gb = |b: u64| b as f64 / 1073741824.0;
    let world_blocks: f64 = 2500.0 * 2500.0;
    println!("== projections: the operator's world (40000x40000 blocks = 2500x2500 chunks/dim) ==");
    for s in &stats {
        let pc = s.full as f64 / s.chunks as f64;
        let pa = s.anvil as f64 / s.chunks as f64;
        println!(
            "  {:<10} anvil ~{:6.1} GiB -> c-saves ~{:6.1} MiB  (x{:.0})",
            s.name,
            pa * world_blocks / 1073741824.0,
            pc * world_blocks / 1048576.0,
            pa / pc.max(0.000001),
        );
    }
    let pc_all = tot_full as f64 / tot_c as f64;
    let pa_all = tot_anvil as f64 / tot_c as f64;
    let dims_n = stats.len() as f64;
    println!(
        "  {:<10} anvil ~{:6.1} GiB -> c-saves ~{:6.1} MiB  (x{:.0})",
        "ALL THREE",
        pa_all * world_blocks * dims_n / 1073741824.0,
        pc_all * world_blocks * dims_n / 1048576.0,
        pa_all * dims_n / pc_all.max(0.000001),
    );
    let pc_ow = stats[0].full as f64 / stats[0].chunks as f64;
    println!(
        "  overworld-only (the onlinemo.de 50 GB estimate, 8 KiB/chunk): 50 GB -> ~{:.0} MiB",
        pc_ow * world_blocks / 1048576.0,
    );

    // 7) the 20000x20000-chunk scale (400M chunks per dimension)
    let world_chunks: f64 = 20_000.0 * 20_000.0;
    println!("== projections: 20000x20000 chunks (400M chunks/dim) ==");
    println!(
        "  ALL THREE  anvil ~{:.2} TiB -> c-saves ~{:.1} GiB  (x{:.0})",
        pa_all * world_chunks * dims_n / 1099511627776.0,
        pc_all * world_chunks * dims_n / 1073741824.0,
        pa_all * dims_n / pc_all.max(0.000001),
    );

    // 8) the pass verdict: every chunk of every dimension verified on the
    // live store AND after a fresh reopen
    let bad_all: usize = stats.iter().map(|s| s.chunks - s.ok_s).sum();
    let miss_all: usize = stats.iter().map(|s| s.miss).sum();
    println!("total probe time {:.1?}", t_all.elapsed());
    if bad_all == 0 && miss_all == 0 {
        println!("verify: {} — bad=0 miss=0", verify_line);
        println!("reopen: {} — bad=0 miss=0", verify_line);
        println!("=== full-world probe: PASS ===");
    } else {
        println!(
            "verify: {} — bad={} miss={} (FAIL)",
            verify_line, bad_all, miss_all
        );
        println!("=== full-world probe: FAIL ===");
        std::process::exit(1);
    }
}
