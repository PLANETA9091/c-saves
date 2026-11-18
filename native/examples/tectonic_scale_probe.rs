//! tectonic_scale_probe: the HONEST scaling curve for the huge-world ask.
//!
//! ONE 384x384 Tectonic world (the full biome mix: oceans left 5/8, beach
//! band, plains/river, mountain ridges right) imported into ONE store in
//! CUMULATIVE rings; after each ring the store drains and the container +
//! the marginal cost of the ring's chunks are measured:
//!   ring 0: [0..96)^2      (9,216 chunks)
//!   ring 1: [96..192)^2    (+27,648 chunks)
//!   ring 2: [192..288)^2   (+55,296)
//!   ring 3: [288..384)^2   (+82,944)
//! The curve answers the honest scaling question: do the repeat classes
//! saturate and the unique-member growth sublinear at scale? The 20k x 20k
//! projection = the WHOLE-world average per-chunk cost (a real pregen
//! covers the full mix) applied linearly to 400M chunks — cross-checked
//! against the 96x96 average from tectonic_probe.
//!
//! Usage: cargo run --release --example tectonic_scale_probe

#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::store::{Cfg, Store};
use std::path::PathBuf;

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

fn main() {
    let rings: [i32; 4] = [96, 192, 288, 384];
    let out = PathBuf::from(
        std::env::var("CSAVES_TECTONIC_OUT").unwrap_or_else(|_| "/tmp/tectonic_scale".to_string()),
    );
    let _ = std::fs::remove_dir_all(&out);
    let dir = out.join("store");
    let st = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();
    let mut builder = tc::Builder::new(rings[3]);

    let mut prev = 0u64;
    let mut cum_chunks = 0usize;
    println!("ring | ring chunks | container (cum) | ring marginal | marginal B/chunk");
    for (ri, &half) in rings.iter().enumerate() {
        let lo = if ri == 0 { 0 } else { rings[ri - 1] };
        let t0 = std::time::Instant::now();
        let mut ring_chunks = 0usize;
        for cz in 0..half {
            for cx in 0..half {
                if cx < lo && cz < lo {
                    continue; // the previous ring's square
                }
                let k = ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff);
                let (p, _) = builder.payload(cx, cz);
                st.write_chunk_import(k, &p).unwrap();
                ring_chunks += 1;
            }
        }
        drain(&st);
        let container = st.disk_bytes();
        let marginal = container - prev;
        let per_chunk = marginal as f64 / ring_chunks.max(1) as f64;
        println!(
            "ring {ri} [{lo:>3}..{half:>3}) | {ring_chunks:>6} | {} B | {marginal:>9} B | {per_chunk:7.1} B/chunk  ({} ms)",
            container,
            t0.elapsed().as_millis(),
        );
        prev = container;
        cum_chunks += ring_chunks;
    }
    let container = st.disk_bytes();
    drop(st);
    let st2 = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();
    let full = st2.full_disk_bytes();
    let avg = container as f64 / cum_chunks.max(1) as f64;
    let world = 20_000.0f64 * 20_000.0;
    println!("cumulative: {cum_chunks} chunks | segs {} | full (incl. states) {} B", container, full);
    println!(
        "384x384 average: {:.1} B/chunk -> 20k x 20k = 400M chunks: ~{:.1} GiB vs Anvil ~1.49 TiB (x{:.0})",
        avg,
        world * avg / 1073741824.0,
        4104.0 / avg,
    );
    println!("{}", st2.slab_member_attribution_json());
}
