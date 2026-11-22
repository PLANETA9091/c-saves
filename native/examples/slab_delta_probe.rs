//! slab_delta_probe: the inter-SECTION delta chain — the one untried axis
//! on the g26 class (the 0.9.120 overworld slab mass: n=4994, raw 10.26 MB,
//! ~166 KB LZMA2 = 68% of the overworld close weight at 192x192).
//!
//! The lex content-similarity walk (v0.9.106) already clusters same-palette
//! members adjacently and LZMA2 matches their shared prefixes. The untried
//! question: is the RESIDUAL after lex clustering better expressed as an
//! explicit vixdel chain — member i encoded as voxel-wise zigzag SUB against
//! member i-1, RLE'd — compressed as one frame? Tectonic pregen oceans/
//! plains share strata: consecutive lex members should differ in few voxels.
//!
//! A/B/C (all whole-frame cold-tier LZMA2, lc=4 lp=0 pb=0):
//!   A: repacked narrow payloads concatenated (the shipped wire, the fact)
//!   B: delta chain, RLE (run-len, zigzag-val) pairs per member
//!   C: delta chain, [varint skip][varint run][val] sparse blocks per member
//! plus the zero-delta census that decides the design.
//!
//! Usage: cargo run --release --example slab_delta_probe [footprint]
use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;

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

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn lzma2_cold(data: &[u8]) -> Vec<u8> {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(6 | liblzma::stream::PRESET_EXTREME)
        .expect("preset");
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).expect("encoder");
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).expect("run");
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).expect("finish");
    out
}

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

fn zig(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// unpack a narrow payload at wsel bits, MSB-first, to a 4096 u8 gid stream
fn unpack_narrow(p: &[u8], bits: u32, vox: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(vox);
    let mask: u8 = ((1u16 << bits) - 1) as u8;
    let mut cur: u32 = 0;
    let mut nb: u32 = 0;
    let mut it = p.iter();
    for _ in 0..vox {
        while nb < bits {
            cur = (cur << 8) | (*it.next().unwrap_or(&0) as u32);
            nb += 8;
        }
        let g = ((cur >> (nb - bits)) & (mask as u32)) as u8;
        nb -= bits;
        out.push(g);
    }
    out
}

/// B-wire: RLE of (run, zigzag delta) — deltas repeated in runs
fn delta_wire_runs(d: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() / 2 + 8);
    let mut i = 0usize;
    while i < d.len() {
        let v = d[i];
        let mut n = 1usize;
        while i + n < d.len() && d[i + n] == v {
            n += 1;
        }
        put_uvarint(&mut out, n as u64);
        put_uvarint(&mut out, zig(v as i64));
        i += n;
    }
    out
}

/// C-wire: sparse [varint skip][varint run][zigzag val] blocks — zeros skipped
fn delta_wire_sparse(d: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() / 8 + 8);
    let mut i = 0usize;
    while i < d.len() {
        if d[i] == 0 {
            let mut n = 0usize;
            while i + n < d.len() && d[i + n] == 0 {
                n += 1;
            }
            i += n;
            continue;
        }
        let mut run = 1usize;
        while i + run < d.len() && d[i + run] == d[i] {
            run += 1;
        }
        put_uvarint(&mut out, i as u64); // absolute skip — decoder walks linearly
        put_uvarint(&mut out, run as u64);
        put_uvarint(&mut out, zig(d[i] as i64));
        i += run;
    }
    put_uvarint(&mut out, u64::MAX / 2); // end marker: skip = sentinel
    put_uvarint(&mut out, 0);
    put_uvarint(&mut out, 0);
    out
}

fn main() {
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(96);
    println!("== slab_delta_probe: the inter-section delta chain (footprint {f}x{f}) ==");
    let mut b = tc::Builder::new(f);
    let dir = std::env::temp_dir().join(format!(
        "csaves-sdprobe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for cz in 0..f {
        for cx in 0..f {
            let (p, _c) = b.payload(cx, cz);
            st.write_chunk(key(cx, cz), &p, false).unwrap();
        }
    }
    drain(&st);
    let disk = st.disk_bytes();
    let members = st.slab_members_by_class();
    drop(st);
    println!("drained disk={disk} B");

    let mut by_class: Map<String, Vec<Vec<u8>>> = Map::new();
    for (cls, body) in &members {
        by_class.entry(cls.clone()).or_default().push(body.clone());
    }
    let mut cls_sorted: Vec<_> = by_class.iter().collect();
    cls_sorted.sort_by_key(|(c, v)| (std::cmp::Reverse(v.iter().map(|x| x.len()).sum::<usize>()), c.clone()));
    println!("-- class census --");
    for (c, v) in &cls_sorted {
        let raw: u64 = v.iter().map(|x| x.len() as u64).sum();
        println!("{:<14} n={:<6} raw={raw}", c, v.len());
    }

    // the narrow classes: gform 10|wsel<<4 -> wsel bits (1 -> 4b, 2 -> 2b, 3 -> 1b)
    let narrow_bits = |gform: u8| match gform >> 4 {
        1 => 4u32,
        2 => 2,
        3 => 1,
        _ => 0,
    };
    for cls in ["glob_g26", "glob_g42", "glob_g58"] {
        let Some(v) = by_class.get(cls) else { continue };
        let bits: u32 = match cls.as_ref() {
            "glob_g26" => 4,
            "glob_g42" => 2,
            _ => 1,
        };
        let _ = narrow_bits; // silence unused in the non-narrow builds
        // header [0xCC][f1][f2'][f3][y][gform] then payload
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        for body in v {
            if body.len() >= 7 && body[5] >> 4 == 1 && bits == 4 {
                payloads.push(body[6..].to_vec());
            } else if body.len() >= 7 && bits != 4 {
                payloads.push(body[6..].to_vec());
            }
        }
        if payloads.is_empty() {
            println!("{cls}: no narrow members");
            continue;
        }
        // lex order (the pack's content-similarity walk, applied to bodies incl. headers)
        let mut bodies: Vec<Vec<u8>> = v.clone();
        bodies.sort();
        // unpack to gid streams in lex order
        let vox = (payloads[0].len() * 8 / bits as usize).max(1);
        let streams: Vec<Vec<u8>> = bodies
            .iter()
            .map(|bd| unpack_narrow(&bd[6..], bits as u32, vox))
            .collect();
        let n = streams.len();
        let raw_full: usize = bodies.iter().map(|x| x.len()).sum();

        // A: the shipped wire — packed bodies, whole-frame
        let a_frame_len: usize = bodies.iter().map(|x| x.len()).sum();
        let a = lzma2_cold(&streams.iter().flat_map(|s| s.iter().copied()).collect::<Vec<u8>>());

        // zero-delta census (voxels equal to the lex-previous member)
        let mut zero = 0u64;
        let mut tot = 0u64;
        for w in streams.windows(2) {
            for i in 0..vox {
                tot += 1;
                if w[0][i] == w[1][i] {
                    zero += 1;
                }
            }
        }

        // B: the delta chain, RLE (run, zigzag)
        let mut b_wire: Vec<u8> = Vec::with_capacity(raw_full / 2);
        let mut first_wire: Vec<u8> = Vec::new();
        {
            let s = &streams[0];
            let mut i = 0usize;
            while i < vox {
                let g = s[i];
                let mut rn = 1usize;
                while i + rn < vox && s[i + rn] == g {
                    rn += 1;
                }
                put_uvarint(&mut first_wire, rn as u64);
                put_uvarint(&mut first_wire, g as u64);
                i += rn;
            }
        }
        b_wire.extend_from_slice(&first_wire);
        for w in streams.windows(2) {
            let d: Vec<i16> = (0..vox).map(|i| w[1][i] as i16 - w[0][i] as i16).collect();
            b_wire.extend_from_slice(&delta_wire_runs(&d));
        }
        let b = lzma2_cold(&b_wire);

        // C: the delta chain, sparse blocks
        let mut c_wire: Vec<u8> = Vec::with_capacity(raw_full / 2);
        c_wire.extend_from_slice(&first_wire);
        for w in streams.windows(2) {
            let d: Vec<i16> = (0..vox).map(|i| w[1][i] as i16 - w[0][i] as i16).collect();
            c_wire.extend_from_slice(&delta_wire_sparse(&d));
        }
        let c = lzma2_cold(&c_wire);

        println!("-- {cls}: n={n} raw={raw_full} vox/member={vox} bits={bits} --");
        println!("   A packed+LZMA2  = {:>8} B (x{:.2})", a.len(), raw_full as f64 / a.len() as f64);
        println!(
            "   B deltaRLE+LZMA2= {:>8} B (x{:.2})  vs A {:+.1}%",
            b.len(),
            raw_full as f64 / b.len() as f64,
            100.0 * (b.len() as f64 / a.len() as f64 - 1.0)
        );
        println!(
            "   C sparse+LZMA2  = {:>8} B (x{:.2})  vs A {:+.1}%",
            c.len(),
            raw_full as f64 / c.len() as f64,
            100.0 * (c.len() as f64 / a.len() as f64 - 1.0)
        );
        println!("   zero-delta voxels: {zero}/{tot} = {:.1}%", 100.0 * zero as f64 / tot as f64);
    }
}
