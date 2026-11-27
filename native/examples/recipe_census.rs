//! recipe_census (0.9.130): the recipe/batch record class — the one axis the
//! 0.9.129 byte-wall census never priced (flagged in the 0.9.128 NEXT list:
//! "pointer-dense, may compress harder columnar").
//!
//! The shipped batch record (kind 0x0C/0x0D) stores, per 192 KiB group:
//!   [varint key0][varint key-delta]x n ++ [varint body_idx]x n
//!   ++ [u16 len]x m ++ concat(unique bodies).
//! The BODY zone is row-per-chunk: [0xFF][mode][flags?][n][zigzag id-delta
//! varints x n | (delta,run) pairs][tail kind + payload]. Columnar pricing =
//! split the body zone into per-field columns (hdr / id-delta / run /
//! tail / other) over the unique bodies of the whole dimension and compress
//! the SAME zones with the SAME tuned LZMA2 the 0x0D record ships (preset
//! 6|EXTREME, lc=4 lp=0 pb=0). A win here is a new batch kind; a flat result
//! closes the last unpriced axis with a printed bound.
//!
//! The bodies come from the CSAVES_RECIPE_DUMP hook on the real import path
//! (the exact bytes the flush commits), so the census measures shipped
//! content, not a model.
//!
//! Usage: cargo run --release --example recipe_census [footprint]

use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;
use std::path::PathBuf;

#[path = "inc/tectonic_corpus.rs"]
mod tc;

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

// -- the shipped tuned LZMA2 (store::lzma2_compress_as, 0xFC config) -------
fn lz2(data: &[u8]) -> Option<Vec<u8>> {
    let mut opts =
        liblzma::stream::LzmaOptions::new_preset(6 | liblzma::stream::PRESET_EXTREME).ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut s = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let mut consumed = 0usize;
    while consumed < data.len() {
        if out.len() == out.capacity() {
            out.reserve(64 * 1024);
        }
        let before = s.total_in() as usize;
        s.process_vec(&data[consumed..], &mut out, liblzma::stream::Action::Run)
            .ok()?;
        let now = s.total_in() as usize;
        if now == before {
            return None;
        }
        consumed = now;
    }
    loop {
        if out.len() == out.capacity() {
            out.reserve(64 * 1024);
        }
        let st = s
            .process_vec(&[], &mut out, liblzma::stream::Action::Finish)
            .ok()?;
        if matches!(st, liblzma::stream::Status::StreamEnd) {
            break;
        }
    }
    Some(out)
}

fn get_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut x = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        x |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(x);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// A parsed recipe wire (the [0xFF] prefix stripped by the caller).
struct Rec {
    mode: u8,
    flags: u8,
    n: u8,
    /// raw varint bytes of the id-delta column (mode 2/8) or the delta half
    /// of the (delta,run) pairs (mode 6/10)
    deltas: Vec<u8>,
    /// raw varint bytes of the run half (mode 6/10 only)
    runs: Vec<u8>,
    /// tail bytes verbatim (kind byte + payload)
    tail: Vec<u8>,
}

fn parse_rec(rec: &[u8]) -> Option<Rec> {
    let mode = *rec.first()?;
    let mut p = 1usize;
    let mut flags = 0u8;
    if matches!(mode, 8 | 10) {
        flags = *rec.get(p)?;
        p += 1;
    }
    if !matches!(mode, 2 | 6 | 8 | 10) {
        return None; // verbatim class (0/1/3/4/5/7)
    }
    let n = *rec.get(p)?;
    p += 1;
    let mut deltas = Vec::new();
    let mut runs = Vec::new();
    let run_len = matches!(mode, 6 | 10);
    for _ in 0..n {
        let s = p;
        get_u64(rec, &mut p)?;
        deltas.extend_from_slice(&rec[s..p]);
        if run_len {
            let s2 = p;
            get_u64(rec, &mut p)?;
            runs.extend_from_slice(&rec[s2..p]);
        }
    }
    // tail: kind 0 = none, 1 = [dict][cl u32][raw u32][comp], 2 = [id varint],
    // 3 = [16B hash] — captured verbatim from the kind byte on.
    let tkind = *rec.get(p)?;
    let tail_start = p;
    p += 1;
    match tkind {
        0 => {}
        1 => {
            p += 1; // dict
            if p + 8 > rec.len() {
                return None;
            }
            let cl = u32::from_le_bytes(rec[p..p + 4].try_into().ok()?) as usize;
            p += 8 + cl;
        }
        2 => {
            get_u64(rec, &mut p)?;
        }
        3 => {
            p += 16;
        }
        _ => return None,
    }
    if p > rec.len() {
        return None;
    }
    Some(Rec {
        mode,
        flags,
        n,
        deltas,
        runs,
        tail: rec[tail_start..].to_vec(),
    })
}

struct DimCensus {
    name: &'static str,
    entries: usize,
    uniq: usize,
    raw_bodies: usize,
    ship_raw: usize,
    ship_lz: usize,
    col_raw: usize,
    col_lz: usize,
    mode_line: String,
}

fn main() {
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(192);
    let out = PathBuf::from(std::env::var("CSAVES_RC_OUT").unwrap_or_else(|_| "/tmp/rc".into()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let mut results: Vec<DimCensus> = Vec::new();
    for name in ["overworld", "nether", "end"] {
        // 1) generate + import through the production path with the dump on
        let dump = out.join(format!("{}.dump", name));
        let _ = std::fs::remove_file(&dump);
        std::env::set_var("CSAVES_RECIPE_DUMP", &dump);
        let mut payloads: Vec<(u64, Vec<u8>)> = Vec::new();
        match name {
            "overworld" => {
                let mut b = tc::Builder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, b.payload(cx, cz).0));
                    }
                }
            }
            "nether" => {
                let mut b = tc::NetherBuilder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, b.payload(cx, cz).0));
                    }
                }
            }
            _ => {
                let mut b = tc::EndBuilder::new(f);
                for cz in 0..f {
                    for cx in 0..f {
                        payloads.push((((cx as u64) << 32) | (cz as u32) as u64, b.payload(cx, cz).0));
                    }
                }
            }
        }
        let store_dir = out.join(format!("store-{}", name));
        let st = Store::open_cfg(&store_dir, true, Cfg::from_env()).unwrap();
        for (k, p) in &payloads {
            st.write_chunk_import(*k, p).unwrap();
        }
        drain(&st);
        std::env::remove_var("CSAVES_RECIPE_DUMP");
        drop(st);

        // 2) read the dump: [u64 key][u32 len][body..]
        let raw = std::fs::read(&dump).unwrap();
        let mut entries: Vec<(u64, &[u8])> = Vec::new();
        {
            let mut p = 0usize;
            while p + 12 <= raw.len() {
                let k = u64::from_le_bytes(raw[p..p + 8].try_into().unwrap());
                let l = u32::from_le_bytes(raw[p + 8..p + 12].try_into().unwrap()) as usize;
                entries.push((k, &raw[p + 12..p + 12 + l]));
                p += 12 + l;
            }
        }
        entries.sort_by_key(|(k, _)| *k);
        let n_entries = entries.len();

        // 3) the grouping path's hash dedup (chunknbt::hash_of = the SAME
        // u128 the flush grouping uses), first occurrence in key order
        let mut seen: Map<u128, u32> = Map::new();
        let mut uniq_recs: Vec<Rec> = Vec::new();
        let mut uniq_other: Vec<&[u8]> = Vec::new();
        let mut uniq_ix_of: Vec<u32> = Vec::with_capacity(n_entries);
        let mut mode_hist: Map<u8, usize> = Map::new();
        let mut raw_bodies = 0usize;
        for (_, b) in &entries {
            raw_bodies += b.len();
            let h = c_saves::chunknbt::hash_of(b);
            let ix = match seen.get(&h) {
                Some(&ix) => ix,
                None => {
                    let ix = (uniq_recs.len() + uniq_other.len()) as u32;
                    seen.insert(h, ix);
                    let rec = b.get(1..).unwrap_or(b); // strip [0xFF]
                    match parse_rec(rec) {
                        Some(r) => {
                            *mode_hist.entry(r.mode).or_default() += 1;
                            uniq_recs.push(r);
                        }
                        None => {
                            let mo = rec.first().copied().unwrap_or(0xFF);
                            *mode_hist.entry(mo).or_default() += 1;
                            uniq_other.push(rec);
                        }
                    }
                    ix
                }
            };
            uniq_ix_of.push(ix);
        }
        let m = uniq_recs.len() + uniq_other.len();

        // 4) variant A — the shipped 0x0C stream (keys + idx + lens + bodies)
        let mut ship = Vec::with_capacity(raw_bodies + n_entries * 4);
        {
            let mut prev = 0u64;
            for (i, (k, _)) in entries.iter().enumerate() {
                let v = if i == 0 { *k } else { k.wrapping_sub(prev) };
                let mut x = v;
                while x >= 0x80 {
                    ship.push((x as u8) | 0x80);
                    x >>= 7;
                }
                ship.push(x as u8);
                prev = *k;
            }
            for ix in &uniq_ix_of {
                let mut x = *ix as u64;
                while x >= 0x80 {
                    ship.push((x as u8) | 0x80);
                    x >>= 7;
                }
                ship.push(x as u8);
            }
            let mut s2: Map<u128, ()> = Map::new();
            let mut lens: Vec<u16> = Vec::new();
            for (_, b) in &entries {
                let h = c_saves::chunknbt::hash_of(b);
                if s2.insert(h, ()).is_none() {
                    lens.push(b.len() as u16);
                }
            }
            for l in &lens {
                ship.extend_from_slice(&l.to_le_bytes());
            }
            for (_, b) in &entries {
                let h = c_saves::chunknbt::hash_of(b);
                if s2.contains_key(&h) {
                    ship.extend_from_slice(b);
                    s2.remove(&h); // emit each unique body once, first occurrence
                }
            }
        }
        let ship_lz = lz2(&ship).unwrap();

        // 5) variant B — the columnar body zone: same key + idx zones, the
        // len zone folded into the hdr column, bodies split per field
        let mut col = Vec::with_capacity(raw_bodies + n_entries * 4);
        {
            let mut prev = 0u64;
            for (i, (k, _)) in entries.iter().enumerate() {
                let v = if i == 0 { *k } else { k.wrapping_sub(prev) };
                let mut x = v;
                while x >= 0x80 {
                    col.push((x as u8) | 0x80);
                    x >>= 7;
                }
                col.push(x as u8);
                prev = *k;
            }
            for ix in &uniq_ix_of {
                let mut x = *ix as u64;
                while x >= 0x80 {
                    col.push((x as u8) | 0x80);
                    x >>= 7;
                }
                col.push(x as u8);
            }
            // [hdr][deltas][runs][tails][others] — hdr carries mode+flags+n
            // (the walk keys), deltas/runs are self-terminating varint runs
            // gated by n, tails are kind-self-describing
            for r in &uniq_recs {
                col.push(r.mode);
                if matches!(r.mode, 8 | 10) {
                    col.push(r.flags);
                }
                col.push(r.n);
            }
            for _ in &uniq_other {
                col.push(0xFF);
            }
            for r in &uniq_recs {
                col.extend_from_slice(&r.deltas);
            }
            for r in &uniq_recs {
                col.extend_from_slice(&r.runs);
            }
            for r in &uniq_recs {
                col.extend_from_slice(&r.tail);
            }
            for o in &uniq_other {
                col.extend_from_slice(o);
            }
        }
        let col_lz = lz2(&col).unwrap();

        let mut modes: Vec<(u8, usize)> = mode_hist.into_iter().collect();
        modes.sort_by_key(|x| std::cmp::Reverse(x.1));
        let mode_line = modes
            .iter()
            .map(|(k, v)| format!("m{}:{}", k, v))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "== {} == entries {} uniq {} raw bodies {} B ({} B/body) | modes [{}]",
            name,
            n_entries,
            m,
            raw_bodies,
            raw_bodies / m.max(1),
            mode_line
        );
        println!(
            "   shipped-zone rebuild: raw {} -> lz {} B | columnar: raw {} -> lz {} B | col vs ship {:+.2}%",
            ship.len(),
            ship_lz.len(),
            col.len(),
            col_lz.len(),
            100.0 * (col_lz.len() as f64 - ship_lz.len() as f64) / ship_lz.len() as f64
        );
        results.push(DimCensus {
            name,
            entries: n_entries,
            uniq: m,
            raw_bodies,
            ship_raw: ship.len(),
            ship_lz: ship_lz.len(),
            col_raw: col.len(),
            col_lz: col_lz.len(),
            mode_line,
        });
    }

    println!("== recipe columnar census verdict ==");
    for r in &results {
        println!(
            "{:<10} ship {:>8} -> {:>7} B | col {:>8} -> {:>7} B | col vs ship {:+.2}%   [{}]",
            r.name,
            r.ship_raw,
            r.ship_lz,
            r.col_raw,
            r.col_lz,
            100.0 * (r.col_lz as f64 - r.ship_lz as f64) / r.ship_lz as f64,
            r.mode_line
        );
    }
}
