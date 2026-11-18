//! gid_dag_probe: price the GID-level delta representation (the real
//! DAG-lite candidate) against the shipped canon-lex baseline.
//!
//! Byte-level deltas priced +48.8% (dag_probe) — LZ77 on raw lex-adjacent
//! canon members beats explicit byte deltas decisively. But mountain
//! strata sections differ by VERTICAL SHIFTS: at the gid level (1 B/voxel)
//! a z-shift is a 16-BYTE shift and the XOR of two shifted strata streams
//! is sparse noise that RLE/LZ can collapse. This probe converts every
//! canon member to its gid stream (shared registry, section_to_glob —
//! the same conversion the pack's glob variant builds), runs the same
//! greedy best-ref delta model over the lex walk, and prices three
//! streams with the tuned LZMA2:
//!   canon_lex   — the shipped baseline (raw canon members, lex walk)
//!   gid_lex     — raw gid streams, lex walk (the glob-variant floor)
//!   gid_delta   — gid streams where each member may shrink to
//!                 [ref-idx placeholder][varint cp][varint cs][XOR mid]
//!
//! The bar for shipping a real gid-delta form: gid_delta < canon_lex
//! (the pack keeps whichever variant compresses smaller). Decode would
//! re-emit canon members from ref + delta — roundtrip machinery that only
//! pays off if this pricing clears the bar.
//!
//! Usage: gid_dag_probe <store-dir>

use c_saves::chunknbt::{section_to_glob, StateRegistry};
use c_saves::store::Store;
use std::path::PathBuf;

fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(
        liblzma::stream::PRESET_EXTREME | 6,
    )
    .ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

fn varint(v: usize, out: &mut Vec<u8>) {
    let mut x = v as u64;
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn delta_body(member: &[u8], ref_: &[u8]) -> Option<Vec<u8>> {
    let n = member.len().min(ref_.len());
    let mut cp = 0usize;
    while cp < n && member[cp] == ref_[cp] {
        cp += 1;
    }
    let mut cs = 0usize;
    while cs < n - cp && member[member.len() - 1 - cs] == ref_[ref_.len() - 1 - cs] {
        cs += 1;
    }
    let mid_m = &member[cp..member.len() - cs];
    let mid_r = &ref_[cp..ref_.len() - cs];
    if mid_m.len() != mid_r.len() {
        return None;
    }
    let mut out = Vec::with_capacity(16 + mid_m.len());
    varint(cp, &mut out);
    varint(cs, &mut out);
    for (a, b) in mid_m.iter().zip(mid_r) {
        out.push(a ^ b);
    }
    Some(out)
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: gid_dag_probe <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let mut canon: Vec<Vec<u8>> = members
        .into_iter()
        .filter(|(cls, _)| cls == "csec3")
        .map(|(_, b)| b)
        .collect();
    let n = canon.len();
    if n < 64 {
        println!("csec3 stream too small ({n}) — nothing to price");
        return;
    }
    canon.sort();
    let raw: usize = canon.iter().map(|b| b.len()).sum();
    let canon_lex: Vec<u8> = canon.concat();
    let c_x = lzma2_raw(&canon_lex).unwrap().len();
    println!("canon_lex: n={n} raw={raw} lz2={c_x}");

    // convert to gid streams with ONE shared registry (delta model only —
    // gids must be comparable across members)
    let mut reg = StateRegistry::default();
    let mut gids: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut failed = 0usize;
    for c in &canon {
        match section_to_glob(c, &mut reg) {
            Some(g) => gids.push(g),
            None => {
                failed += 1;
                gids.push(c.clone()); // conservative: stay raw
            }
        }
    }
    let graw: usize = gids.iter().map(|b| b.len()).sum();
    let gid_lex: Vec<u8> = gids.concat();
    let g_x = lzma2_raw(&gid_lex).unwrap().len();
    println!("gid_lex: raw={graw} lz2={g_x} (failed conversions: {failed})");

    // gid delta pass over the lex walk
    let mut stream: Vec<u8> = Vec::with_capacity(graw / 2);
    let mut deltas = 0usize;
    let window = 64usize;
    for (i, m) in gids.iter().enumerate() {
        let mut best: Option<Vec<u8>> = None;
        let lo = i.saturating_sub(window);
        for r in lo..i {
            if let Some(d) = delta_body(m, &gids[r]) {
                let cost = d.len() + 1;
                if cost < m.len() && best.as_ref().map(|b| cost < b.len()).unwrap_or(true) {
                    best = Some(d);
                }
            }
        }
        if let Some(d) = best {
            stream.push(0xFF); // ref-idx placeholder
            stream.extend_from_slice(&d);
            deltas += 1;
        } else {
            stream.extend_from_slice(m);
        }
    }
    let d_x = lzma2_raw(&stream).unwrap().len();
    println!(
        "gid_delta: deltas={deltas}/{n} lz2={d_x} ({:+.2}% vs gid_lex, {:+.2}% vs canon_lex SHIP BAR)",
        (d_x as f64 / g_x as f64 - 1.0) * 100.0,
        (d_x as f64 / c_x as f64 - 1.0) * 100.0
    );
    println!("GID-DAG-PROBE done");
}
