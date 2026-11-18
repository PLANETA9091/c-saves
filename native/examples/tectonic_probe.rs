//! tectonic_probe: honest huge-world numbers for the Tectonic-shaped ask —
//! "even a 20k x 20k chunk world must weigh very little".
//!
//! The corpus is STRUCTURAL mimicry, not worldgen: it reproduces the
//! compressible structure of a real Tectonic pregen (v3):
//!   - vast bare-ocean fields whose chunks are byte-identical except the
//!     xPos/zPos pair (the 0.9.102 pos-free-tail lever eats them),
//!   - sparse decoration variants (kelp fields, trees, flower patches) that
//!     REPEAT across coordinates — the way real feature placement does,
//!   - structure starts (shipwrecks) whose tail NBT carries chunkX/chunkZ —
//!     they honestly BREAK recipe identity the way vanilla does,
//!   - mountain ridges whose full band sections repeat while only the
//!     surface bands and heightmaps are unique.
//! Every proportion is a deterministic hash draw over the printed mix; the
//! honest claim is per-class marginal cost, the 20k x 20k table is a linear
//! extrapolation of measured bytes.
//!
//! Baselines measured on disk, not estimated:
//!   - Anvil: the SAME payloads written as real .mca region files
//!     (zlib-9, the strongest honest vanilla treatment),
//!   - c-saves: the production import path (write_chunk_import, DEFAULT
//!     cfg = real storage; the pristine/contract regeneration mode stays
//!     OFF by operator directive) driven through the full drain.
//!
//! Usage: cargo run --release --example tectonic_probe [-- <cx> <cz>]

use c_saves::chunknbt::hash_of;
use c_saves::store::{Cfg, Store};
use fastnbt::{LongArray, Value};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap as Map;
use std::io::Write;
use std::path::{Path, PathBuf};

#[path = "inc/tectonic_corpus.rs"]
mod tc;
use tc::*;


fn main() {
    let cx_w: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(96);
    let cz_w: i32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(96);
    let out = PathBuf::from(
        std::env::var("CSAVES_TECTONIC_OUT").unwrap_or_else(|_| "/tmp/tectonic_world".to_string()),
    );
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(out.join("region")).unwrap();

    // 1) build every payload (shared shapes patched per coordinate)
    let t0 = std::time::Instant::now();
    let mut b = Builder::new(cx_w);
    let mut payloads: Vec<(u64, Vec<u8>, Class)> = Vec::with_capacity((cx_w * cz_w) as usize);
    let mut class_n: Map<&'static str, usize> = Map::new();
    let mut raw = 0u64;
    for cz in 0..cz_w {
        for cx in 0..cx_w {
            let (p, cls) = b.payload(cx, cz);
            raw += p.len() as u64;
            *class_n.entry(cls.name()).or_default() += 1;
            payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p, cls));
        }
    }
    println!("== plan ==");
    println!(
        "chunks {}x{} = {} | gen {:.1?} | raw {:.2} MiB ({:.2} KiB/chunk)",
        cx_w,
        cz_w,
        payloads.len(),
        t0.elapsed(),
        raw as f64 / 1048576.0,
        raw as f64 / 1024.0 / payloads.len() as f64
    );
    let mut classes: Vec<(&'static str, usize)> = class_n.into_iter().collect();
    classes.sort_by_key(|c| std::cmp::Reverse(c.1));
    for (k, v) in &classes {
        println!("  {:<14} {:>6} ({:4.1}%)", k, v, 100.0 * *v as f64 / payloads.len() as f64);
    }

    // 2) Anvil baseline: real region files of the same payloads (zlib-9)
    let mut regions: Map<(i32, i32), Vec<(usize, usize)>> = Map::new();
    for (i, (k, _, _)) in payloads.iter().enumerate() {
        let cx = (k >> 32) as i32;
        let cz = (k & 0xffff_ffff) as u32 as i32;
        regions
            .entry((cx.div_euclid(32), cz.div_euclid(32)))
            .or_default()
            .push((((cx.rem_euclid(32)) + (cz.rem_euclid(32)) * 32) as usize, i));
    }
    let mut anvil = 0u64;
    let mut rkeys: Vec<(i32, i32)> = regions.keys().copied().collect();
    rkeys.sort();
    for rk in &rkeys {
        let slots: Vec<(usize, &[u8])> = regions[rk]
            .iter()
            .map(|(s, i)| (*s, payloads[*i].1.as_slice()))
            .collect();
        anvil += write_region(&out.join("region"), rk.0, rk.1, &slots);
    }
    println!("== anvil ==");
    println!(
        "{} region files: {:.2} MiB on disk ({:.0} B/chunk) — zlib-9, real bytes",
        rkeys.len(),
        anvil as f64 / 1048576.0,
        anvil as f64 / payloads.len() as f64
    );

    // 3) c-saves: production import path (default cfg = REAL storage; the
    //    contract/regeneration mode stays off by operator directive)
    let store_dir = out.join("store");
    let st = Store::open_cfg(&store_dir, true, Cfg::from_env()).unwrap();
    let t0 = std::time::Instant::now();
    let mut keys: Vec<u64> = payloads.iter().map(|(k, _, _)| *k).collect();
    keys.sort();
    let by_key: Map<u64, usize> =
        payloads.iter().enumerate().map(|(i, (k, _, _))| (*k, i)).collect();
    for &k in &keys {
        let p = &payloads[by_key[&k]].1;
        st.write_chunk_import(k, p).unwrap();
    }
    let import_t = t0.elapsed();
    let t0 = std::time::Instant::now();
    st.sync();
    st.seal();
    st.request_pack();
    // the maintenance thread keeps stepping in production; mirror that here
    // (a single while-loop can exit through the fallback path before the
    // dead-ratio rewrite gets its turn — observed on the NOHM A/B leg)
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
    let pack_t = t0.elapsed();
    let container = st.disk_bytes();
    println!("== c-saves ==");
    println!(
        "import {:.1?} ({:.1} MiB/s) | drain {:.1?} | container {:.2} MiB = {} B",
        import_t,
        raw as f64 / 1048576.0 / import_t.as_secs_f64(),
        pack_t,
        container as f64 / 1048576.0,
        container
    );
    println!(
        "{:.1} B/chunk | ratio vs anvil x{:.2}",
        container as f64 / payloads.len() as f64,
        anvil as f64 / container.max(1) as f64
    );

    // 4) verify every read against the generated payload. The equality
    //    contract is the CI verify's own: compound fields as plain Values,
    //    sections via the palette-permutation-safe comparator (the canonical
    //    CSEC re-encode reorders palettes by frequency — semantically
    //    neutral). Byte-exact is reported separately.
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
    let t0 = std::time::Instant::now();
    let (mut ok_s, mut ok_b, mut miss) = (0usize, 0usize, 0usize);
    for &k in &keys {
        let want = &payloads[by_key[&k]].1;
        match st.read_chunk(k) {
            Some(got) => {
                if chunks_semantic(want, &got) {
                    ok_s += 1;
                }
                if got == *want {
                    ok_b += 1;
                }
            }
            None => miss += 1,
        }
    }
    println!(
        "verify: {}/{} semantic OK ({} byte-exact, {} miss) in {:.1?}",
        ok_s,
        payloads.len(),
        ok_b,
        miss,
        t0.elapsed()
    );

    // 5) per-class drained attribution
    println!("== slab attribution ==");
    println!("{}", st.slab_member_attribution_json());
    println!("== profile ==");
    println!("{}", st.profile_json());

    // 6) heightmap columnar pricing on THIS corpus: the tail wire (root
    //    minus sections) with vs without the Heightmaps compound, both under
    //    one big LZMA2 window; unique heightmap wires columnar + 4 B refs.
    let mut tails_all: Vec<u8> = Vec::new();
    let mut tails_no: Vec<u8> = Vec::new();
    let mut hm_seen: Map<u128, ()> = Map::new();
    let mut hm_uniq: Vec<Vec<u8>> = Vec::new();
    let mut n_hm = 0usize;
    let mut per_chunk_z = 0usize;
    for (k, _, _) in payloads.iter() {
        let i = by_key[k];
        let mut root: Value = fastnbt::from_bytes(&payloads[i].1).unwrap();
        // strip sections first: the tail wire is the root compound minus them
        if let Value::Compound(m) = &mut root {
            m.remove("sections");
        }
        tails_all.extend_from_slice(&fastnbt::to_bytes(&root).unwrap());
        if let Value::Compound(m) = &mut root {
            let hm = m.remove("Heightmaps");
            if let Some(hm) = hm {
                let w = fastnbt::to_bytes(&hm).unwrap();
                per_chunk_z += z19(&w);
                let h = (hash_of(&w) as u128) << 64 | w.len() as u128;
                if !hm_seen.contains_key(&h) {
                    hm_seen.insert(h, ());
                    hm_uniq.push(w);
                }
                n_hm += 1;
            }
        }
        tails_no.extend_from_slice(&fastnbt::to_bytes(&root).unwrap());
    }
    let lz_all = lzma2_raw(&tails_all).map(|b| b.len()).unwrap_or(0);
    let lz_no = lzma2_raw(&tails_no).map(|b| b.len()).unwrap_or(0);
    let hm_col: Vec<u8> = hm_uniq.concat();
    let lz_hm = lzma2_raw(&hm_col).map(|b| b.len()).unwrap_or(0);
    let refs = 4 * n_hm;
    println!("== heightmap columnar pricing ==");
    println!(
        "tails: {} chunks, all {:.2} KiB -> lz {} B; no-hm {:.2} KiB -> lz {} B",
        payloads.len(),
        tails_all.len() as f64 / 1024.0,
        lz_all,
        tails_no.len() as f64 / 1024.0,
        lz_no
    );
    println!(
        "heightmaps: {n_hm} carry, {} unique, sum z19 {per_chunk_z} B; unique-columnar lz {lz_hm} B + refs {refs} B = {} B",
        hm_uniq.len(),
        lz_hm + refs,
    );
    println!(
        "tier delta under big-window model: lz_all {} vs (lz_no {} + col {}) = {} -> {:+} B ({:.2}%)",
        lz_all,
        lz_no,
        lz_hm + refs,
        lz_no + lz_hm + refs,
        (lz_no + lz_hm + refs) as i64 - lz_all as i64,
        100.0 * ((lz_no + lz_hm + refs) as i64 - lz_all as i64) as f64 / lz_all.max(1) as f64
    );

    // 7) honest extrapolation: linear per-chunk marginal on the measured
    //    container (the mix, not the area, decides the mass)
    let per_chunk = container as f64 / payloads.len() as f64;
    let anvil_pc = anvil as f64 / payloads.len() as f64;
    let world = 20_000.0f64 * 20_000.0;
    println!("== 20k x 20k extrapolation (same mix, linear) ==");
    println!("measured: {:.0} B/chunk c-saves vs {:.0} B/chunk anvil", per_chunk, anvil_pc);
    println!(
        "20k x 20k = 400M chunks: c-saves ~= {:.1} GiB | anvil ~= {:.2} TiB (x{:.0})",
        per_chunk * world / 1073741824.0,
        anvil_pc * world / 1099511627776.0,
        anvil_pc / per_chunk
    );
}
