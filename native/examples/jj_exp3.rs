//! jj_exp3 — post-compression byte attribution per component class +
//! prediction of two codec levers BEFORE touching production code:
//!   L1: wide heightmap delta codec (JJThunder 12-bit maps, currently raw LongArrays)
//!   L2: dominant-mask + exceptions codec for dense block_states data
//!   L3: RLE variant of the same (comparison)
//!   L4: per-map heightmap dedup
//! Model: per-class concatenation compressed with zstd-19 (models 2 MiB slab frames).

use fastnbt::Value;
use std::collections::HashMap as Map;
use std::io::Read;
use std::path::{Path, PathBuf};

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
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
        let payload = match comp_type {
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
            2 => {
                let mut v = Vec::new();
                if flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
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
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

/// Maximal bits b in 1..=16 with floor(64/b) * longs >= n_entries.
fn infer_bits(longs: usize, n: usize) -> Option<u32> {
    (1..=16u32).rev().find(|b| (64 / b) as usize * longs >= n)
}

/// Non-straddle unpack (vanilla block_states/heightmap packing): epl entries
/// per long, LSB-first, padding bits ignored.
fn unpack_wide(longs: &[i64], n: usize, bits: u32) -> Vec<u16> {
    let epl = (64 / bits) as usize;
    let mut out = vec![0u16; n];
    for (i, e) in out.iter_mut().enumerate() {
        let l = longs[i / epl] as u64;
        let sh = (i % epl) as u32 * bits;
        *e = ((l >> sh) & ((1u64 << bits) - 1)) as u16;
    }
    out
}

/// Pack u16 entries non-straddle at `bits` (LSB-first), zero padding.
fn pack_wide(entries: &[u16], bits: u32) -> Vec<i64> {
    let epl = (64 / bits) as usize;
    let mut out = vec![0i64; entries.len().div_ceil(epl)];
    for (i, &e) in entries.iter().enumerate() {
        out[i / epl] |= (e as i64) << ((i % epl) as u32 * bits);
    }
    out
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

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn z19(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    zstd::bulk::compress(data, 19).unwrap().len()
}

/// Canonical serialization of one palette entry (compound {Name, Properties}
/// or plain string) for frequency/canonical ordering + palette streams.
fn palette_entry_bytes(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::String(s) => out.extend_from_slice(s.as_bytes()),
        Value::Compound(m) => {
            if let Some(Value::String(s)) = m.get("Name") {
                out.extend_from_slice(s.as_bytes());
            }
            if let Some(Value::Compound(props)) = m.get("Properties") {
                let mut keys: Vec<&String> = props.keys().collect();
                keys.sort();
                for k in keys {
                    out.push(b';');
                    out.extend_from_slice(k.as_bytes());
                    out.push(b'=');
                    if let Value::String(s) = &props[k] {
                        out.extend_from_slice(s.as_bytes());
                    }
                }
            }
        }
        other => out.extend_from_slice(&fastnbt::to_bytes(other).unwrap_or_default()),
    }
    out
}

struct SecStats {
    uniform: usize,
    dense: usize,
    pal_raw: u64,
    data_raw: u64,
    biomes_raw: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(
        args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"),
    );

    let region_dir = dim.join("region");
    let mut chunks = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for f in &files {
        if f.file_name().unwrap().to_string_lossy().ends_with(".mca") {
            chunks.extend(load_mca(f));
        }
    }
    println!("=== jj_exp3: post-compression attribution + codec predictions ===");
    println!("region chunks loaded: {}", chunks.len());

    // ---- streams ----
    let mut hm_raw: Vec<u8> = Vec::new(); // every heightmap long-array, packed bytes, as stored
    let mut hm_raw_dedup: Vec<u8> = Vec::new(); // per-map dedup
    let mut hm_delta: Vec<u8> = Vec::new(); // wide zigzag-delta varint chain
    let mut hm_delta_t: Vec<u8> = Vec::new(); // transposed 16x16 scan order
    let mut hm_seen: Map<u64, ()> = Map::new();
    let mut hm_maps = 0usize;
    let mut hm_unencod = 0usize; // not roundtrip-able (foreign packing)

    let mut cur_pal: Vec<u8> = Vec::new(); // canonical palette streams (dedup'd sections)
    let mut cur_data: Vec<u8> = Vec::new(); // ceillog2-packed remapped data (CSEC-narrow model)
    let mut mask_bm: Vec<u8> = Vec::new(); // dominant==0 bitmaps
    let mut mask_exc: Vec<u8> = Vec::new(); // exception streams
    let mut rle: Vec<u8> = Vec::new(); // run-length streams
    let mut plane_uni: Vec<u8> = Vec::new(); // plane model: headers + uniform values
    let mut plane_bm: Vec<u8> = Vec::new(); // plane model: per-mixed-plane bitmaps
    let mut plane_exc: Vec<u8> = Vec::new(); // plane model: exceptions
    let mut plane_dens: Vec<u8> = Vec::new(); // plane model: dense-fallback planes
    let mut biomes_stream: Vec<u8> = Vec::new();
    // per-section dense data bytes (for dictionary training experiment)
    let mut sec_data_samples: Vec<Vec<u8>> = Vec::new();
    let mut st = SecStats { uniform: 0, dense: 0, pal_raw: 0, data_raw: 0, biomes_raw: 0 };
    // conditional-entropy measurement (order-1 vertical / horizontal predictors)
    let mut vox_total = 0u64;
    let mut above_total = 0u64;
    let mut left_total = 0u64;
    let mut same_above = 0u64;
    let mut same_left = 0u64;
    // symbol histograms: unconditional, cond-on-above-equal, cond-on-above-diff, cond-on-left-eq/diff
    let mut h_un = vec![0u64; 4096];
    let mut h_aeq = vec![0u64; 4096];
    let mut h_adf = vec![0u64; 4096];
    let mut h_leq = vec![0u64; 4096];
    let mut h_ldf = vec![0u64; 4096];
    let mut sec_seen: Map<u64, ()> = Map::new();
    let mut sec_total = 0usize;
    let mut sec_unique = 0usize;
    // shift-match experiment: per chunk, dense sections mapped to chunk-global identity space
    let mut chunk_dense: Vec<Vec<Vec<u32>>> = Vec::new();
    let mut shift_hits = 0usize;
    let mut shift_checked = 0usize;
    let mut shift_saved_bytes = 0u64; // projected: matched section ~260 B compressed each

    for (_, _, payload) in &chunks {
        let Ok(root) = fastnbt::from_bytes::<Value>(payload) else { continue };
        let Value::Compound(map) = root else { continue };
        chunk_dense.push(Vec::new());
        let mut chunk_names: Map<Vec<u8>, u32> = Map::new();
        // --- sections ---
        if let Some(Value::List(list)) = map.get("sections") {
            for s in list {
                sec_total += 1;
                let Value::Compound(sm) = s else { continue };
                let ser = fastnbt::to_bytes(s).unwrap_or_default();
                let h = fxhash(&ser);
                if sec_seen.insert(h, ()).is_some() {
                    continue;
                }
                sec_unique += 1;
                let Some(Value::Compound(bs)) = sm.get("block_states") else { continue };
                let (Some(Value::List(pal)), Some(Value::LongArray(la))) =
                    (bs.get("palette"), bs.get("data"))
                else {
                    // palette-only uniform section: cost ~ palette serialization
                    st.uniform += 1;
                    if let Some(Value::List(pal)) = bs.get("palette") {
                        for p in pal {
                            cur_pal.extend_from_slice(&palette_entry_bytes(p));
                        }
                    }
                    biomes_stream.extend(sm.get("biomes").map(|b| fastnbt::to_bytes(b).unwrap_or_default()).unwrap_or_default());
                    continue;
                };
                let n = pal.len();
                if n == 0 || la.is_empty() {
                    st.uniform += 1;
                    continue;
                }
                let Some(bits) = infer_bits(la.len(), 4096) else { continue };
                let entries = unpack_wide(la, 4096, bits);
                // guard: repack must equal source (padding zero) else skip section
                if pack_wide(&entries, bits) != la.to_vec() {
                    hm_unencod += 1;
                    continue;
                }
                st.dense += 1;
                st.pal_raw += pal.iter().map(|p| palette_entry_bytes(p).len() as u64).sum::<u64>();
                st.data_raw += (la.len() * 8) as u64;

                // canonical frequency order
                let mut freq = vec![0u32; n];
                for &e in &entries {
                    freq[e as usize] += 1;
                }
                let mut order: Vec<u16> = (0..n as u16).collect();
                order.sort_by_key(|&i| (std::cmp::Reverse(freq[i as usize]), i));
                let mut remap = vec![0u16; n];
                for (new, &old) in order.iter().enumerate() {
                    remap[old as usize] = new as u16;
                }
                let remapped: Vec<u16> = entries.iter().map(|&e| remap[e as usize]).collect();
                // map to chunk-global identity space (palette-order independent)
                let mut ident = Vec::with_capacity(4096);
                let mut cmap: Vec<u32> = Vec::with_capacity(n);
                for p in pal {
                    let kb = palette_entry_bytes(p);
                    let nid = chunk_names.len() as u32;
                    let id = *chunk_names.entry(kb).or_insert(nid);
                    cmap.push(id);
                }
                for &e in &entries {
                    ident.push(cmap[e as usize]);
                }
                chunk_dense.last_mut().unwrap().push(ident);

                // conditional-entropy accounting (Minecraft layout: index = (y*16+z)*16+x, y-major)
                for y in 0..16usize {
                    for z in 0..16usize {
                        for x in 0..16usize {
                            let i = (y * 16 + z) * 16 + x;
                            let s = remapped[i] as usize;
                            vox_total += 1;
                            h_un[s] += 1;
                            if y > 0 {
                                let a = remapped[((y - 1) * 16 + z) * 16 + x] as usize;
                                above_total += 1;
                                if a == s {
                                    same_above += 1;
                                    h_aeq[s] += 1;
                                } else {
                                    h_adf[s] += 1;
                                }
                            }
                            if x > 0 {
                                let l = remapped[(y * 16 + z) * 16 + x - 1] as usize;
                                left_total += 1;
                                if l == s {
                                    same_left += 1;
                                    h_leq[s] += 1;
                                } else {
                                    h_ldf[s] += 1;
                                }
                            }
                        }
                    }
                }

                // palette stream (canonical order)
                for &oi in &order {
                    cur_pal.extend_from_slice(&palette_entry_bytes(&pal[oi as usize]));
                }

                // variant A: ceillog2 narrow packing (current CSEC model)
                let bits2 = (0..16u32).find(|b| n <= (1usize << *b)).unwrap_or(16).max(1);
                let packed = pack_wide(&remapped, bits2).iter()
                    .flat_map(|l| l.to_le_bytes()).collect::<Vec<u8>>();
                cur_data.extend_from_slice(&packed);
                sec_data_samples.push(packed);

                // variant B: dominant mask + exceptions
                let mut bm = vec![0u8; 512];
                let mut exc: Vec<u8> = Vec::new();
                let mut prev_i = 0usize;
                for (i, &e) in remapped.iter().enumerate() {
                    if e == 0 {
                        bm[i >> 3] |= 1 << (i & 7);
                    } else {
                        push_varint(&mut exc, zigzag((i - prev_i) as i64));
                        push_varint(&mut exc, e as u64);
                        prev_i = i;
                    }
                }
                mask_bm.extend_from_slice(&bm);
                mask_exc.extend_from_slice(&exc);

                // variant C: RLE
                let mut rl: Vec<u8> = Vec::new();
                let mut i = 0usize;
                while i < 4096 {
                    let v = remapped[i];
                    let mut run = 1usize;
                    while i + run < 4096 && remapped[i + run] == v {
                        run += 1;
                    }
                    push_varint(&mut rl, (run - 1) as u64);
                    push_varint(&mut rl, v as u64);
                    i += run;
                }
                rle.extend_from_slice(&rl);

                // variant D: y-plane model (16 planes of 256, y-major layout)
                // plane kinds: 0=uniform, 1=mask+exceptions, 2=dense fallback
                for py in 0..16usize {
                    let plane = &remapped[py * 256..(py + 1) * 256];
                    let mut distinct = [false; 4096];
                    let mut nd = 0usize;
                    for &e in plane {
                        if !distinct[e as usize] {
                            distinct[e as usize] = true;
                            nd += 1;
                        }
                    }
                    if nd == 1 {
                        plane_uni.push(0);
                        push_varint(&mut plane_uni, plane[0] as u64);
                        continue;
                    }
                    // plane-local mask: dominant within plane
                    let mut pfreq = [0u32; 4096];
                    for &e in plane {
                        pfreq[e as usize] += 1;
                    }
                    let dom = (0..n as usize).max_by_key(|&i| (pfreq[i], std::cmp::Reverse(i))).unwrap_or(0) as u16;
                    let mut bm = vec![0u8; 32];
                    let mut exc: Vec<u8> = Vec::new();
                    let mut prev_i = 0usize;
                    for (i, &e) in plane.iter().enumerate() {
                        if e == dom {
                            bm[i >> 3] |= 1 << (i & 7);
                        } else {
                            push_varint(&mut exc, zigzag((i - prev_i) as i64));
                            push_varint(&mut exc, e as u64);
                            prev_i = i;
                        }
                    }
                    let dense_plane = pack_wide(plane, bits2).iter()
                        .flat_map(|l| l.to_le_bytes()).collect::<Vec<u8>>();
                    let mask_cost = 1 + 32 + exc.len();
                    if mask_cost < dense_plane.len() {
                        plane_uni.push(1);
                        push_varint(&mut plane_uni, dom as u64);
                        plane_bm.extend_from_slice(&bm);
                        plane_exc.extend_from_slice(&exc);
                    } else {
                        plane_uni.push(2);
                        plane_dens.extend_from_slice(&dense_plane);
                    }
                }

                // biomes
                biomes_stream.extend(sm.get("biomes")
                    .map(|b| fastnbt::to_bytes(b).unwrap_or_default()).unwrap_or_default());
                st.biomes_raw += sm.get("biomes").map(|b| fastnbt::to_bytes(b).unwrap_or_default().len() as u64).unwrap_or(0);
            }
        }
        // --- heightmaps ---
        if let Some(Value::Compound(hms)) = map.get("Heightmaps") {
            for (_k, hv) in hms {
                let Value::LongArray(la) = hv else { continue };
                hm_maps += 1;
                let lb = la.iter().flat_map(|l| l.to_be_bytes()).collect::<Vec<u8>>();
                let h = fxhash(&lb);
                let first = hm_seen.insert(h, ()).is_none();
                if first {
                    hm_raw_dedup.extend_from_slice(&lb);
                }
                hm_raw.extend_from_slice(&lb);
                // wide delta chain, storage order and transposed order
                let Some(bits) = infer_bits(la.len(), 256) else {
                    hm_unencod += 1;
                    continue;
                };
                let entries = unpack_wide(la, 256, bits);
                if pack_wide(&entries, bits) != la.to_vec() {
                    hm_unencod += 1;
                    continue;
                }
                for transposed in [false, true] {
                    let mut out: Vec<u8> = Vec::with_capacity(300);
                    let mut prev: i64 = 0;
                    for i in 0..256 {
                        let j = if transposed {
                            let x = i & 15;
                            let z = i >> 4;
                            (x << 4) | z
                        } else {
                            i
                        };
                        let e = entries[j] as i64;
                        push_varint(&mut out, zigzag(e - prev));
                        prev = e;
                    }
                    if transposed {
                        hm_delta_t.extend_from_slice(&out);
                    } else {
                        hm_delta.extend_from_slice(&out);
                    }
                }
            }
        }
    }

    println!("\n-- section stats --");
    println!("total {sec_total}, unique {sec_unique}, uniform {}, dense {}, foreign-pack skipped {hm_unencod}", st.uniform, st.dense);
    println!("palette raw {} B, data raw {} B, biomes raw {} B", st.pal_raw, st.data_raw, st.biomes_raw);
    println!("heightmap maps: {hm_maps}, unique {}", hm_seen.len());

    // ---- shift-match experiment ----
    // Direction-consistent version: sections arrive y-ascending; only allow
    // s = upper window of t (t extends BELOW s, t earlier in list) — decoder
    // resolves t before s, no forward dependencies.
    for secs in &chunk_dense {
        if secs.len() < 2 {
            continue;
        }
        for (si, s) in secs.iter().enumerate() {
            shift_checked += 1;
            let mut matched = false;
            'outer: for (ti, t) in secs.iter().enumerate() {
                if ti >= si {
                    continue; // only earlier (lower-y) targets
                }
                for k in 1..=16usize {
                    // s = t shifted up k planes: s[0..N-kw] == t[kw..]
                    if s[..4096 - k * 256] == t[k * 256..] {
                        matched = true;
                        break 'outer;
                    }
                }
            }
            if matched {
                shift_hits += 1;
                shift_saved_bytes += 260;
            }
        }
    }
    println!("\n-- shift-match experiment --");
    println!("dense sections checked {}, matched {} ({:.1}%), projected saving {} B",
        shift_checked, shift_hits,
        if shift_checked == 0 { 0.0 } else { 100.0 * shift_hits as f64 / shift_checked as f64 },
        shift_saved_bytes);

    println!("\n-- zstd-19 per stream --");
    let z = |name: &str, d: &[u8]| {
        let c = z19(d);
        println!("{name:24} raw {:>10}  z19 {:>9}  ({:.2})", d.len(), c, if d.is_empty() { 0.0 } else { d.len() as f64 / c as f64 });
        c
    };
    let z_pal = z("palette (canonical)", &cur_pal);
    let z_data = z("data ceillog2 (cur)", &cur_data);
    let z_bm = z("mask bitmap", &mask_bm);
    let z_exc = z("mask exceptions", &mask_exc);
    let z_rle = z("rle runs", &rle);
    let z_pu = z("plane headers+uni", &plane_uni);
    let z_pb = z("plane bitmaps", &plane_bm);
    let z_pe = z("plane exceptions", &plane_exc);
    let z_pd = z("plane dense-fallback", &plane_dens);
    let z_bio = z("biomes", &biomes_stream);
    let z_hm_raw = z("hmaps raw (all)", &hm_raw);
    let z_hm_dd = z("hmaps raw (dedup)", &hm_raw_dedup);
    let z_hm_d = z("hmaps delta", &hm_delta);
    let z_hm_dt = z("hmaps delta transp", &hm_delta_t);

    // ---- dictionary experiment: large trained dict on dense-section bytes ----
    println!("\n-- dict experiment (dense data samples: {}) --", sec_data_samples.len());
    if sec_data_samples.len() > 200 {
        let mut idx: Vec<usize> = (0..sec_data_samples.len()).collect();
        idx.sort_by_key(|&i| sec_data_samples[i].len()); // deterministic
        let cut = idx.len() * 70 / 100;
        let train_sel = &idx[..cut];
        let test_sel = &idx[cut..];
        let train: Vec<&[u8]> = train_sel.iter().map(|&i| sec_data_samples[i].as_slice()).collect();
        let test_raw: u64 = test_sel.iter().map(|&i| sec_data_samples[i].len() as u64).sum();
        println!("train {} samples ({} B), test {} samples ({} B)",
            train.len(), train.iter().map(|s| s.len()).sum::<usize>(), test_sel.len(), test_raw);
        // baseline: no dict, each test section its own frame (models per-slab-frame granularity)
        let base: u64 = test_sel.iter().map(|&i| z19(&sec_data_samples[i])).sum::<usize>() as u64;
        println!("no-dict per-section frames: {}", base);
        // also: concatenated test stream (models big-frame matching among test sections)
        let mut test_all = Vec::new();
        for &i in test_sel {
            test_all.extend_from_slice(&sec_data_samples[i]);
        }
        println!("no-dict concat stream: {}", z19(&test_all));
        for dsize in [32768usize, 112640, 262144] {
            if let Ok(dict) = zstd::dict::from_samples(&train, dsize) {
                let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict).unwrap();
                let with: u64 = test_sel.iter()
                    .map(|&i| c.compress(&sec_data_samples[i]).unwrap().len())
                    .sum::<usize>() as u64;
                let with_concat = c.compress(&test_all).unwrap().len();
                println!("dict {:>7}: per-section {} (x{:.2}), concat {} (x{:.2} vs no-dict concat)",
                    dsize, with, base as f64 / with as f64, with_concat,
                    z19(&test_all) as f64 / with_concat as f64);
            }
        }
    }

    println!("\n-- conditional entropy (order-1 context, bits/voxel) --");
    fn hist_entropy(h: &[u64], total: u64) -> f64 {
        if total == 0 {
            return 0.0;
        }
        let mut e = 0.0f64;
        for &c in h {
            if c > 0 {
                let p = c as f64 / total as f64;
                e -= p * p.log2();
            }
        }
        e
    }
    let pa_eq = same_above as f64 / above_total.max(1) as f64;
    let pa_df = 1.0 - pa_eq;
    let pl_eq = same_left as f64 / left_total.max(1) as f64;
    let pl_df = 1.0 - pl_eq;
    let h0 = hist_entropy(&h_un, vox_total);
    let ha = pa_eq * hist_entropy(&h_aeq, same_above)
        + pa_df * hist_entropy(&h_adf, above_total - same_above);
    let hl = pl_eq * hist_entropy(&h_leq, same_left)
        + pl_df * hist_entropy(&h_ldf, left_total - same_left);
    println!("voxels {vox_total}, P(same|above) {pa_eq:.4}, P(same|left) {pl_eq:.4}");
    println!("H0 unconditional              {h0:.3} bits/voxel");
    println!("H|above {ha:.3} | left {hl:.3} bits/voxel");
    let best = ha.min(hl);
    println!("projected data size at H1: {} B  (z19 today: see 'data ceillog2 (cur)' above; raw-narrow {} B)",
        (vox_total as f64 * best / 8.0) as u64, cur_data.len());

    println!("\n-- models --");
    let cur_model = z_pal + z_data + z_bio;
    let mask_model = z_pal + z_bm + z_exc + z_bio;
    let rle_model = z_pal + z_rle + z_bio;
    let plane_model = z_pal + z_pu + z_pb + z_pe + z_pd + z_bio;
    println!("current model (pal+data+biomes):      {:>9}", cur_model);
    println!("mask model    (pal+bm+exc+biomes):    {:>9}  (x{:.2} vs cur)", mask_model, cur_model as f64 / mask_model as f64);
    println!("rle model     (pal+rle+biomes):       {:>9}  (x{:.2} vs cur)", rle_model, cur_model as f64 / rle_model as f64);
    println!("plane model   (pal+planes+biomes):    {:>9}  (x{:.2} vs cur)", plane_model, cur_model as f64 / plane_model as f64);
    // zstd-22 check on the winning data transform
    if !plane_uni.is_empty() {
        let mut all = plane_uni.clone();
        all.extend_from_slice(&plane_bm);
        all.extend_from_slice(&plane_exc);
        all.extend_from_slice(&plane_dens);
        let mut joined = cur_data.clone();
        let z22 = {
            let mut c = zstd::bulk::Compressor::new(19).unwrap();
            let _ = &mut c;
            zstd::bulk::compress(&joined, 19).unwrap().len()
        };
        let _ = &mut joined;
        println!("data@19 vs data@22 sanity: {} vs {}", z19(&cur_data), z22);
    }
    println!("hmaps: raw {} | dedup {} | delta {} | delta+dedup-combined {}",
        z_hm_raw, z_hm_dd, z_hm_d, z19(&hm_delta) + 0);
    println!("heightmap lever: {} -> {} (dedup+delta best: {})", z_hm_raw, z_hm_dd.min(z_hm_d), z_hm_dd.min(z_hm_d).min(z_hm_dt));
}

fn fxhash(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
