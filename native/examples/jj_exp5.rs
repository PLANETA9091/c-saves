//! jj_exp5 — where is the next chunk-size lever? The bigworld-survival
//! clean run put the ENTIRE container into section slab streams (98.1%),
//! 2.23 GB of it dense block-state streams, and z19 no longer compresses
//! them. This experiment measures, on REAL vanilla-terrain tiles, whether
//! the dense stream itself is the floor or whether a better MODEL beats it:
//!
//!   A  current    — canonical per-section bytes (palette + packed data),
//!                    z19 over chunk-local batches (approximates slabs)
//!   B  H0         — order-0 arithmetic floor: 4096 blocks x H(palette id)
//!   C  H_col      — order-1 VERTICAL conditioning with one global
//!                    P(state | block_below) table (the geology model:
//!                    strata are columns; a block is predicted by the
//!                    block below it in the same column). Two passes:
//!                    pass 1 builds the table, pass 2 scores it — the
//!                    decoder-realizable number.
//!   D  bitmap     — decoder-realizable candidate form: per section, a
//!                    1-bit "same as the block below" bitmap + exception
//!                    (position u16, palette idx u16) list, z19-compressed
//!   E  globalreg  — per-section GLOBAL state ids at fixed width
//!                    (palette bytes eliminated, cross-section similarity
//!                    visible to the window), z19
//!
//! Usage: jj_exp5 <dim-dir>   (a bigworld tile artifact: region/ subdir)

use fastnbt::Value;
use flate2::read::ZlibDecoder;
use std::collections::{HashMap, HashMap as Map};
use std::io::Read;
use std::path::{Path, PathBuf};

fn load_mca(path: &Path) -> Vec<(usize, Vec<u8>)> {
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
        let payload = match data[off + 4] {
            2 => {
                let mut v = Vec::new();
                if ZlibDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            1 => {
                let mut v = Vec::new();
                if flate2::read::GzDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            3 => data[off + 5..off + 4 + len].to_vec(),
            _ => continue,
        };
        out.push((i, payload));
    }
    out
}

/// Canonical state key: Name + sorted Properties (matching CSEC semantics).
fn state_key(b: &Value) -> String {
    let Value::Compound(m) = b else { return format!("{b:?}") };
    let name = match m.get("Name") {
        Some(Value::String(s)) => s.clone(),
        _ => "?".to_string(),
    };
    let mut props: Vec<String> = Vec::new();
    if let Some(Value::Compound(p)) = m.get("Properties") {
        for (k, v) in p.iter() {
            let vs = match v {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            };
            props.push(format!("{k}={vs}"));
        }
        props.sort();
    }
    if props.is_empty() {
        name
    } else {
        format!("{name}[{}]", props.join(","))
    }
}

struct Section {
    y: i32,
    palette: Vec<String>,
    /// 4096 palette indices (empty when the palette is a single entry)
    idx: Vec<u16>,
    /// canonical palette bytes (fastnbt approximation of CSEC palette)
    pal_bytes: usize,
}

fn unpack(data: &[i64], bits: u32) -> Vec<u16> {
    // vanilla non-straddled layout: whole entries per long, right-aligned
    let per = (64 / bits) as usize;
    let mask = ((1u64 << bits) - 1) as u16;
    let mut out = Vec::with_capacity(4096);
    for long in data {
        let l = *long as u64;
        for k in 0..per {
            if out.len() == 4096 {
                break;
            }
            out.push(((l >> (k * bits as usize)) & mask as u64) as u16);
        }
        if out.len() == 4096 {
            break;
        }
    }
    while out.len() < 4096 {
        out.push(0);
    }
    out
}

fn parse_sections(chunk: &[u8]) -> Vec<Section> {
    let Ok(v) = fastnbt::from_bytes::<Value>(chunk) else { return vec![] };
    let Value::Compound(m) = v else { return vec![] };
    let Some(Value::List(secs)) = m.get("sections") else { return vec![] };
    let mut out = Vec::new();
    for s in secs {
        let Value::Compound(sm) = s else { continue };
        let y = match sm.get("Y") {
            Some(Value::Byte(b)) => *b as i32,
            _ => continue,
        };
        let Some(bs) = sm.get("block_states") else { continue };
        let Value::Compound(bm) = bs else { continue };
        let Some(Value::List(pal)) = bm.get("palette") else { continue };
        if pal.is_empty() {
            continue;
        }
        let palette: Vec<String> = pal.iter().map(state_key).collect();
        let pal_bytes = pal
            .iter()
            .map(|p| {
                let mut mm = Map::new();
                mm.insert("p".to_string(), p.clone());
                fastnbt::to_bytes(&Value::Compound(mm))
                    .map(|b| b.len().saturating_sub(5))
                    .unwrap_or(0)
            })
            .sum();
        let idx = if palette.len() <= 1 {
            Vec::new()
        } else {
            let bits = ((palette.len() as u32 - 1).ilog2() + 1).max(4);
            match bm.get("data") {
                Some(Value::LongArray(la)) => unpack(la.as_ref(), bits),
                _ => Vec::new(),
            }
        };
        out.push(Section { y, palette, idx, pal_bytes });
    }
    out.sort_by_key(|s| s.y);
    out
}

fn entropy_bits(freq: &[u64], total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    freq.iter()
        .filter(|&&f| f > 0)
        .map(|&f| {
            let p = f as f64 / total as f64;
            -p * p.log2()
        })
        .sum::<f64>()
        * total as f64
}

fn z19(data: &[u8]) -> usize {
    zstd::bulk::compress(data, 19)
        .map(|c| c.len())
        .unwrap_or(data.len())
}

/// vanilla no-straddle packing (inverse of unpack, LSB-first per long)
fn pack_nostroddle(idx: &[u16], bits: u32) -> Vec<u8> {
    let per = (64 / bits) as usize;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < idx.len() {
        let mut l: u64 = 0;
        for k in 0..per {
            if i >= idx.len() {
                break;
            }
            l |= (idx[i] as u64) << (k * bits as usize);
            i += 1;
        }
        out.extend_from_slice(&l.to_le_bytes());
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).cloned().unwrap_or_default());
    let region_dir = dim.join("region");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".mca"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();

    let t0 = std::time::Instant::now();
    // global registry: state key -> global id
    let mut gids: HashMap<String, u32> = HashMap::new();
    // conditional table: below_gid -> (gid -> count)
    let mut cond: HashMap<u32, HashMap<u32, u64>> = HashMap::new();

    // ---------------- pass 1: build the global P(state | below) table ----
    let mut chunks_seen = 0u64;
    for f in &files {
        for (_slot, payload) in load_mca(f) {
            let secs = parse_sections(&payload);
            if secs.is_empty() {
                continue;
            }
            chunks_seen += 1;
            if chunks_seen % 4000 == 0 {
                println!(
                    "pass1 ... {chunks_seen} chunks, {} gids, {}ms",
                    gids.len(),
                    t0.elapsed().as_millis()
                );
            }
            let mut prev_top: Option<&[u16]> = None;
            let mut prev_pal: Option<&[String]> = None;
            for s in &secs {
                // register this section's palette globally (order-independent)
                let g: Vec<u32> = s
                    .palette
                    .iter()
                    .map(|k| {
                        let next = gids.len() as u32;
                        *gids.entry(k.clone()).or_insert(next)
                    })
                    .collect();
                if s.idx.len() != 4096 {
                    prev_top = None;
                    prev_pal = None;
                    continue;
                }
                // map the previous section's top layer into global ids
                let prev_layer: Option<Vec<u32>> = match (prev_top, prev_pal) {
                    (Some(top), Some(pp)) => Some(
                        top.iter()
                            .map(|&i| {
                                let key = pp[i as usize].clone();
                                let next = gids.len() as u32;
                                *gids.entry(key).or_insert(next)
                            })
                            .collect(),
                    ),
                    _ => None,
                };
                for layer in 0..16u32 {
                    let base = (layer * 256) as usize;
                    for off in 0..256usize {
                        let gid = g[s.idx[base + off] as usize];
                        let below: Option<u32> = if layer == 0 {
                            prev_layer.as_ref().map(|pl| pl[off])
                        } else {
                            Some(g[s.idx[base - 256 + off] as usize])
                        };
                        if let Some(b) = below {
                            *cond
                                .entry(b)
                                .or_default()
                                .entry(gid)
                                .or_insert(0) += 1;
                        }
                    }
                }
                prev_top = Some(&s.idx[15 * 256..]);
                prev_pal = Some(&s.palette);
            }
        }
    }
    println!(
        "pass1 done: {chunks_seen} chunks, {} distinct states, {} conditional rows, {}ms",
        gids.len(),
        cond.len(),
        t0.elapsed().as_millis()
    );

    // ---------------- pass 2: honest measurements with the static table --
    let mut n_sections = 0u64;
    let mut n_dense = 0u64;
    let mut n_single = 0u64;
    let mut cur_bytes = 0u64;
    let mut cur_z19 = 0u64;
    let mut cur_dense_z19 = 0u64;
    let mut h0_bits = 0f64;
    let mut hcol_bits = 0f64;
    let mut bitmap_z19 = 0u64;
    let mut bm_sample_sum = 0u64;
    let mut bm_sample_n = 0u64;
    let mut glob_z19 = 0u64;
    let mut pal_bytes = 0u64;
    let mut bitmap_raw = 0u64;
    let mut excepts_total = 0u64;
    let mut same_pct_sum = 0f64;

    chunks_seen = 0;
    let glob_bits = ((gids.len() as u32 - 1).ilog2() + 1).max(1) as usize;
    for f in &files {
        for (_slot, payload) in load_mca(f) {
            let secs = parse_sections(&payload);
            if secs.is_empty() {
                continue;
            }
            chunks_seen += 1;
            if chunks_seen % 4000 == 0 {
                println!("pass2 ... {chunks_seen} chunks, {}ms", t0.elapsed().as_millis());
            }
            let mut batch: Vec<u8> = Vec::new();
            let mut batch_dense: Vec<u8> = Vec::new();
            let mut batch_glob: Vec<u8> = Vec::new();
            let mut prev_top: Option<&[u16]> = None;
            let mut prev_pal: Option<&[String]> = None;
            for s in &secs {
                n_sections += 1;
                pal_bytes += s.pal_bytes as u64;
                let g: Vec<u32> = s
                    .palette
                    .iter()
                    .map(|k| *gids.get(k).expect("pass1 registered all states"))
                    .collect();
                if s.palette.len() <= 1 {
                    n_single += 1;
                    cur_bytes += s.pal_bytes as u64;
                    batch.extend_from_slice(s.palette[0].as_bytes());
                    batch_glob.push(g[0] as u8);
                    prev_top = None;
                    prev_pal = None;
                    continue;
                }
                if s.idx.len() != 4096 {
                    prev_top = None;
                    prev_pal = None;
                    continue;
                }
                n_dense += 1;
                let bits = ((s.palette.len() as u32 - 1).ilog2() + 1).max(4);
                let packed = pack_nostroddle(&s.idx, bits);
                let packed_len = packed.len();
                cur_bytes += (s.pal_bytes + packed_len) as u64;
                batch.extend_from_slice(&s.pal_bytes.to_le_bytes());
                batch.extend_from_slice(&packed);
                // the like-for-like baseline: dense sections ONLY, same set
                // every scheme below is measured on
                batch_dense.extend_from_slice(&s.pal_bytes.to_le_bytes());
                batch_dense.extend_from_slice(&packed);
                // B: order-0
                let mut freq = vec![0u64; s.palette.len()];
                for &i in &s.idx {
                    freq[i as usize] += 1;
                }
                h0_bits += entropy_bits(&freq, 4096);
                // E: global-id stream at fixed width
                let gw = (glob_bits + 7) / 8;
                for &i in &s.idx {
                    let v = g[i as usize];
                    let gbuf = v.to_le_bytes();
                    batch_glob.extend_from_slice(&gbuf[..gw]);
                }
                // C + D: vertical conditioning (static table) + bitmap form
                let prev_layer: Option<Vec<u32>> = match (prev_top, prev_pal) {
                    (Some(top), Some(pp)) => Some(
                        top.iter()
                            .map(|&i| *gids.get(&pp[i as usize]).expect("registered"))
                            .collect(),
                    ),
                    _ => None,
                };
                let mut bitmap = vec![0u8; 512];
                let mut excepts: Vec<u8> = Vec::new();
                let mut same_cnt = 0u64;
                let mut col_bits = 0f64;
                for layer in 0..16u32 {
                    let base = (layer * 256) as usize;
                    for off in 0..256usize {
                        let pos = base + off;
                        let gid = g[s.idx[pos] as usize];
                        let below: Option<u32> = if layer == 0 {
                            prev_layer.as_ref().map(|pl| pl[off])
                        } else {
                            Some(g[s.idx[pos - 256] as usize])
                        };
                        if let Some(b) = below {
                            // honest conditional probability from the FINAL table
                            let t = cond.get(&b).map(|m| m.values().sum::<u64>()).unwrap_or(0);
                            let c = cond.get(&b).and_then(|m| m.get(&gid)).copied().unwrap_or(0);
                            let p = if t > 0 { c as f64 / t as f64 } else { 0.0 };
                            col_bits += -p.max(1e-9).log2();
                            if b == gid {
                                same_cnt += 1;
                                bitmap[pos / 8] |= 1 << (pos % 8);
                            } else {
                                excepts.extend_from_slice(&(pos as u16).to_le_bytes());
                                excepts.extend_from_slice(&(s.idx[pos] as u16).to_le_bytes());
                            }
                        }
                    }
                }
                hcol_bits += col_bits;
                excepts_total += (excepts.len() / 4) as u64;
                bitmap_raw += (512 + excepts.len()) as u64;
                same_pct_sum += same_cnt as f64 / 4096.0 * 100.0;
                let mut form = bitmap;
                form.extend_from_slice(&excepts);
                // z19 per section is the expensive call (hundreds of
                // thousands of dense sections on a tile pair): measure on
                // every 16th dense section and scale
                bm_sample_sum += z19(&form) as u64;
                bm_sample_n += 1;
                if n_dense % 16 == 0 {
                    bitmap_z19 = bm_sample_sum * n_dense / bm_sample_n.max(1);
                }
                prev_top = Some(&s.idx[15 * 256..]);
                prev_pal = Some(&s.palette);
            }
            cur_z19 += z19(&batch) as u64;
            cur_dense_z19 += z19(&batch_dense) as u64;
            glob_z19 += z19(&batch_glob) as u64;
        }
    }

    println!("\n==== jj_exp5 results ====");
    println!("chunks: {chunks_seen} | files: {}", files.len());
    println!(
        "sections: {n_sections} total, {n_dense} dense (multi-state), {n_single} single-state"
    );
    println!("global registry: {} distinct states ({}-bit ids)", gids.len(), glob_bits);
    println!(
        "A  current canonical bytes : {:>12} (palettes {} = {:.1}%)",
        cur_bytes,
        pal_bytes,
        100.0 * pal_bytes as f64 / cur_bytes.max(1) as f64
    );
    println!("A' z19(chunk batches)      : {:>12}", cur_z19);
    println!(
        "A'' z19(DENSE-ONLY batches) : {:>12}   <- baseline for the prizes below",
        cur_dense_z19
    );
    println!(
        "B  H0 arithmetic floor     : {:>12} ({:.0} KiB)",
        h0_bits as u64 / 8,
        h0_bits / 8.0 / 1024.0
    );
    println!(
        "C  H_col vertical floor    : {:>12} ({:.0} KiB)  [static table, decoder-realizable]",
        hcol_bits as u64 / 8,
        hcol_bits / 8.0 / 1024.0
    );
    println!(
        "D  bitmap+exceptions z19   : {:>12} (raw {}, excepts {}, same-bits {:.1}%)",
        bitmap_z19,
        bitmap_raw,
        excepts_total,
        same_pct_sum / n_dense.max(1) as f64
    );
    println!("E  global-ids z19          : {:>12} ({}B/block)", glob_z19, (glob_bits + 7) / 8);
    println!();
    if cur_dense_z19 > 0 {
        println!("prize vs A'' (B H0)      : x{:.2}", cur_dense_z19 as f64 / (h0_bits as u64 / 8).max(1) as f64);
        println!("prize vs A'' (C H_col)   : x{:.2}", cur_dense_z19 as f64 / (hcol_bits as u64 / 8).max(1) as f64);
        println!("prize vs A'' (D bitmap)  : x{:.2}", cur_dense_z19 as f64 / bitmap_z19.max(1) as f64);
        println!("prize vs A'' (E global)  : x{:.2}", cur_dense_z19 as f64 / glob_z19.max(1) as f64);
    }
}
