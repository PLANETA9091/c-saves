//! lzma_dict_probe: cross-slab redundancy probe. The cold tier compresses
//! each slab independently, so repeats ACROSS slabs (same structure NBT
//! strings, same cave strata patterns in different regions of a big world)
//! are lost. LZMA2 supports a preset dictionary (lzma_options_lzma.
//! preset_dict) which the liblzma crate does not expose — this probe pokes
//! it through liblzma-sys (LzmaOptions is a single-field wrapper of the C
//! struct, so a layout-identical struct is transmuted back).
//!
//! Method: corpus members are grouped at a FORCED small cap (512 KiB) to
//! fabricate a many-slab world, then every slab is compressed (a) tuned
//! preset-6|E lc4/pb0 alone, (b) with a shared preset dict sampled from the
//! corpus, (c) with the previous slab's tail as the dict (streaming style,
//! not container-representable — upper bound reference). Decode roundtrip
//! is checked for (b) — the wire would carry the dict in dicts/ like the
//! zstd dictionaries do.
//!
//! Usage: lzma_dict_probe <dim-dir> [cap_kib]

use c_saves::chunknbt::{self, SecBytes, Split};
use liblzma_sys as sys;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
            3 => comp.clone(),
            _ => continue,
        };
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

/// Build the crate's LzmaOptions from a raw C struct (single-field wrapper:
/// layout-identical by construction; the sys crate version is pinned in
/// Cargo.lock). Applies preset 6|EXTREME + the tuned literal coder and the
/// optional preset dict.
fn tuned_opts(dict: Option<&[u8]>) -> liblzma::stream::LzmaOptions {
    unsafe {
        let mut raw: sys::lzma_options_lzma = std::mem::zeroed();
        // xz's lzma_lzma_preset returns 0 (false) on SUCCESS — the inverted
        // convention the liblzma crate also maps (ret != 0 -> Err)
        assert_eq!(sys::lzma_lzma_preset(&mut raw, 6 | sys::LZMA_PRESET_EXTREME), 0u8);
        raw.lc = 4;
        raw.lp = 0;
        raw.pb = 0;
        if let Some(d) = dict {
            raw.preset_dict = d.as_ptr();
            raw.preset_dict_size = d.len() as u32;
        }
        std::mem::transmute::<sys::lzma_options_lzma, liblzma::stream::LzmaOptions>(raw)
    }
}

fn lzma2_enc(data: &[u8], dict: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&tuned_opts(dict));
    let mut s = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    s.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

fn lzma2_dec(data: &[u8], raw_len: usize, dict: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&tuned_opts(dict));
    let mut s = liblzma::stream::Stream::new_raw_decoder(&f).ok()?;
    let mut out = Vec::with_capacity(raw_len);
    s.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    Some(out)
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: lzma_dict_probe <dim-dir>"));
    let cap: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .map(|k: usize| k * 1024)
        .unwrap_or(512 * 1024);

    let mut seen: HashSet<u128> = HashSet::new();
    let mut members: Vec<Vec<u8>> = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dim.join("region"))
        .expect("region dir")
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    for f in files {
        if f.extension().map(|e| e != "mca").unwrap_or(true) {
            continue;
        }
        for (_cx, _cz, payload) in load_mca(&f) {
            if let Ok(Split::Split { sections, .. }) =
                chunknbt::split(&payload, false, None, true)
            {
                for (h, sb) in &sections {
                    if seen.insert(*h) {
                        members.push(match sb {
                            SecBytes::Shared(a) => (**a).clone(),
                            SecBytes::Owned(b) => b.clone(),
                        });
                    }
                }
            }
        }
    }

    // many-slab grouping at the forced cap (24 B entry rows, pack order)
    let mut groups: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_raw = 24usize;
    for m in &members {
        let need = 24 + m.len();
        if !cur.is_empty() && cur_raw + need > cap {
            groups.push(std::mem::take(&mut cur));
            cur_raw = 24;
        }
        cur.extend_from_slice(&(0u32).to_le_bytes());
        cur.extend_from_slice(&(m.len() as u32).to_le_bytes());
        cur.extend_from_slice(&[0u8; 16]);
        cur.extend_from_slice(m);
        cur_raw += need;
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    let total_raw: u64 = groups.iter().map(|g| g.len() as u64).sum();
    println!(
        "members {} -> {} slabs at {} KiB cap, stream {:.2} MiB",
        members.len(),
        groups.len(),
        cap / 1024,
        total_raw as f64 / (1 << 20) as f64
    );

    // shared dict: deterministic sample — the head of every 4th member up to
    // the 256 KiB budget (a trained dict would do better; this is the floor)
    let mut dict: Vec<u8> = Vec::new();
    for (i, m) in members.iter().enumerate() {
        if i % 4 == 0 && dict.len() + m.len() <= 256 * 1024 {
            dict.extend_from_slice(m);
        }
        if dict.len() >= 256 * 1024 {
            break;
        }
    }
    println!("shared dict: {} KiB", dict.len() / 1024);

    let t = Instant::now();
    let mut plain = 0u64;
    for g in &groups {
        plain += lzma2_enc(g, None).expect("enc").len() as u64;
    }
    println!("plain (tuned):          {:>9} B  wall {:.1}s", plain, t.elapsed().as_secs_f64());

    let t = Instant::now();
    let mut shared = 0u64;
    for g in &groups {
        shared += lzma2_enc(g, Some(&dict)).expect("enc").len() as u64;
    }
    println!(
        "shared preset dict:     {:>9} B  ({:+.2}%) wall {:.1}s",
        shared,
        (shared as f64 - plain as f64) / plain as f64 * 100.0,
        t.elapsed().as_secs_f64()
    );

    // decode roundtrip of the preset-dict form (wire would carry the dict)
    let back = lzma2_dec(&lzma2_enc(&groups[0], Some(&dict)).expect("enc"), groups[0].len(), Some(&dict))
        .expect("dec");
    assert_eq!(back, groups[0], "preset-dict roundtrip");

    // streaming-style upper bound: dict = previous slab's head (not
    // container-representable as-is — reference only)
    let mut prev_head: Vec<u8> = Vec::new();
    let mut stream = 0u64;
    for (i, g) in groups.iter().enumerate() {
        let d = if i == 0 { None } else { Some(&prev_head[..]) };
        let c = lzma2_enc(g, d).expect("enc");
        stream += c.len() as u64;
        prev_head.clear();
        let take = g.len().min(256 * 1024);
        prev_head.extend_from_slice(&g[..take]);
    }
    println!(
        "prev-slab dict (bound): {:>9} B  ({:+.2}%)",
        stream,
        (stream as f64 - plain as f64) / plain as f64 * 100.0
    );
    // ---- row-format ladder: what does the on-disk metadata cost? ----
    // The store writes per-member rows [zigzag id-delta u64][raw_len u64]
    // [hash hi u64][hash lo u64] (~26-32 B) INSIDE the compressed stream;
    // the 16 B content hashes are near-incompressible. Measure the
    // compressed cost of the hash column and of fat fixed-width columns.
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    fn rows_for(members: &[Vec<u8>], ids: &[u64], mode: u8) -> Vec<u8> {
        // pack-order grouping at 8 MiB (single group for this corpus)
        let mut g: Vec<u8> = Vec::new();
        let mut prev: i64 = 0;
        for (i, m) in members.iter().enumerate() {
            match mode {
                0 => {
                    // current wire: zz(id-delta) u64 + len u64 + hash 16
                    let zz = ((ids[i] as i64 - prev) << 1) ^ (ids[i] as i64 - prev >> 63);
                    g.extend_from_slice(&(zz as u64).to_le_bytes());
                    g.extend_from_slice(&(m.len() as u64).to_le_bytes());
                    let h = xxhash_rust::xxh3::xxh3_128(m);
                    g.extend_from_slice(&h.to_le_bytes());
                }
                1 => {
                    // no hash column
                    let zz = ((ids[i] as i64 - prev) << 1) ^ (ids[i] as i64 - prev >> 63);
                    g.extend_from_slice(&(zz as u64).to_le_bytes());
                    g.extend_from_slice(&(m.len() as u64).to_le_bytes());
                }
                2 => {
                    // varint id-delta + varint len, no hash
                    let d = ids[i] as i64 - prev;
                    varint(((d << 1) ^ (d >> 63)) as u64, &mut g);
                    varint(m.len() as u64, &mut g);
                }
                4 => {
                    // TRUE current wire: varint zz(id-delta) + varint len + 16B hash
                    let d = ids[i] as i64 - prev;
                    varint(((d << 1) ^ (d >> 63)) as u64, &mut g);
                    varint(m.len() as u64, &mut g);
                    let h = xxhash_rust::xxh3::xxh3_128(m);
                    g.extend_from_slice(&h.to_le_bytes());
                }
                3 => {
                    // varint id-delta + varint len + 4 B xxh32
                    let d = ids[i] as i64 - prev;
                    varint(((d << 1) ^ (d >> 63)) as u64, &mut g);
                    varint(m.len() as u64, &mut g);
                    let h32 = xxhash_rust::xxh3::xxh3_64(m) as u32;
                    g.extend_from_slice(&h32.to_le_bytes());
                }
                _ => unreachable!(),
            }
            prev = ids[i] as i64;
            g.extend_from_slice(m);
        }
        g
    }
    let ids: Vec<u64> = (0..members.len() as u64).collect(); // dense ids, pack order
    let names = [
        "rows: id8+len8+hash16",
        "rows: id8+len8 (no hash)",
        "rows: vid+vlen (no hash)  = HASHLESS WIRE",
        "rows: vid+vlen+xxh32",
        "rows: vid+vlen+hash16 (TRUE current)",
    ];
    let mut base = 0u64;
    let mut true_cur = 0u64;
    for mode in [0u8, 1, 2, 3, 4] {
        if mode == 4 {
            true_cur = lzma2_enc(&rows_for(&members, &ids, 4), None).expect("enc").len() as u64;
        }
        let g = rows_for(&members, &ids, mode);
        let c = lzma2_enc(&g, None).expect("enc").len() as u64;
        if mode == 4 {
            println!(
                "rowfmt {:<34} raw {:>9} B -> comp {:>9} B",
                names[mode as usize], g.len(), c
            );
            println!("HASHLESS vs TRUE current: {:+.2}%", (c as f64 - true_cur as f64) / true_cur as f64 * 100.0);
            continue;
        }
        if mode == 0 {
            base = c;
        }
        println!(
            "rowfmt {:<34} raw {:>9} B -> comp {:>9} B ({:+.2}% vs fixed-width)",
            names[mode as usize],
            g.len(),
            c,
            (c as f64 - base as f64) / base as f64 * 100.0
        );
    }

}
