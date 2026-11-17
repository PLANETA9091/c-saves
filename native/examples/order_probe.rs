//! order_probe: how much does member ORDER inside the big slab streams
//! matter to the cold codecs?
//!
//! Research gate for a Hilbert-curve member ordering. FLVC (Eisenwave's
//! voxel-compression study) found space-filling-curve locality (Hilbert >
//! Morton > row-major) materially improves RLE/entropy stages; the 0.9.98
//! class partition proved LZMA2's literal contexts ARE order-sensitive
//! (two content families alternating = +31 KB model pollution). But blob-id
//! order already gives chunk-locality, and the 64 MiB-class windows +
//! dictionaries may bridge everything — this probe measures the actual
//! sensitivity on the REAL drained corpus.
//!
//! Method: build the production container (import -> dict -> pack-drain),
//! walk every per-class member stream (slab_members_by_class), then
//! re-compress the biggest class under alternative member orderings with
//! the SAME cold codecs the store uses (zstd-19; raw LZMA2 6|EXTREME with
//! the tuned lc=4/lp=0/pb=0 filter). Every permutation is content-neutral —
//! the member bytes are identical, only adjacency changes.
//!
//! Usage: cargo run --release --example order_probe -- <dim-dir> [out-dir]

use c_saves::store::{Cfg, Store};
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
    let Ok(data) = std::fs::read(path) else { return vec![] };
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
            3 => comp.clone(),
            _ => continue,
        };
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

/// splitmix64 shuffle (deterministic across runs/platforms)
fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut ord: Vec<usize> = (0..n).collect();
    let mut s = seed;
    for i in (1..n).rev() {
        let j = (splitmix(&mut s) % (i as u64 + 1)) as usize;
        ord.swap(i, j);
    }
    ord
}

fn interleaved(n: usize, stride: usize) -> Vec<usize> {
    // cyclic stride walk: member i of the "next chunk" lands where the
    // current chunk's member ends — approximates chunk-level locality
    // changes (x-strip -> z-strip ordering) without needing coordinates
    let mut ord = Vec::with_capacity(n);
    let cols = stride.max(1);
    let rows = n.div_ceil(cols);
    for r in 0..rows {
        for c in 0..cols {
            let idx = c * rows + r;
            if idx < n {
                ord.push(idx);
            }
        }
    }
    ord
}

fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
    // mirror the store's tuned cold config: preset 6|EXTREME, lc=4 lp=0 pb=0
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).expect("usage: order_probe <dim-dir> [out-dir]"));
    let out_root = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/order-probe"));
    let out = out_root.join("container");
    let _ = std::fs::remove_dir_all(&out_root);
    std::fs::create_dir_all(&out).unwrap();

    // ---- build the production container ----
    let mut region_chunks = Vec::new();
    let mut entity_chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        region_chunks.extend(load_mca(f));
    }
    let mut efiles: Vec<PathBuf> = std::fs::read_dir(dim.join("entities"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    efiles.sort();
    for f in &efiles {
        entity_chunks.extend(load_mca(f));
    }
    println!(
        "corpus: {} region chunks, {} entity chunks",
        region_chunks.len(),
        entity_chunks.len()
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
        rs.write_chunk_stripped(key(*cx, *cz), p, true).unwrap();
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
    drop(rs);
    drop(es);

    // ---- price the biggest class under alternative orderings ----
    let (big_cls, big_members) = by_class
        .iter()
        .max_by_key(|(_, v)| v.iter().map(|b| b.len()).sum::<usize>())
        .map(|(c, v)| (c.clone(), v.clone()))
        .unwrap();
    let n = big_members.len();
    let raw: usize = big_members.iter().map(|b| b.len()).sum();
    println!(
        "\n-- ordering sensitivity: class {big_cls}, {n} members, {raw} B raw --"
    );

    let mut orders: Vec<(String, Vec<usize>)> = vec![
        ("id".into(), (0..n).collect()),
        ("rev".into(), (0..n).rev().collect()),
        ("shuf1".into(), shuffled(n, 0xDEADBEEF)),
        ("shuf2".into(), shuffled(n, 0x12345678)),
        ("ilv16".into(), interleaved(n, 16)),
        ("ilv64".into(), interleaved(n, 64)),
        ("ilv256".into(), interleaved(n, 256)),
    ];
    println!(
        "{:>8} {:>12} {:>12} {:>8} {:>8}",
        "order", "z19 B", "lzma2 B", "z19/x", "lzma2/x"
    );
    let mut results: Vec<(String, usize, usize)> = Vec::new();
    for (name, ord) in orders.drain(..) {
        let stream: Vec<u8> = ord
            .iter()
            .flat_map(|&i| big_members[i].iter().copied())
            .collect();
        let z = zstd::bulk::compress(&stream, 19).unwrap();
        let x = lzma2_raw(&stream).unwrap();
        println!(
            "{:>8} {:>12} {:>12} {:>8.3} {:>8.3}",
            name,
            z.len(),
            x.len(),
            raw as f64 / z.len() as f64,
            raw as f64 / x.len() as f64
        );
        results.push((name, z.len(), x.len()));
    }
    // verdict: the CURRENT stored order (blob-id) is the bar any new
    // ordering must beat; report the best-alt gain and the worst loss
    let id_x = results.iter().find(|r| r.0 == "id").map(|r| r.2).unwrap();
    let best_alt_x = results
        .iter()
        .filter(|r| r.0 != "id")
        .map(|r| r.2)
        .min()
        .unwrap_or(id_x);
    let worst_x = results.iter().map(|r| r.2).max().unwrap_or(id_x);
    println!(
        "ORDER-VERDICT id_is_best={} best_alt_vs_id={:+.2}% worst_vs_id={:+.2}% (band {:.2}%)",
        best_alt_x >= id_x,
        100.0 * (best_alt_x as f64 - id_x as f64) / id_x as f64,
        100.0 * (worst_x as f64 - id_x as f64) / id_x as f64,
        100.0 * (worst_x - best_alt_x) as f64 / id_x as f64
    );
    let _ = std::fs::remove_dir_all(&out_root);
}
