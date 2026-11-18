//! sim_probe: content-SIMILARITY member ordering for the class-partitioned
//! slab streams. The committed order_probe (6a463e7) closed the Hilbert /
//! generic-permutation direction (id/rev/shuf/interleaved — id was best),
//! but it never tested orders derived from member CONTENT similarity: the
//! 0.9.101 worklog session measured -2.75% on the JJ glob stream with a
//! lex-by-histogram walk and was lost to a sandbox recycle before push.
//!
//! This probe rebuilds the production container from a dim dir, walks the
//! per-class member streams, and re-prices the biggest classes under
//! content-derived orderings:
//!   id        — the stored (blob-id / chunk-locality) order, the bar
//!   lex       — plain lexicographic member-body sort (palette bytes lead
//!               every canonical member, so same-palette members cluster)
//!   histlex32 — 32-bin byte-histogram signature, lex tie-break
//!   nn32      — greedy nearest-neighbor chain over 32-bin L1 signatures
//!   nn256     — greedy nearest-neighbor chain over 256-bin L1 signatures
//! Every permutation is content-neutral: member bytes are identical, only
//! adjacency changes. Pricing = the store's tuned cold codec (LZMA2 6|e
//! lc=4/lp=0/pb=0) + zstd-19 as the secondary meter.
//!
//! Determinism: all orders are pure functions of member bytes (lex sort /
//! histogram sort / NN chain with (dist, DefaultHasher) tie-break —
//! DefaultHasher::new() is fixed-key in std).
//!
//! Usage: sim_probe <dim-dir> [--import]

use c_saves::store::{Cfg, Store};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else {
        return vec![];
    };
    if data.len() < 8192 {
        return vec![];
    }
    let mut out = Vec::new();
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([
            data[i * 4],
            data[i * 4 + 1],
            data[i * 4 + 2],
            data[i * 4 + 3],
        ]);
        if loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        if off + 5 > data.len() {
            continue;
        }
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        if len < 2 || off + 4 + len > data.len() {
            continue;
        }
        let comp_type = data[off + 4];
        let comp = data[off + 5..off + 4 + len].to_vec();
        let payload = match comp_type {
            1 => {
                let mut v = Vec::new();
                if flate2::read::GzDecoder::new(&comp[..]).read_to_end(&mut v).is_err() {
                    continue;
                }
                v
            }
            2 => {
                let mut v = Vec::new();
                if flate2::read::ZlibDecoder::new(&comp[..]).read_to_end(&mut v).is_err() {
                    continue;
                }
                v
            }
            _ => continue,
        };
        out.push((rx * 32 + (i % 32) as i32, rz * 32 + (i / 32) as i32, payload));
    }
    out
}

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

fn body_hash(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// byte histogram, folded to `bins` bins (bin = byte >> log2(256/bins))
fn signature(b: &[u8], bins: usize) -> Vec<u32> {
    let shift = 8 - bins.trailing_zeros().max(1).min(8) as u32;
    let mut s = vec![0u32; bins];
    for &x in b {
        s[(x >> shift) as usize % bins] += 1;
    }
    s
}

fn l1(a: &[u32], b: &[u32]) -> u64 {
    a.iter().zip(b).map(|(x, y)| (i64::from(*x) - i64::from(*y)).unsigned_abs()).sum()
}

/// plain lexicographic member-body sort
fn order_lex(members: &[Vec<u8>]) -> Vec<usize> {
    let mut ord: Vec<usize> = (0..members.len()).collect();
    ord.sort_by(|&a, &b| members[a].cmp(&members[b]));
    ord
}

/// histogram-signature lex sort (deterministic, O(n log n) — the shippable shape)
fn order_histlex(members: &[Vec<u8>], bins: usize) -> Vec<usize> {
    let sigs: Vec<Vec<u32>> = members.iter().map(|m| signature(m, bins)).collect();
    let mut ord: Vec<usize> = (0..members.len()).collect();
    ord.sort_by(|&a, &b| sigs[a].cmp(&sigs[b]).then_with(|| members[a].cmp(&members[b])));
    ord
}

/// greedy NN chain, O(n^2) — the ceiling probe. Deterministic tie-break:
/// (distance, content hash, lex).
fn order_nn(members: &[Vec<u8>], bins: usize) -> Vec<usize> {
    let n = members.len();
    let sigs: Vec<Vec<u32>> = members.iter().map(|m| signature(m, bins)).collect();
    let hashes: Vec<u64> = members.iter().map(|m| body_hash(m)).collect();
    let mut used = vec![false; n];
    let mut chain = Vec::with_capacity(n);
    // start: lexicographically smallest body
    let mut cur = (0..n).min_by(|&a, &b| members[a].cmp(&members[b])).unwrap();
    used[cur] = true;
    chain.push(cur);
    for _ in 1..n {
        let mut best: Option<(u64, u64, u64)> = None;
        for (i, u) in used.iter().enumerate() {
            if *u {
                continue;
            }
            let d = l1(&sigs[cur], &sigs[i]);
            let k = (d, hashes[i], i as u64);
            if best.map(|b| k < b).unwrap_or(true) {
                best = Some(k);
            }
        }
        let (_, _, nxt_u) = best.unwrap();
        let nxt = nxt_u as usize;
        used[nxt] = true;
        chain.push(nxt);
        cur = nxt;
    }
    chain
}

/// NN within lex-prefix buckets — the shippable approximation of the NN
/// ceiling: bucket members by their first `pb` body bytes (palette+form
/// prefix), greedy-NN chain INSIDE each bucket over `bins`-bin signatures,
/// buckets concatenated in lex-prefix order. Cost O(n log n + sum k_i^2);
/// palettes repeat across strata members, so buckets stay moderate.
fn order_lexnn(members: &[Vec<u8>], pb: usize, bins: usize) -> Vec<usize> {
    let n = members.len();
    let mut ord: Vec<usize> = (0..n).collect();
    ord.sort_by(|&a, &b| members[a].cmp(&members[b]));
    // walk the lex-sorted list, cut buckets at prefix boundaries
    let mut out = Vec::with_capacity(n);
    let mut start = 0usize;
    while start < n {
        let key = {
            let b = &members[ord[start]];
            let e = pb.min(b.len());
            b[..e].to_vec()
        };
        let mut end = start + 1;
        while end < n {
            let b = &members[ord[end]];
            let eb = pb.min(b.len());
            if eb != key.len() || b[..eb] != key[..] {
                break;
            }
            end += 1;
        }
        if end - start <= 3 {
            out.extend_from_slice(&ord[start..end]);
        } else {
            // NN inside the bucket: indices into `members`, chained, then
            // appended in chain order
            let idx: Vec<usize> = ord[start..end].to_vec();
            out.extend(order_nn_idx(members, &idx, bins));
        }
        start = end;
    }
    out
}

fn order_nn_idx(members: &[Vec<u8>], idx: &[usize], bins: usize) -> Vec<usize> {
    let sigs: Vec<Vec<u32>> = idx.iter().map(|&i| signature(&members[i], bins)).collect();
    let hashes: Vec<u64> = idx.iter().map(|&i| body_hash(&members[i])).collect();
    let k = idx.len();
    let mut used = vec![false; k];
    let mut chain = Vec::with_capacity(k);
    let mut cur = (0..k).min_by(|&a, &b| members[idx[a]].cmp(&members[idx[b]])).unwrap();
    used[cur] = true;
    chain.push(idx[cur]);
    for _ in 1..k {
        let mut best: Option<(u64, u64, u64)> = None;
        for (j, u) in used.iter().enumerate() {
            if *u {
                continue;
            }
            let d = l1(&sigs[cur], &sigs[j]);
            let key = (d, hashes[j], j as u64);
            if best.map(|b| key < b).unwrap_or(true) {
                best = Some(key);
            }
        }
        let (_, _, nxt_u) = best.unwrap();
        let nxt = nxt_u as usize;
        used[nxt] = true;
        chain.push(idx[nxt]);
        cur = nxt;
    }
    chain
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).expect("usage: sim_probe <dim-dir> [--import]"));
    let import_mode = args.iter().any(|a| a == "--import");
    let out_root = PathBuf::from("/tmp/sim-probe");
    let out = out_root.join("container");
    let _ = std::fs::remove_dir_all(&out_root);
    std::fs::create_dir_all(&out).unwrap();

    // ---- build the production container ----
    let mut region_chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        region_chunks.extend(load_mca(f));
    }
    let mut entity_chunks = Vec::new();
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(dim.join("entities"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        entity_chunks.extend(load_mca(f));
    }
    println!(
        "corpus: {} region chunks, {} entity chunks (import_mode={})",
        region_chunks.len(),
        entity_chunks.len(),
        import_mode
    );
    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        ..Cfg::from_env()
    };
    let rs = Store::open_cfg(&out.join("region"), true, cfg.clone()).unwrap();
    let es = Store::open_cfg(&out.join("entities"), true, cfg).unwrap();
    es.set_whole_mode();
    for (cx, cz, p) in &region_chunks {
        let r = if import_mode {
            rs.write_chunk_import(key(*cx, *cz), p)
        } else {
            rs.write_chunk_stripped(key(*cx, *cz), p, true)
        };
        r.unwrap();
    }
    for (cx, cz, p) in &entity_chunks {
        es.write_chunk(key(*cx, *cz), p, true).unwrap();
    }
    rs.sync();
    es.sync();
    rs.train_dict_if_needed();
    es.train_dict_if_needed();
    rs.seal();
    es.seal();
    rs.request_pack();
    es.request_pack();
    while rs.compact_step() || es.compact_step() {}
    rs.sync();
    es.sync();
    println!(
        "drained: {:.2} MiB",
        (rs.disk_bytes() + es.disk_bytes()) as f64 / 1048576.0
    );

    // ---- per-class member streams ----
    let members = rs.slab_members_by_class();
    let mut by_class: std::collections::BTreeMap<String, Vec<Vec<u8>>> = Default::default();
    for (cls, body) in members {
        by_class.entry(cls).or_default().push(body);
    }
    let mut ranked: Vec<(String, usize, usize)> = by_class
        .iter()
        .map(|(c, v)| (c.clone(), v.len(), v.iter().map(|b| b.len()).sum()))
        .collect();
    ranked.sort_by_key(|x| std::cmp::Reverse(x.2));
    for (cls, n, bytes) in &ranked {
        println!("class {cls}: {n} members, {bytes} B raw");
    }

    // ---- price every class >= 64 members under similarity orders ----
    for (cls, mems) in by_class.iter() {
        let n = mems.len();
        if n < 64 {
            continue;
        }
        let raw: usize = mems.iter().map(|b| b.len()).sum();
        println!("\n-- class {cls}: {n} members, {raw} B raw --");
        let mut orders: Vec<(String, Vec<usize>)> = vec![
            ("id".into(), (0..n).collect()),
            ("lex".into(), order_lex(mems)),
            ("histlex32".into(), order_histlex(mems, 32)),
            ("lexnn16_32".into(), order_lexnn(mems, 16, 32)),
            ("nn32".into(), order_nn(mems, 32)),
        ];
        if n <= 4000 {
            orders.push(("nn256".into(), order_nn(mems, 256)));
        }
        println!(
            "{:>10} {:>12} {:>10} {:>12} {:>10}",
            "order", "lzma2 B", "vs id", "z19 B", "vs id"
        );
        let (id_x, id_z) = {
            let s: Vec<u8> = (0..n).flat_map(|i| mems[i].iter().copied()).collect();
            (lzma2_raw(&s).unwrap().len(), zstd::bulk::compress(&s, 19).unwrap().len())
        };
        for (name, ord) in &orders {
            let s: Vec<u8> = ord.iter().flat_map(|&i| mems[i].iter().copied()).collect();
            let x = lzma2_raw(&s).unwrap().len();
            let z = zstd::bulk::compress(&s, 19).unwrap().len();
            println!(
                "{:>10} {:>12} {:>+9.2}% {:>12} {:>+9.2}%",
                name,
                x,
                (x as f64 / id_x as f64 - 1.0) * 100.0,
                z,
                (z as f64 / id_z as f64 - 1.0) * 100.0
            );
        }
    }
    println!("\nSIM-PROBE done");
}
