//! tail_hm_probe: the heightmap stream's remaining floor. The stored tails
//! carry 0xC6 v2 (first-order zigzag-varint delta chain, column order).
//! This probe prices the NEXT representations on the real drained tail
//! members of a store (end tails = 97% of the end slab mass):
//!   A baseline   : stored bodies rebuilt verbatim through one writer
//!   B second     : second-order delta chain (d2 = v - 2v[-1] + v[-2])
//!   C serp+first : serpentine scan (boustrophedon column walk) + first-order
//!   D serp+second: serpentine scan + second-order
//!   E nibble     : 4-bit zigzag nibble deltas (escape varint), column order
//!   F serp+nibble: serpentine + 4-bit nibble deltas
//! plus a census (delta magnitudes, second-order zero share) and the
//! no-heightmap remainder marginal. All variants re-serialize EVERY tail
//! through the same writer so the A/B only differs in the heightmap wire.
//!
//! Usage: tail_hm_probe <store-dir>

use c_saves::store::Store;
use fastnbt::Value;
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

/// decode a stored 0xC6 v2 body -> 256 values (mirror of chunknbt::hmap_decode)
fn hm_v2_decode(b: &[u8]) -> Option<Vec<i64>> {
    if b.first() != Some(&0xC6) || b.get(1) != Some(&2u8) {
        return None;
    }
    let mut p = 2usize;
    let mut out = Vec::with_capacity(256);
    let mut prev: i64 = 0;
    for _ in 0..256 {
        let mut z = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *b.get(p)?;
            p += 1;
            z |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
        let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
        prev += d;
        if !(0..512).contains(&prev) {
            return None;
        }
        out.push(prev);
    }
    Some(out)
}

fn zig(zv: i64) -> u64 {
    ((zv << 1) ^ (zv >> 63)) as u64
}
fn push_varint(out: &mut Vec<u8>, mut z: u64) {
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

/// serpentine permutation of the 256 column indices: row z runs x 0..16,
/// even rows forward, odd rows backward (keeps the walk spatially continuous).
fn serp() -> Vec<usize> {
    let mut p = Vec::with_capacity(256);
    for z in 0..16i64 {
        for x in 0..16i64 {
            let xx = if z % 2 == 0 { x } else { 15 - x };
            p.push((xx + z * 16) as usize);
        }
    }
    p
}

// ---- variant encoders: (values[256]) -> 0xC6 body bytes ----

fn v2_reencode(vals: &[i64]) -> Vec<u8> {
    let mut out = vec![0xC6, 2];
    let mut prev = 0i64;
    for v in vals {
        push_varint(&mut out, zig(v - prev));
        prev = *v;
    }
    out
}

fn v3_second(vals: &[i64]) -> Vec<u8> {
    // [0xC6][3] = second-order chain: first entry absolute zigzag, second
    // entry first-order, then d2[i] = v[i] - 2v[i-1] + v[i-2] zigzag varint.
    let mut out = vec![0xC6, 3];
    let mut p1 = 0i64;
    let mut p2 = 0i64;
    for (i, v) in vals.iter().enumerate() {
        let d = if i == 0 {
            *v
        } else if i == 1 {
            *v - p1
        } else {
            *v - 2 * p1 + p2
        };
        push_varint(&mut out, zig(d));
        p2 = p1;
        p1 = *v;
    }
    out
}

fn v4_serp_first(vals: &[i64], p: &[usize]) -> Vec<u8> {
    let mut out = vec![0xC6, 4];
    let mut prev = 0i64;
    for &i in p {
        push_varint(&mut out, zig(vals[i] - prev));
        prev = vals[i];
    }
    out
}

fn v5_serp_second(vals: &[i64], p: &[usize]) -> Vec<u8> {
    let mut out = vec![0xC6, 5];
    let mut p1 = 0i64;
    let mut p2 = 0i64;
    for (k, &i) in p.iter().enumerate() {
        let d = if k == 0 {
            vals[i]
        } else if k == 1 {
            vals[i] - p1
        } else {
            vals[i] - 2 * p1 + p2
        };
        push_varint(&mut out, zig(d));
        p2 = p1;
        p1 = vals[i];
    }
    out
}

fn nib_push(out: &mut Vec<u8>, n: u8, low: bool) {
    if low {
        out.push(n & 0x0f);
    } else {
        let l = out.len() - 1;
        out[l] |= (n & 0x0f) << 4;
    }
}

fn v6_nibble(vals: &[i64], p: &[usize], serp: bool) -> Vec<u8> {
    // [0xC6][6] = 4-bit nibble zigzag deltas, LSB-first nibble packing.
    // zigzag z in 0..=7 -> one nibble; else nibble 0x8 escape + varint z.
    // zigzag: 0=0 1=+1 2=-1 3=+2 4=-2 5=+3 6=-3 7=+4 (escape beyond).
    let mut out = vec![0xC6, 6];
    let mut prev = 0i64;
    let mut low = true;
    for &i in p {
        let d = vals[i] - prev;
        prev = vals[i];
        let z = zig(d);
        if z <= 7 {
            nib_push(&mut out, z as u8, low);
            low = !low;
        } else {
            nib_push(&mut out, 0x8, low);
            low = true;
            push_varint(&mut out, z);
        }
    }
    if !low && serp {
        // unreachable bookkeeping (kept for symmetric call sites)
    }
    out
}

fn replace_heightmaps(v: &Value, map_fn: &dyn Fn(&[u8]) -> Vec<u8>, strip: bool) -> Value {
    match v {
        Value::Compound(m) => {
            let mut out = std::collections::HashMap::new();
            for (k, val) in m {
                if k == "Heightmaps" {
                    if strip {
                        continue;
                    }
                    if let Value::Compound(hm) = val {
                        let mut nh = std::collections::HashMap::new();
                        for (mk, mv) in hm {
                            if let Value::ByteArray(arr) = mv {
                                let u: Vec<u8> = arr.iter().map(|x| *x as u8).collect();
                                let w: Vec<i8> = map_fn(&u).into_iter().map(|x| x as i8).collect();
                                nh.insert(
                                    mk.clone(),
                                    Value::ByteArray(fastnbt::ByteArray::new(w)),
                                );
                            } else {
                                nh.insert(mk.clone(), mv.clone());
                            }
                        }
                        out.insert(k.clone(), Value::Compound(nh));
                    } else {
                        out.insert(k.clone(), val.clone());
                    }
                } else {
                    out.insert(k.clone(), val.clone());
                }
            }
            Value::Compound(out)
        }
        other => other.clone(),
    }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: tail_hm_probe <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let tails: Vec<Vec<u8>> = members
        .into_iter()
        .filter(|(c, _)| c == "tail_nbt")
        .map(|(_, b)| b)
        .collect();
    println!("tail members: {}", tails.len());

    let p = serp();
    let mut streams: Vec<(&str, Vec<u8>)> = vec![
        ("A_base", vec![]),
        ("B_second", vec![]),
        ("C_serp1", vec![]),
        ("D_serp2", vec![]),
        ("E_nib", vec![]),
        ("F_serpnib", vec![]),
        ("G_nohm", vec![]),
    ];
    let mut parsed = 0usize;
    let mut maps = 0usize;
    // census
    let (mut d_tot, mut d_le7, mut d_le3, mut d_zero) = (0u64, 0u64, 0u64, 0u64);
    let (mut s2_tot, mut s2_zero, mut s2_le3) = (0u64, 0u64, 0u64);
    let mut hm_raw = 0u64;
    for t in &tails {
        let Ok(Value::Compound(m)) = fastnbt::from_bytes::<Value>(t) else {
            continue;
        };
        parsed += 1;
        // baseline rebuild
        let base = Value::Compound(m.clone());
        if let Ok(b) = fastnbt::to_bytes(&base) {
            streams[0].1.extend_from_slice(&b);
        }
        // collect heightmap values
        let mut vals_map: std::collections::HashMap<String, Vec<i64>> =
            std::collections::HashMap::new();
        if let Some(Value::Compound(hm)) = m.get("Heightmaps") {
            for (mk, mv) in hm {
                if let Value::ByteArray(arr) = mv {
                    let au: Vec<u8> = arr.iter().map(|x| *x as u8).collect();
                    if let Some(vs) = hm_v2_decode(&au) {
                        hm_raw += arr.len() as u64;
                        maps += 1;
                        // census on first-order deltas
                        let mut prev = 0i64;
                        for v in &vs {
                            let d = v - prev;
                            prev = *v;
                            let a = d.abs();
                            d_tot += 1;
                            if d == 0 {
                                d_zero += 1;
                            }
                            if a <= 3 {
                                d_le3 += 1;
                            }
                            if a <= 4 {
                                d_le7 += 1;
                            }
                        }
                        // second-order census (column order)
                        let mut p1 = 0i64;
                        let mut p2 = 0i64;
                        for (i, v) in vs.iter().enumerate() {
                            let d2 = if i == 0 {
                                *v
                            } else if i == 1 {
                                *v - p1
                            } else {
                                *v - 2 * p1 + p2
                            };
                            s2_tot += 1;
                            let a = d2.abs();
                            if d2 == 0 {
                                s2_zero += 1;
                            }
                            if a <= 3 {
                                s2_le3 += 1;
                            }
                            p2 = p1;
                            p1 = *v;
                        }
                    }
                }
            }
        }
        // variant rebuilds
        let mut mk_stream = |name_idx: usize, enc: &dyn Fn(&[u8]) -> Vec<u8>, m: &std::collections::HashMap<String, Value>| {
            let rebuilt = replace_heightmaps(&Value::Compound(m.clone()), enc, false);
            if let Ok(b) = fastnbt::to_bytes(&rebuilt) {
                streams[name_idx].1.extend_from_slice(&b);
            }
        };
        let v2c = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                v2_reencode(&vs)
            } else {
                x.to_vec()
            }
        };
        let v3c = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                v3_second(&vs)
            } else {
                x.to_vec()
            }
        };
        let v4c = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                v4_serp_first(&vs, &p)
            } else {
                x.to_vec()
            }
        };
        let v5c = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                v5_serp_second(&vs, &p)
            } else {
                x.to_vec()
            }
        };
        let v6c = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                v6_nibble(&vs, &p, false)
            } else {
                x.to_vec()
            }
        };
        mk_stream(1, &v3c, &m);
        mk_stream(2, &v4c, &m);
        mk_stream(3, &v5c, &m);
        mk_stream(4, &v6c, &m);
        // F: column-order nibble (same as E but through the same rebuild)
        let v6col = |x: &[u8]| -> Vec<u8> {
            if let Some(vs) = hm_v2_decode(x) {
                let ident: Vec<usize> = (0..256).collect();
                v6_nibble(&vs, &ident, false)
            } else {
                x.to_vec()
            }
        };
        mk_stream(5, &v6col, &m);
        // G: strip heightmaps entirely
        let stripped = replace_heightmaps(&Value::Compound(m.clone()), &|x| x.to_vec(), true);
        if let Ok(b) = fastnbt::to_bytes(&stripped) {
            streams[6].1.extend_from_slice(&b);
        }
    }
    println!("parsed {parsed} tails, {maps} maps, hm raw {hm_raw} B");
    println!(
        "census: d(1st) tot={d_tot} zero={d_zero} |d|<=3={d_le3} |d|<=4={d_le7} | second-order zero={s2_zero} |d2|<=3={s2_le3} of {s2_tot}"
    );
    let base_c = lzma2_raw(&streams[0].1).unwrap().len();
    for (name, data) in &streams {
        let c = lzma2_raw(data).unwrap().len();
        println!("{name:9} raw {:9}  lzma2 {:8}  {:6.2}%", data.len(), c, 100.0 * c as f64 / base_c as f64);
    }
}
