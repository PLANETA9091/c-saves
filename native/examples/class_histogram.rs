//! class_histogram: post-container member-class pricing for a STORE DIR.
//! Opens an existing drained container, walks every slab member with the
//! content-derived class, and prices each class stream with the store's
//! cold codecs (raw LZMA2 tuned) — the honest per-pole byte attribution
//! the raw-share profile can't give (LZMA2 compresses classes very
//! differently).
//!
//! Usage: cargo run --release --example class_histogram -- <store-dir>

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

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: class_histogram <store-dir>"));
    let st = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    let members = st.slab_members_by_class();
    let mut by_class: BTreeMap<String, Vec<Vec<u8>>> = Default::default();
    for (cls, body) in members {
        by_class.entry(cls).or_default().push(body);
    }
    let mut rows: Vec<(String, usize, usize, usize)> = Vec::new();
    for (cls, v) in &by_class {
        let raw: usize = v.iter().map(|b| b.len()).sum();
        let stream: Vec<u8> = v.iter().flat_map(|b| b.iter().copied()).collect();
        let priced = lzma2_raw(&stream).map(|b| b.len()).unwrap_or(usize::MAX);
        rows.push((cls.clone(), v.len(), raw, priced));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.3));
    let total_raw: usize = rows.iter().map(|r| r.2).sum();
    let total_lz: usize = rows.iter().map(|r| r.3).sum::<usize>().min(total_raw);
    println!(
        "{cls:>12} {n:>6} {raw:>10} {lz:>10} {share:>6.1}%  {ratio:>6.2}x",
        cls = "CLASS",
        n = "members",
        raw = "raw B",
        lz = "lzma2 B",
        share = "lz-share",
        ratio = "ratio"
    );
    for (cls, n, raw, lz) in &rows {
        println!(
            "{cls:>12} {n:>6} {raw:>10} {lz:>10} {:>5.1}%  {:>6.2}x",
            100.0 * *lz as f64 / total_lz.max(1) as f64,
            *raw as f64 / (*lz).max(1) as f64
        );
    }
    println!(
        "TOTAL members-raw {total_raw} B -> lzma2 {total_lz} B ({:.2}x)",
        total_raw as f64 / total_lz.max(1) as f64
    );
}
