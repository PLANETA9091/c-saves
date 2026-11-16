//! g0_transpose: does a plain y-fast transposed u8 gid stream (no RLE)
//! compress better under LZMA2 than the vanilla-order raw form? The
//! current gform table has RLE+transposed (g2) but no plain transposed
//! form, so wide-palette/short-run sections never got the transpose.
//! Measures the honest window-level verdict: concat(raw members) vs
//! concat(transposed payloads) under the cold LZMA2 config, plus a
//! per-member zstd winner census for the admission-rule design.
//!
//! Usage: g0_transpose <dim-dir>

use c_saves::store::{Cfg, Store};
use std::io::Read;
use std::path::{Path, PathBuf};

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
        let ct = data[off + 4];
        let comp = data[off + 5..off + 4 + len].to_vec();
        let payload = match ct {
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
        out.push((rx * 32 + (i as i32 & 31), rz * 32 + (i as i32 >> 5), payload));
    }
    out
}

fn load_corpus(dim: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let mut c = Vec::new();
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
        c.extend(load_mca(&f));
    }
    c
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

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

/// vanilla (x,y,z) -> y-fast (y,x,z) reorder, side=16, u8 streams (the
/// involution: applying twice returns the input).
fn transpose_fast_y(v: &[u8], side: usize) -> Vec<u8> {
    let s2 = side * side;
    let mut out = vec![0u8; v.len()];
    for y in 0..side {
        for z in 0..side {
            for x in 0..side {
                out[x * s2 + z * side + y] = v[y * s2 + z * side + x];
            }
        }
    }
    out
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: g0_transpose <dim-dir>"));
    let corpus = load_corpus(&dim);
    let dir = std::env::temp_dir().join("csaves-g0trans");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in &corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    st.sync();
    st.seal();
    let mut steps = 0;
    loop {
        let did = st.compact_step();
        steps += did as usize;
        if !did || steps > 400 {
            break;
        }
    }
    let members = st.slab_members_by_class();
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);

    // g0 members: [0xCB][f1][f2c][f3][y][4096 gid u8][csec tail]
    let mut raw_all = Vec::new();
    let mut tr_all = Vec::new();
    let mut mixed_struct = Vec::new();
    let mut st_wins = 0usize;
    let mut z_raw_wins = 0usize;
    let mut z_tr_wins = 0usize;
    let mut skipped = 0usize;
    let mut z_raw_sum = 0usize;
    let mut z_tr_sum = 0usize;
    for (cls, b) in &members {
        if cls != "glob_g0" || b.len() < 5 + 4096 {
            continue;
        }
        // payload extent check: the csec tail (biomes/light) follows the
        // 4096-byte stream; only pure streams are measurable here
        let payload = &b[5..5 + 4096];
        let tail = &b[5 + 4096..];
        let tr_payload = transpose_fast_y(payload, 16);
        raw_all.extend_from_slice(b);
        let mut tb = Vec::with_capacity(b.len());
        tb.extend_from_slice(&b[..5]);
        tb.extend_from_slice(&tr_payload);
        tb.extend_from_slice(tail);
        tr_all.extend_from_slice(&tb);
        // per-member zstd census (admission-rule signal, not the verdict)
        let zr = zstd::bulk::compress(b, 19).unwrap_or_else(|_| vec![0u8; b.len()]);
        let mut tb2 = tb.clone();
        let zt = zstd::bulk::compress(&tb2, 19).unwrap_or_else(|_| vec![0u8; b.len()]);
        z_raw_sum += zr.len();
        z_tr_sum += zt.len();
        if zt.len() < zr.len() {
            z_tr_wins += 1;
        } else {
            z_raw_wins += 1;
        }
        // structural pick: transposed transitions vs vanilla transitions
        // (the RLE walk counts runs = transitions + 1)
        let vanilla_runs = rle_len(&b[5..5 + 4096]);
        let transposed_runs = rle_len(&tr_payload);
        let struct_pick = if transposed_runs < vanilla_runs { &tb } else { b };
        mixed_struct.extend_from_slice(struct_pick);
        if transposed_runs < vanilla_runs {
            st_wins += 1;
        }
        skipped += 0;
    }
    println!(
        "g0 members measured: n={} z_raw_sum={} z_tr_sum={} | zstd winners: raw={} transposed={}",
        z_raw_wins + z_tr_wins,
        z_raw_sum,
        z_tr_sum,
        z_raw_wins,
        z_tr_wins
    );
    println!(
        "structural pick: transposed-wins={} of {} (run-count proxy)",
        st_wins,
        z_raw_wins + z_tr_wins
    );
    println!("concat raw={} transposed={} structural={}", raw_all.len(), tr_all.len(), mixed_struct.len());
    let t = std::time::Instant::now();
    let lz_raw = lzma2_cold(&raw_all);
    let lz_tr = lzma2_cold(&tr_all);
    let lz_st = lzma2_cold(&mixed_struct);
    println!(
        "LZMA2 raw={} transposed={} structural={} | T-delta={:+} B ({:+.2}%) ST-delta={:+} B ({:+.2}%) enc={:.1}s",
        lz_raw.len(),
        lz_tr.len(),
        lz_st.len(),
        lz_tr.len() as i64 - lz_raw.len() as i64,
        (lz_tr.len() as f64 - lz_raw.len() as f64) / lz_raw.len() as f64 * 100.0,
        lz_st.len() as i64 - lz_raw.len() as i64,
        (lz_st.len() as f64 - lz_raw.len() as f64) / lz_raw.len() as f64 * 100.0,
        t.elapsed().as_secs_f64()
    );
    let _ = skipped;
}

/// run count of a u8 stream (runs = transitions + 1; cheap proxy for
/// LZMA2 friendliness of the permutation)
fn rle_len(v: &[u8]) -> usize {
    let mut runs = 1usize;
    let mut i = 1usize;
    while i < v.len() {
        if v[i] != v[i - 1] {
            runs += 1;
        }
        i += 1;
    }
    runs
}
