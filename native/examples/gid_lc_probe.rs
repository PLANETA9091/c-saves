//! gid_lc_probe: LZMA2 lc/lp sweep on the REAL narrow gid streams. The
//! codec axis was closed on u8 streams (preset ladder flat, lc4 the tune),
//! but the 0.9.118 narrow 4-bit packing is a NEW representation: two voxels
//! per byte, nibble-phase-structured. lp=1 aligns literal contexts to the
//! nibble phase; lc4 then sees (phase, prev-high-nibble) contexts. This
//! probe extracts every glob_g26 (narrow) member payload from a drained
//! store, concatenates in slab order, and prices lc x lp at the tuned
//! preset. Also prices the per-member split (each member its own LZMA2
//! frame) as the honest per-member floor reference.
//!
//! Usage: gid_lc_probe <store-dir>

use c_saves::store::Store;
use liblzma::stream as xs;
use std::path::PathBuf;

fn lzma2_cfg(data: &[u8], lc: u32, lp: u32, pb: u32) -> Option<usize> {
    let mut opts = xs::LzmaOptions::new_preset(xs::PRESET_EXTREME | 6).ok()?;
    opts.literal_context_bits(lc)
        .literal_position_bits(lp)
        .position_bits(pb);
    let mut f = xs::Filters::new();
    f.lzma2(&opts);
    let mut stream = xs::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, xs::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, xs::Action::Finish).ok()?;
    Some(out.len())
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: gid_lc_probe <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let mut payload = Vec::new();
    let mut n = 0usize;
    let mut raw = 0usize;
    let mut per_member: Vec<Vec<u8>> = Vec::new();
    for (c, b) in members {
        if c != "glob_g26" {
            continue;
        }
        // [0xCC][f1][f2c][f3][y][gform][payload]
        if b.len() > 6 {
            let p = &b[6..];
            payload.extend_from_slice(p);
            per_member.push(p.to_vec());
            raw += p.len();
            n += 1;
        }
    }
    println!("g26 members {n}, payload raw {raw} B");
    let mut best = (usize::MAX, 0u32, 0u32);
    for lc in 0..=4u32 {
        for lp in 0..=2u32 {
            if lp > 0 && lc + lp as u32 > 4 {
                continue; // lzma spec: lc + lp <= 4
            }
            let c = lzma2_cfg(&payload, lc, lp, 0).unwrap_or(usize::MAX);
            println!("lc{lc} lp{lp} pb0  -> {:8}  ({:.2}%)", c, 100.0 * c as f64 / raw as f64);
            if c < best.0 {
                best = (c, lc, lp);
            }
        }
    }
    println!("BEST lc{} lp{} -> {}", best.1, best.2, best.0);
    // reference: tuned lc4 lp0 pb0 (the shipped config)
    let tuned = lzma2_cfg(&payload, 4, 0, 0).unwrap();
    println!("tuned lc4 lp0: {tuned}");
    // per-member floor: sum of individually compressed members (reference only)
    let mut pm = 0usize;
    for m in per_member.iter().take(400) {
        pm += lzma2_cfg(m, 4, 0, 0).unwrap_or(m.len());
    }
    println!("per-member floor (first 400 members): {pm}");
}
