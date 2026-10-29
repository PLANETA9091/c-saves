//! jj_exp4 — cross-chunk delta potential of DENSE block_states sections.
//!
//! v0.7.0's shift-ref only matches vertical shifts WITHIN one chunk; JJThunder
//! mountains separate dense (surface-transition) sections by 50-150 uniform
//! planes, so the k<=15 window almost never fires between dense sections.
//! The untested lever: neighbouring CHUNKS have similar surfaces — a dense
//! section delta'd against a NEIGHBOUR's dense section (same palette family)
//! may be far cheaper than its own packed stream.
//!
//! Measures, per dense section (multi-entry palette + data array):
//!   baseline  = zstd-19 of the canonical packed stream (current container form)
//!   proposed  = zstd-19 of zigzag-varint delta vs the best neighbour base
//!               (+ u16 base ref cost), neighbours = same canonical palette,
//!               chunks within Chebyshev R, any Y.
//!
//! Both sides get the same per-section z19 model (conservative equally).

use fastnbt::Value;
use flate2::read::ZlibDecoder;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

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

/// no-straddle unpack of a vanilla LongArray into `n` entries of `bits`.
fn unpack_wide(la: &[i64], n: usize, bits: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(n);
    let mut acc: u64 = 0;
    let mut accb = 0u32;
    let mask = (1u64 << bits) - 1;
    for &l in la {
        let mut l = l as u64;
        let mut left = 64u32;
        while left > 0 && out.len() < n {
            if accb == 0 && left >= bits {
                out.push((l & mask) as u32);
                l >>= bits;
                left -= bits;
            } else {
                let take = (bits - accb).min(left);
                acc |= (l & ((1u64 << take) - 1)) << accb;
                l >>= take;
                accb += take;
                left -= take;
                if accb == bits {
                    out.push((acc & mask) as u32);
                    acc = 0;
                    accb = 0;
                }
            }
        }
    }
    while out.len() < n {
        out.push(0);
    }
    out
}

fn pal_identity(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Compound(cm) => {
            let Value::String(name) = cm.get("Name")? else { return None };
            let mut props: Vec<(String, String)> = Vec::new();
            if let Some(Value::Compound(pn)) = cm.get("Properties") {
                for (k, kv) in pn {
                    let Value::String(vv) = kv else { return None };
                    props.push((k.clone(), vv.clone()));
                }
            }
            props.sort();
            let ps: String = props.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
            Some(format!("{name}[{ps}]"))
        }
        _ => None,
    }
}

fn zigzag_varint(delta: &[i32], out: &mut Vec<u8>) {
    for &d in delta {
        let z = ((d << 1) ^ (d >> 31)) as u64;
        let mut z = z;
        loop {
            let b = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
}

struct Member {
    cx: i32,
    cz: i32,
    y: i32,
    packed: Vec<u8>,   // canonical no-straddle packed bytes (current form)
    entries: Vec<u32>, // canonical indices
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).cloned().unwrap_or_default());
    let region_dir = dir.join("region");
    let neigh_r: i32 = std::env::var("JJ4_R").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let max_cand: usize = std::env::var("JJ4_CAND").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let sample: usize = std::env::var("JJ4_SAMPLE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".mca")).collect())
        .unwrap_or_default();
    files.sort();

    // pal_key -> members
    let mut groups: Map<u128, Vec<Member>> = Map::new();
    let mut n_dense = 0usize;
    let mut n_uniform = 0usize;
    let mut seen_chunks = 0usize;

    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
        if parts.len() != 3 {
            continue;
        }
        let (rx, rz): (i32, i32) = (parts[1].parse().unwrap_or(0), parts[2].parse().unwrap_or(0));
        for (slot, payload) in load_mca(f) {
            let Ok(v) = fastnbt::from_bytes::<Value>(&payload) else { continue };
            let Value::Compound(m) = v else { continue };
            let Some(Value::List(secs)) = m.get("sections") else { continue };
            let cx = rx * 32 + (slot & 31) as i32;
            let cz = rz * 32 + (slot >> 5) as i32;
            seen_chunks += 1;
            for s in secs {
                let Value::Compound(sm) = s else { continue };
                let y = match sm.get("Y") {
                    Some(Value::Byte(b)) => *b as i32,
                    _ => continue,
                };
                let Some(Value::Compound(bs)) = sm.get("block_states") else {
                    continue;
                };
                let Some(Value::List(pal)) = bs.get("palette") else { continue };
                if pal.len() < 2 {
                    n_uniform += 1;
                    continue;
                }
                let Some(Value::LongArray(la)) = bs.get("data") else {
                    n_uniform += 1; // single-value palette despite len>=2 shape
                    continue;
                };
                if seen_chunks % sample != 0 {
                    continue;
                }
                // canonical order = sorted identity strings (CSEC order)
                let mut ids: Vec<String> = Vec::with_capacity(pal.len());
                for p in pal {
                    match pal_identity(p) {
                        Some(s) => ids.push(s),
                        None => {
                            ids.clear();
                            break;
                        }
                    }
                }
                if ids.is_empty() {
                    continue;
                }
                let mut order: Vec<usize> = (0..ids.len()).collect();
                order.sort_by(|a, b| ids[*a].cmp(&ids[*b]));
                let mut orig2canon = vec![0u32; ids.len()];
                for (ci, &oi) in order.iter().enumerate() {
                    orig2canon[oi] = ci as u32;
                }
                let n = ids.len();
                let bits: u32 = ((la.as_ref().len() as u32) * 64).div_ceil(4096);
                let bits = bits.max(4);
                let raw = unpack_wide(la.as_ref(), 4096, bits);
                if raw.iter().any(|&x| x as usize >= n) {
                    continue; // malformed
                }
                let canon: Vec<u32> = raw.iter().map(|x| orig2canon[*x as usize]).collect();
                // canonical packed bytes at max(4, ceillog2(n))
                let cbits = bits_for(n);
                let packed = pack_canon(&canon, cbits);
                let pal_key = c_saves::chunknbt::hash_of(ids.join("\u{1}").as_bytes());
                groups.entry(pal_key).or_default().push(Member {
                    cx,
                    cz,
                    y,
                    packed,
                    entries: canon,
                });
                n_dense += 1;
            }
        }
    }
    println!(
        "corpus: {seen_chunks} chunks, {n_uniform} uniform sections, {n_dense} dense sections, {} palette groups",
        groups.len()
    );

    let mut base_total = 0u64;
    let mut delta_total = 0u64;
    let mut dups = 0usize;
    let mut tried = 0usize;
    let mut no_cand = 0usize;
    let mut best_zero = 0usize;
    let mut per_group: Vec<(String, usize, u64, u64)> = Vec::new();

    for (pk, members) in groups.iter_mut() {
        members.sort_by_key(|m| (m.cx, m.cz, m.y));
        let mut g_base = 0u64;
        let mut g_delta = 0u64;
        for i in 0..members.len() {
            let m = &members[i];
            tried += 1;
            // candidates: same palette, chunk within Chebyshev R, any Y
            let mut cands: Vec<usize> = Vec::new();
            for j in 0..members.len() {
                if i == j {
                    continue;
                }
                let b = &members[j];
                if (b.cx - m.cx).abs() <= neigh_r && (b.cz - m.cz).abs() <= neigh_r {
                    cands.push(j);
                    if cands.len() >= max_cand * 4 {
                        break;
                    }
                }
            }
            if cands.is_empty() {
                no_cand += 1;
                g_base += m.packed.len() as u64;
                g_delta += m.packed.len() as u64;
                continue;
            }
            // rank candidates by Hamming distance on a strided sample (cheap)
            let stride = m.entries.len() / 256.max(1);
            cands.sort_by_key(|&j| {
                let b = &members[j];
                let mut d = 0u32;
                let mut k = 0;
                while k < m.entries.len() {
                    if m.entries[k] != b.entries[k] {
                        d += 1;
                    }
                    k += stride;
                }
                (d, (b.y - m.y).abs())
            });
            cands.truncate(max_cand);
            // exact-dup check (production dedups these already)
            if cands
                .iter()
                .any(|&j| members[j].entries == m.entries)
            {
                dups += 1;
                continue; // costs ~0 in production via content dedup
            }
            // baseline: z19 of the packed stream
            let zbase = zstd19(&m.packed);
            g_base += zbase.len() as u64;
            // best delta among candidates
            let mut best: Option<Vec<u8>> = None;
            let mut zero_delta = false;
            for &j in &cands {
                let b = &members[j];
                let delta: Vec<i32> = m
                    .entries
                    .iter()
                    .zip(b.entries.iter())
                    .map(|(a, c)| *a as i32 - *c as i32)
                    .collect();
                if delta.iter().all(|d| *d == 0) {
                    zero_delta = true;
                    break;
                }
                let mut vb = Vec::with_capacity(delta.len());
                zigzag_varint(&delta, &mut vb);
                let zd = zstd19(&vb);
                if best.as_ref().map(|cur| zd.len() < cur.len()).unwrap_or(true) {
                    best = Some(zd);
                }
            }
            if zero_delta {
                best_zero += 1;
                continue;
            }
            match best {
                Some(zd) => g_delta += (zd.len() + 2) as u64,
                None => g_delta += m.packed.len() as u64,
            }
        }
        base_total += g_base;
        delta_total += g_delta;
        if members.len() > 50 {
            per_group.push((
                format!("pal#{:016x}", pk & 0xffff_ffff_ffff_ffff),
                members.len(),
                g_base,
                g_delta,
            ));
        }
    }

    println!("tried {tried}, exact dups {dups} (free via dedup), zero-delta {best_zero}, no-candidate {no_cand}");
    println!(
        "baseline z19 packed: {:.2} MiB | delta-vs-neighbour + ref: {:.2} MiB | ratio x{:.2}",
        mib(base_total),
        mib(delta_total),
        base_total as f64 / delta_total.max(1) as f64
    );
    per_group.sort_by_key(|x| (x.3 * 100) / x.2.max(1));
    for (name, n, b, d) in per_group.iter().take(12) {
        println!(
            "  {name}: {n} sections, base {:.1} KiB -> delta {:.1} KiB (x{:.2})",
            *b as f64 / 1024.0,
            *d as f64 / 1024.0,
            *b as f64 / (*d).max(1) as f64
        );
    }
}

fn bits_for(n: usize) -> u32 {
    ((usize::BITS - (n.max(2) as u32).leading_zeros()) as usize).max(4) as u32
}

fn pack_canon(entries: &[u32], bits: u32) -> Vec<u8> {
    // no-straddle packing, little-endian longs, then raw bytes (matches the
    // CSEC dense form closely enough for the z19 model)
    let per = (64 / bits) as usize;
    let mut out = Vec::with_capacity((4096 + per - 1) / per * 8);
    let mask = (1u64 << bits) - 1;
    let mut chunk = 0u64;
    let mut filled = 0usize;
    for &e in entries {
        chunk |= (e as u64 & mask) << (filled as u32 * bits);
        filled += 1;
        if filled == per {
            out.extend_from_slice(&chunk.to_le_bytes());
            chunk = 0;
            filled = 0;
        }
    }
    if filled > 0 {
        out.extend_from_slice(&chunk.to_le_bytes());
    }
    out
}

fn zstd19(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 19).unwrap_or_else(|_| data.to_vec())
}
