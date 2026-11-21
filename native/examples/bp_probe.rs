//! bp_probe: bit-plane ordering of the g6 gid streams — a NEW representation
//! axis, probed on the real drained Tectonic full-dimension corpus (the
//! overworld store; the g6 class carries the corpus's biggest raw share).
//!
//! Every 0xCC/gform-6 member carries a 4096-B y-fast-transposed u8 gid
//! stream. The registry is frequency-canonical (gid 0 = dominant state), so
//! most voxels sit in low gids: the HIGH bit-planes are almost all zero.
//! LZMA2 sees the interleaved bytes today; separating the planes turns each
//! sparse plane into long zero-runs the match finder collapses. Variants
//! priced with the EXACT cold-tier config (LZMA2 preset 6|EXTREME,
//! lc=4 lp=0 pb=0) over the member-concatenated streams:
//!   A status quo: payload bytes as stored (transposed interleaved)
//!   B per-member LSB-first bit-planes (p0||..||p7) of the stored stream
//!   C per-member MSB-first bit-planes (p7||..||p0)
//!   D per-member bit-planes of the VANILLA-order stream (un-transposed;
//!     equals re-transposing each plane individually — the transpose is an
//!     involution, so plane-of-transposed re-transposed = plane-of-vanilla)
//!   E global plane-major: every member's p7 first, then p6, ... (drops the
//!     per-member framing — bounds the idea from above)
//!   F per-member nibble split (hi-nibble 2048 B || lo-nibble 2048 B)
//! Plus per-plane bit-census (zero ratio + nonzero-voxel count) and the
//! per-member max-gid histogram — the mechanism behind whatever wins.
//!
//! Usage: cargo run --release --example bp_probe -- <store-dir>

use c_saves::store::{Cfg, Store};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Mirror of the store's tuned cold config (preset 6|EXTREME, lc4/lp0/pb0).
fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
    let mut opts =
        liblzma::stream::LzmaOptions::new_preset(liblzma::stream::PRESET_EXTREME | 6).ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream
        .process_vec(data, &mut out, liblzma::stream::Action::Run)
        .ok()?;
    stream
        .process_vec(&[], &mut out, liblzma::stream::Action::Finish)
        .ok()?;
    Some(out)
}

/// y-fast transpose involution (side=16, s2=256): out[x*256 + z*16 + y] =
/// v[y*256 + z*16 + x]; applying it twice is the identity.
fn transpose(v: &[u8]) -> Vec<u8> {
    const SIDE: usize = 16;
    let s2 = SIDE * SIDE;
    let mut out = vec![0u8; v.len()];
    for y in 0..SIDE {
        for z in 0..SIDE {
            for x in 0..SIDE {
                out[x * s2 + z * SIDE + y] = v[y * s2 + z * SIDE + x];
            }
        }
    }
    out
}

/// Extract bit-plane p of a u8 stream: one packed byte per 8 voxels,
/// MSB-first within each byte (bit p of byte i -> bit (7 - i%8) of plane
/// byte i/8). 4096 voxels -> 512 bytes per plane.
fn bit_plane(v: &[u8], p: u32) -> Vec<u8> {
    let mut out = vec![0u8; v.len() / 8];
    for (i, &b) in v.iter().enumerate() {
        if (b >> p) & 1 == 1 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

fn nibble_split(v: &[u8]) -> Vec<u8> {
    let mut hi = Vec::with_capacity(v.len());
    let mut lo = Vec::with_capacity(v.len());
    for &b in v {
        hi.push(b & 0xf0);
        lo.push((b & 0x0f) << 4);
    }
    hi.extend_from_slice(&lo);
    hi
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: bp_probe <store-dir>"));
    let st = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    let members = st.slab_members_by_class();

    // g6 members: [0xCC][f1][f2][f3][Y][gform=6][4096-B payload][tail]
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut class_maxgid: BTreeMap<u8, usize> = BTreeMap::new();
    for (_cls, m) in &members {
        if m.first() != Some(&0xCC) || m.get(5) != Some(&6) || m.len() < 6 + 4096 {
            continue;
        }
        let p = m[6..6 + 4096].to_vec();
        let maxg = p.iter().copied().max().unwrap_or(0);
        *class_maxgid.entry(maxg).or_default() += 1;
        payloads.push(p);
    }
    let n = payloads.len();
    if n == 0 {
        println!("bp_probe: no g6 members found");
        return;
    }
    let raw: usize = payloads.iter().map(|p| p.len()).sum();
    println!("g6 members: {n} raw={raw} B ({:.1} B/member)", raw as f64 / n as f64);
    let mut hist: Vec<(u8, usize)> = class_maxgid.into_iter().collect();
    hist.sort_by_key(|(g, _)| *g);
    println!("max-gid histogram (bucketed): {:?}", {
        let mut buckets: BTreeMap<u8, usize> = BTreeMap::new();
        for (g, c) in hist {
            let b = if g < 16 { 15 } else if g < 32 { 31 } else if g < 64 { 63 } else if g < 128 { 127 } else { 255 };
            *buckets.entry(b).or_default() += c;
        }
        buckets.into_iter().collect::<Vec<_>>()
    });

    // A: status quo
    let mut a = Vec::with_capacity(raw);
    for p in &payloads {
        a.extend_from_slice(p);
    }

    // B/C/D/F per-member transforms
    let mut b = Vec::with_capacity(raw);
    let mut c = Vec::with_capacity(raw);
    let mut d = Vec::with_capacity(raw);
    let mut f = Vec::with_capacity(raw);
    for p in &payloads {
        let vanilla = transpose(p);
        for plane in 0..8u32 {
            b.extend_from_slice(&bit_plane(p, plane));
            d.extend_from_slice(&bit_plane(&vanilla, plane));
        }
        for plane in (0..8u32).rev() {
            c.extend_from_slice(&bit_plane(p, plane));
        }
        f.extend_from_slice(&nibble_split(p));
    }

    // E: global plane-major across members
    let mut e: Vec<Vec<u8>> = (0..8).map(|_| Vec::with_capacity(raw / 8)).collect();
    for p in &payloads {
        for plane in 0..8u32 {
            e[plane as usize].extend_from_slice(&bit_plane(p, plane));
        }
    }
    let mut e_all = Vec::with_capacity(raw);
    for plane in e.into_iter().rev() {
        e_all.extend_from_slice(&plane);
    }

    let a_c0 = lzma2_raw(&a).unwrap().len();
    let price = |name: &str, data: &[u8]| {
        let comp = lzma2_raw(data).map(|c| c.len()).unwrap_or(0);
        println!(
            "{name}: raw={} comp={} ({:.2}% of A)",
            data.len(),
            comp,
            comp as f64 / a_c0 as f64 * 100.0
        );
        comp
    };
    let a_c = price("A as-stored-transposed", &a);

    // G: NARROW gid streams — pack the transposed stream at exactly
    // bits_for(max_gid) bits per voxel (nibble 4 / pair 2 / bit 1). The
    // census above shows every member's max_gid <= 15 on this corpus, so
    // half of the raw stream is structurally zero today.
    let narrow = |bits: u32| -> Vec<u8> {
        let mut out = Vec::new();
        for p in &payloads {
            let cur = p.iter().map(|&g| g as u64).max().unwrap_or(0);
            let need = 16 - (cur as u32).leading_zeros().max(1);
            let use_bits = need.min(bits);
            let mut acc: u64 = 0;
            let mut nacc: u32 = 0;
            for &g in p {
                acc = (acc << use_bits) | (g as u64 & ((1u64 << use_bits) - 1));
                nacc += use_bits;
                while nacc >= 8 {
                    nacc -= 8;
                    out.push((acc >> nacc) as u8);
                }
            }
            if nacc > 0 {
                out.push((acc << (8 - nacc)) as u8);
            }
        }
        out
    };
    let g4_c = price("G 4-bit narrow (all members)", &narrow(4));
    let g2_c = price("H 2-bit narrow (lossy here)", &narrow(2));
    let g1_c = price("I 1-bit narrow (lossy here)", &narrow(1));

    // J: the HONEST per-member narrow — width = bits_for(max_gid) per
    // member (1/2/4 bits), no masking, same transposed order. This is what
    // a content-deterministic narrow gform would ship.
    let mut j = Vec::new();
    let mut fine: BTreeMap<u32, usize> = BTreeMap::new();
    for p in &payloads {
        let cur = p.iter().map(|&g| g as u64).max().unwrap_or(0);
        let need = (64 - cur.leading_zeros()).max(1) as u32; // bits_for(max_gid)
        *fine.entry(need).or_default() += 1;
        let mut acc: u64 = 0;
        let mut nacc: u32 = 0;
        for &g in p {
            acc = (acc << need) | g as u64;
            nacc += need;
            while nacc >= 8 {
                nacc -= 8;
                j.push((acc >> nacc) as u8);
            }
        }
        if nacc > 0 {
            j.push((acc << (8 - nacc)) as u8);
        }
    }
    println!("fine width histogram (bits -> members): {:?}", fine.into_iter().collect::<Vec<_>>());
    let j_c = price("J per-member honest narrow", &j);

    // J2: byte-ALIGNED per-member narrow — the ladder widths only (1/2/4
    // bits; anything wider stays u8). No bit-residue misalignment: every
    // member is a whole number of bytes at its own width. This is the
    // ladder the wire design actually ships.
    let mut j2 = Vec::new();
    let mut j2_narrow = 0usize;
    for p in &payloads {
        let cur = p.iter().map(|&g| g as u64).max().unwrap_or(0);
        let w = if cur <= 1 { 1 } else if cur <= 3 { 2 } else if cur <= 15 { 4 } else { 8 };
        if w < 8 {
            j2_narrow += 1;
        }
        let mut acc: u64 = 0;
        let mut nacc: u32 = 0;
        for &g in p {
            acc = (acc << w) | g as u64;
            nacc += w;
            while nacc >= 8 {
                nacc -= 8;
                j2.push((acc >> nacc) as u8);
            }
        }
        if nacc > 0 {
            j2.push((acc << (8 - nacc)) as u8);
        }
    }
    let j2_c = price("J2 per-member BYTE-ALIGNED narrow", &j2);
    println!("J2: {j2_narrow}/{n} members narrow ({:.1}% of A)", j2_c as f64 / a_c as f64 * 100.0);
    println!(
        "narrow winner: {:.1}% of A",
        [g4_c, g2_c, g1_c, j_c].iter().min().copied().unwrap_or(a_c) as f64 / a_c as f64 * 100.0
    );

    // K/L: plain-order (un-transposed) control — the base variant wire:
    // narrow must beat u8 in BOTH orders for the group-replacement design
    let mut k = Vec::new();
    let mut l = Vec::new();
    for p in &payloads {
        let van = transpose(p);
        k.extend_from_slice(&van);
        let cur = van.iter().map(|&g| g as u64).max().unwrap_or(0);
        let need = (64 - cur.leading_zeros()).max(1) as u32;
        if need < 8 {
            let mut acc: u64 = 0;
            let mut nacc: u32 = 0;
            for &g in &van {
                acc = (acc << need) | g as u64;
                nacc += need;
                while nacc >= 8 {
                    nacc -= 8;
                    l.push((acc >> nacc) as u8);
                }
            }
            if nacc > 0 {
                l.push((acc << (8 - nacc)) as u8);
            }
        } else {
            l.extend_from_slice(&van);
        }
    }
    let k_c = price("K u8 plain-order", &k);
    let l_c = price("L narrow plain-order", &l);
    println!("plain-order narrow vs u8: {:.1}%", l_c as f64 / k_c as f64 * 100.0);

    let b_c = price("B planes-lsb (transposed)", &b);
    let c_c = price("C planes-msb (transposed)", &c);
    let d_c = price("D planes (vanilla order)", &d);
    let e_c = price("E global plane-major", &e_all);
    let f_c = price("F nibble split", &f);
    println!("winner: {:.1}% of A", [b_c, c_c, d_c, e_c, f_c].iter().min().copied().unwrap_or(a_c) as f64 / a_c as f64 * 100.0);

    // per-plane bit census over the stored streams
    println!("plane census (stored transposed streams):");
    for plane in 0..8u32 {
        let mut ones = 0usize;
        let mut zero_bytes = 0usize;
        for p in &payloads {
            let pl = bit_plane(p, plane);
            zero_bytes += pl.iter().filter(|&&x| x == 0).count();
            ones += pl.iter().map(|b| b.count_ones() as usize).sum::<usize>();
        }
        let bits = n * 4096;
        println!(
            "  p{plane}: ones={ones}/{bits} ({:.1}% zero) zero-planes-entire={} ({:.0}%)",
            100.0 * (1.0 - ones as f64 / bits as f64),
            zero_bytes / 512,
            100.0 * (zero_bytes / 512) as f64 / n as f64
        );
    }
}
