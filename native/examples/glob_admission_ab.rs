//! glob_admission_ab: the DENSE-gid class A/B — do the g0/g6 members
//! (raw u8 gid streams, 4096 B each) want the gid-RLE admission on the
//! Tectonic overworld corpus?
//!
//! The v0.9.115 route opened gid-RLE (div=4) only for members whose CSEC
//! source stream was run-length coded (from_rle). The 0.9.116 full-mix
//! attribution shows the overworld slab mass now lives in glob_g6 (n=1270,
//! raw 5.2 MB = 4,100 B/member — dense u8 gid streams): sections whose
//! CSEC source arrived DENSE (noisy ridge boundaries — the canonical RLE
//! admission lost there) and therefore never opened the route. If those
//! streams' RLE wire is small, the per-member admission would have taken
//! it — but per-member mixing was measured a JJ regression (x2.99 vs
//! x3.08), because run boundaries break cross-section alignment.
//!
//! The class-level question (the pack could carry the WHOLE class in its
//! RLE variant and pick the smaller compressed stream, exactly like the
//! canon-vs-glob and g6-twin window gates): price BOTH whole-class streams
//! with the EXACT cold-tier config. The class split it prints also shows
//! whether a coarse content predicate (e.g. "RLE wire <= 1024 B") is
//! uniform across the class (per-class gate safe) or mixed.
//!
//! Usage: cargo run --release --example glob_admission_ab [footprint]
use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;
use std::path::PathBuf;

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

/// the store's cold-tier LZMA2 config verbatim (preset 6|EXTREME, lc=4 lp=0 pb=0)
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

fn runs(v: &[u8]) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < v.len() {
        let g = v[i];
        let mut n = 1u64;
        while i + (n as usize) < v.len() && v[i + (n as usize)] == g {
            n += 1;
        }
        out.push((g, n));
        i += n as usize;
    }
    out
}

fn rle_wire(rs: &[(u8, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (g, n) in rs {
        out.push(*g);
        put_uvarint(&mut out, *n);
    }
    out
}

/// the y-fast transpose (involution), side 16, on 4096 bytes
fn transpose(v: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; v.len()];
    let side = 16;
    let area = side * side;
    // chunknbt::transpose_fast_y semantics: x/z-fast to y-fast reorder.
    // The exact permutation does not matter for PRICING as long as encode
    // and decode agree; use the vanilla <-> y-major block reorder the
    // engine uses (i = ((x*16 + z)*16 + y) -> ((y*16 + x)*16 + z)).
    for x in 0..side {
        for z in 0..side {
            for y in 0..side {
                let from = ((x * side) + z) * side + y;
                let to = ((y * side) + x) * side + z;
                out[to] = v[from];
            }
        }
    }
    let _ = area;
    out
}

fn main() {
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(96);
    // 1) generate the overworld corpus (the Tectonic mix)
    let mut corpus: Vec<(i32, i32, Vec<u8>, &'static str)> = Vec::with_capacity((f * f) as usize);
    let mut b = tc::Builder::new(f);
    for cz in 0..f {
        for cx in 0..f {
            let (p, c) = b.payload(cx, cz);
            corpus.push((cx, cz, p, c.name()));
        }
    }
    // 2) import + drain through the production path
    let dir = std::env::temp_dir().join(format!(
        "csaves-globab-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p, _c) in &corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    drain(&st);
    let disk = st.disk_bytes();
    let members = st.slab_members_by_class();
    drop(st);
    println!("drained disk={disk} B, members={}", members.len());

    // 3) per-class census + the dense-u8 A/B
    let mut by_class: Map<String, Vec<Vec<u8>>> = Map::new();
    for (cls, body) in &members {
        by_class.entry(cls.clone()).or_default().push(body.clone());
    }
    let mut cls_sorted: Vec<_> = by_class.iter().collect();
    cls_sorted.sort_by_key(|(c, v)| (std::cmp::Reverse(v.len() * 1), c.clone()));
    println!("-- class census --");
    for (c, v) in &cls_sorted {
        let raw: u64 = v.iter().map(|x| x.len() as u64).sum();
        println!("{:<14} n={:<6} raw={raw}", c, v.len());
    }

    // dense-u8 payloads: g0 (plain) and g6 (transposed twin)
    let mut dense: Vec<(bool, Vec<u8>)> = Vec::new(); // (is_transposed, payload)
    for c in ["glob_g0", "glob_g6"] {
        if let Some(v) = by_class.get(c) {
            for body in v {
                // [0xCC][f1][f2c][f3][y][gform][payload 4096]
                if body.len() >= 6 + 4096 {
                    dense.push((c == "glob_g6", body[6..6 + 4096].to_vec()));
                }
            }
        }
    }
    if dense.is_empty() {
        println!("no dense-u8 glob members found");
        return;
    }

    // 4) per-member run statistics + RLE wire sizes
    let mut run_hist: Map<u64, u64> = Map::new(); // bucketed run lengths
    let mut planes = 0u64; // runs == 256 (full 16x16 planes)
    let mut rle_ok_v = 0u64; // RLE-van wire <= 1024
    let mut rle_ok_t = 0u64;
    let mut per_member: Vec<(u64, u64, u64, u64)> = Vec::new(); // (dense, rv, rt, minwire)
    for (is_t, payload) in &dense {
        let plain = if *is_t { transpose(payload) } else { payload.clone() };
        let rv = rle_wire(&runs(&plain));
        let rt = rle_wire(&runs(&transpose(&plain)));
        for (_g, n) in runs(&plain) {
            let bucket = match n {
                0..=2 => 1,
                3..=8 => 4,
                9..=32 => 16,
                33..=128 => 64,
                129..=255 => 128,
                256 => 256,
                _ => 4096,
            };
            *run_hist.entry(bucket).or_default() += 1;
            if n == 256 {
                planes += 1;
            }
        }
        if (rv.len() as u64) <= 1024 {
            rle_ok_v += 1;
        }
        if (rt.len() as u64) <= 1024 {
            rle_ok_t += 1;
        }
        let mw = rv.len().min(rt.len()) as u64;
        per_member.push((4096, rv.len() as u64, rt.len() as u64, mw));
    }
    let n_members = dense.len() as u64;
    let mut bh: Vec<(u64, u64)> = run_hist.into_iter().collect();
    bh.sort();
    println!("-- dense-u8 members: {} ({}) --", n_members, {
        let t: u64 = n_members * 4096;
        format!("{} B raw", t)
    });
    println!("run-length histogram (runs): {bh:?}");
    println!("full planes (run==256): {planes} / total runs");
    println!(
        "members whose RLE wire <= 1024 B (div=4 admission): van {rle_ok_v} / T {rle_ok_t} / {}",
        n_members
    );

    // 5) whole-class honest A/B at the cold tier
    let stream_a: Vec<u8> = dense.iter().map(|(_, p)| p.clone()).collect::<Vec<Vec<u8>>>().concat();
    // variant B: per-member SMALLER RLE wire (what a per-member min would build)
    let mut stream_b: Vec<u8> = Vec::new();
    for (is_t, payload) in &dense {
        let plain = if *is_t { transpose(payload) } else { payload.clone() };
        let rv = rle_wire(&runs(&plain));
        let rt = rle_wire(&runs(&transpose(&plain)));
        if rv.len() <= rt.len() && rv.len() < 4096 {
            stream_b.extend_from_slice(&rv);
        } else if rt.len() < rv.len() && rt.len() < 4096 {
            stream_b.extend_from_slice(&rt);
        } else {
            stream_b.extend_from_slice(payload);
        }
    }
    // variant C: the uniform per-class route — EVERY member goes RLE-vanilla
    // (uniform predicate keeps cross-member alignment inside the variant)
    let mut stream_c: Vec<u8> = Vec::new();
    for (is_t, payload) in &dense {
        let plain = if *is_t { transpose(payload) } else { payload.clone() };
        stream_c.extend_from_slice(&rle_wire(&runs(&plain)));
    }
    // variant D: uniform per-class RLE-transposed
    let mut stream_d: Vec<u8> = Vec::new();
    for (is_t, payload) in &dense {
        let plain = if *is_t { transpose(payload) } else { payload.clone() };
        stream_d.extend_from_slice(&rle_wire(&runs(&transpose(&plain))));
    }
    // admission-filtered C (only members passing div=4 go RLE, rest stay raw):
    // the honest per-class variant the pack could mint under the existing gate
    let mut stream_e: Vec<u8> = Vec::new();
    for (is_t, payload) in &dense {
        let plain = if *is_t { transpose(payload) } else { payload.clone() };
        let rv = rle_wire(&runs(&plain));
        let rt = rle_wire(&runs(&transpose(&plain)));
        let mw = rv.len().min(rt.len());
        if mw <= 1024 {
            if rv.len() <= rt.len() {
                stream_e.extend_from_slice(&rv);
            } else {
                stream_e.extend_from_slice(&rt);
            }
        } else {
            stream_e.extend_from_slice(payload);
        }
    }
    let ta = std::time::Instant::now();
    let lz_a = lzma2_cold(&stream_a);
    let lz_b = lzma2_cold(&stream_b);
    let lz_c = lzma2_cold(&stream_c);
    let lz_d = lzma2_cold(&stream_d);
    let lz_e = lzma2_cold(&stream_e);
    println!("-- whole-class cold-tier A/B (lzma2 6e lc4, {}) --", ta.elapsed().as_secs_f64());
    println!(
        "A dense          raw={:>9} lz={:>7}",
        stream_a.len(),
        lz_a.len()
    );
    println!(
        "B per-member-min raw={:>9} lz={:>7}  (delta vs A: {})",
        stream_b.len(),
        lz_b.len(),
        lz_b.len() as i64 - lz_a.len() as i64
    );
    println!(
        "C uniform RLE-v  raw={:>9} lz={:>7}  (delta vs A: {})",
        stream_c.len(),
        lz_c.len(),
        lz_c.len() as i64 - lz_a.len() as i64
    );
    println!(
        "D uniform RLE-T  raw={:>9} lz={:>7}  (delta vs A: {})",
        stream_d.len(),
        lz_d.len(),
        lz_d.len() as i64 - lz_a.len() as i64
    );
    println!(
        "E admission-mix  raw={:>9} lz={:>7}  (delta vs A: {})",
        stream_e.len(),
        lz_e.len(),
        lz_e.len() as i64 - lz_a.len() as i64
    );
    let _ = std::fs::remove_dir_all(&dir);
}
