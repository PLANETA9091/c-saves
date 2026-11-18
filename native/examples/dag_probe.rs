//! dag_probe: price the sub-section / whole-member DELTA representation
//! (DAG-lite) on top of the SHIPPED lex walk, WITHOUT decoding members.
//!
//! Model: for each member (lex walk, the shipped adjacency), find the best
//! reference among the previous members of the same class stream; encode a
//! pseudo-member = varint(ref_idx) + varint(common_prefix) +
//! varint(common_suffix) + XOR(mid-region). Splice pseudo-members into the
//! stream (per-member gate: only where the pseudo-member is SMALLER than
//! the raw body), LZMA2 the whole stream with the tuned cold config, and
//! compare against the shipped baseline (all-raw, lex walk).
//!
//! This prices the BYTE-level delta win — the floor of what a real
//! gid-level DAG implementation could deliver (gid deltas price <= byte
//! deltas usually). If the byte-level model is flat, the direction is dead
//! on this corpus shape — same lesson as the columnar tier (0.9.105).
//!
//! Usage: dag_probe <store-dir> [class-prefix]

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

/// byte-level delta against ref: [cprefix, csuffix] shared, XOR mid
fn delta_body(member: &[u8], ref_: &[u8]) -> Option<Vec<u8>> {
    // longest common prefix
    let n = member.len().min(ref_.len());
    let mut cp = 0usize;
    while cp < n && member[cp] == ref_[cp] {
        cp += 1;
    }
    // longest common suffix (not overlapping the prefix)
    let mut cs = 0usize;
    while cs < n - cp && member[member.len() - 1 - cs] == ref_[ref_.len() - 1 - cs] {
        cs += 1;
    }
    let mid_m = &member[cp..member.len() - cs];
    let mid_r = &ref_[cp..ref_.len() - cs];
    if mid_m.len() != mid_r.len() {
        // different lengths: delta needs a length field + copy semantics —
        // price the CONSERVATIVE case: no delta at all
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
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).expect("usage: dag_probe <store-dir> [class-prefix]"));
    let cls_prefix = args.get(2).cloned().unwrap_or_else(|| "csec3".into());
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    // per-class streams in WALK order; ship shape = lex inside the slab;
    // the Tectonic csec3 class is one slab, so the walk IS the stream
    let mut streams: std::collections::BTreeMap<String, Vec<Vec<u8>>> = Default::default();
    for (cls, body) in members {
        if cls.starts_with(&cls_prefix) {
            streams.entry(cls).or_default().push(body);
        }
    }
    for (cls, mut mems) in streams {
        let n = mems.len();
        if n < 64 {
            continue;
        }
        let raw: usize = mems.iter().map(|b| b.len()).sum();
        // shipped baseline: lex walk, all raw
        mems.sort();
        let base_stream: Vec<u8> = mems.concat();
        let base_x = lzma2_raw(&base_stream).unwrap().len();
        // delta pass: greedy best ref among previous members (the lex
        // window), per-member gate, delta body replaces the raw body
        let mut stream: Vec<u8> = Vec::with_capacity(base_stream.len());
        let mut deltas = 0usize;
        let mut delta_bytes = 0usize;
        let mut saved_raw = 0usize;
        let window = 64usize; // refs searched back (bounded pack cost)
        for (i, m) in mems.iter().enumerate() {
            let mut best: Option<Vec<u8>> = None;
            let lo = i.saturating_sub(window);
            for r in lo..i {
                if let Some(d) = delta_body(m, &mems[r]) {
                    let cost = d.len() + (i - r).min(3); // ~varint ref idx
                    if cost < m.len() && best.as_ref().map(|b| cost < b.len()).unwrap_or(true) {
                        best = Some(d);
                    }
                }
            }
            if let Some(d) = best {
                // 1-byte ref-idx placeholder (the real ref rides the entry
                // table as a varint — 1-3 B; model stays a floor estimate)
                stream.push(0xFF);
                stream.extend_from_slice(&d);
                deltas += 1;
                delta_bytes += d.len();
                saved_raw += m.len() - d.len();
            } else {
                stream.extend_from_slice(m);
            }
        }
        let x = lzma2_raw(&stream).unwrap().len();
        println!(
            "class {cls}: n={n} raw={raw} | baseline_lex_lz2={base_x} | delta_stream_lz2={x} ({:+.2}%) | deltas={deltas} raw_saved={saved_raw}",
            (x as f64 / base_x as f64 - 1.0) * 100.0
        );
    }
    println!("DAG-PROBE done");
}
