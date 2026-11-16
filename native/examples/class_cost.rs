//! class_cost: real per-class compressed cost of the drained container.
//! The slab frame compresses as ONE LZMA2 stream, so per-class cost cannot
//! be read off the wire (proportional raw-share misleads when classes
//! compress at different ratios). This probe drains the corpus, splits the
//! slab members into per-class streams, then prices each class with the
//! EXACT cold-tier config (LZMA2 preset 6|EXTREME, lc=4 lp=0 pb=0) plus a
//! zstd-19 reference line. Tail lever A/Bs on real member boundaries:
//!   - status quo: tails inside the mixed LZMA2 window (cross-member
//!     matching) = lzma2(full concat) vs lzma2(test half alone);
//!   - dedicated tail stream with a trained zstd dictionary (train 80%,
//!     test 20%, dict bytes charged).
//!
//! Usage: class_cost <dim-dir>

use c_saves::store::{Cfg, Store};
use std::collections::HashMap as Map;
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

fn load_corpus(dim: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let mut corpus = Vec::new();
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
        corpus.extend(load_mca(&f));
    }
    corpus
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

/// The store's cold-tier LZMA2 config verbatim (0xF7 family: preset
/// 6|EXTREME + lc=4 lp=0 pb=0, raw encoder).
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

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: class_cost <dim-dir>"));
    let corpus = load_corpus(&dim);
    let dir = std::env::temp_dir().join(format!(
        "csaves-classcost-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in &corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    st.sync();
    st.seal();
    let mut steps = 0usize;
    loop {
        let did = st.compact_step();
        steps += did as usize;
        if !did || steps > 400 {
            break;
        }
    }
    let disk = st.disk_bytes();
    let members = st.slab_members_by_class();
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);

    println!("drained disk={disk} B, members={}", members.len());

    // ---- per-class honest cost ------------------------------------------
    let mut by_class: Map<String, Vec<Vec<u8>>> = Map::new();
    for (cls, body) in &members {
        by_class.entry(cls.clone()).or_default().push(body.clone());
    }
    // aggregate small glob_* forms into one row for the table
    let mut rows: Vec<(String, Vec<Vec<u8>>)> =
        by_class.iter().map(|(c, v)| (c.clone(), v.clone())).collect();
    let mut glob_minor = Vec::new();
    rows.retain(|(c, v)| {
        if c.starts_with("glob_g") && c != "glob_g0" {
            glob_minor.extend(v.iter().cloned());
            false
        } else {
            true
        }
    });
    rows.push(("glob_g1..".into(), glob_minor));
    rows.sort_by_key(|(c, _)| c.clone());

    let mut total_raw = 0u64;
    let mut total_lz = 0u64;
    println!(
        "{:<12} {:>7} {:>12} {:>10} {:>8} {:>10} {:>7}",
        "class", "n", "raw", "lzma2", "ratio", "zstd19", "enc_s"
    );
    for (cls, bodies) in &rows {
        let raw_n: u64 = bodies.iter().map(|b| b.len() as u64).sum();
        let concat: Vec<u8> = bodies.concat();
        let t = std::time::Instant::now();
        let lz = lzma2_cold(&concat);
        let dur = t.elapsed().as_secs_f64();
        let z = zstd::bulk::compress(&concat, 19).unwrap_or_default();
        total_raw += raw_n;
        total_lz += lz.len() as u64;
        println!(
            "{:<12} {:>7} {:>12} {:>10} {:>8.1} {:>10} {:>7.1}",
            cls,
            bodies.len(),
            raw_n,
            lz.len(),
            raw_n as f64 / lz.len() as f64,
            z.len(),
            dur
        );
    }
    println!(
        "TOTAL raw={total_raw} standalone_lzma2={total_lz} (mixed-frame actual is smaller — cross-member matching)"
    );

    // ---- tail lever A/B --------------------------------------------------
    let tails: Vec<Vec<u8>> = by_class
        .iter()
        .find(|(c, _)| c.as_str() == "tail_nbt")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if tails.len() > 100 {
        tail_experiments(&tails, &by_class);
    }
}

/// Tail lever A/B on real member boundaries.
fn tail_experiments(tails: &[Vec<u8>], by_class: &Map<String, Vec<Vec<u8>>>) {
    let t_raw: usize = tails.iter().map(|b| b.len()).sum();
    println!("--- tail experiments: {n} members, {raw} B ---", n = tails.len(), raw = t_raw);
    let split_at = tails.len() * 4 / 5;
    let (train, test) = tails.split_at(split_at);
    let test_raw: usize = test.iter().map(|b| b.len()).sum();

    // status-quo proxy: LZMA2 sees tails TOGETHER with the whole corpus in
    // one 8 MiB-window stream. Compare lzma2(test concat) alone vs
    // lzma2(glob_g0 concat + test concat) — the difference prices the
    // cross-member benefit the mixed frame currently gives tails.
    let test_concat: Vec<u8> = test.concat();
    let lz_alone = lzma2_cold(&test_concat);
    let mut mixed = by_class
        .iter()
        .find(|(c, _)| c.as_str() == "glob_g0")
        .map(|(_, v)| v.concat())
        .unwrap_or_default();
    let g0_len = mixed.len();
    mixed.extend_from_slice(&test_concat);
    let lz_mixed_all = lzma2_cold(&mixed);
    let lz_mixed_g0_only = lzma2_cold(&mixed[..g0_len]);
    let cross_benefit = lz_alone.len() as i64
        - (lz_mixed_all.len() as i64 - lz_mixed_g0_only.len() as i64);
    println!(
        "lzma2 test-alone={} | mixed-marginal={} | cross-member benefit={:+} B",
        lz_alone.len(),
        lz_mixed_all.len() as i64 - lz_mixed_g0_only.len() as i64,
        cross_benefit
    );

    // dedicated tail stream + trained zstd dict
    let dict = match zstd::dict::from_samples(train, 32 * 1024) {
        Ok(d) => d,
        Err(e) => {
            println!("dict training failed: {e}");
            return;
        }
    };
    let mut comp = Vec::new();
    {
        let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict).expect("cdict");
        for t in test {
            let one = c.compress(t).expect("compress");
            comp.extend(one);
        }
    }
    let plain = {
        let mut c = zstd::bulk::Compressor::new(19).expect("zstd");
        let mut comp = Vec::new();
        for t in test {
            comp.extend(c.compress(t).expect("compress").into_iter());
        }
        comp
    };
    println!(
        "per-member zstd19 plain={} | +dict32K={} (dict={} B) net={}",
        plain.len(),
        comp.len(),
        dict.len(),
        comp.len() + dict.len()
    );

    // per-member LZMA2 for the same test set (dedicated LZMA2 stream, no dict)
    let lz_percat: Vec<u8> = test_concat.clone();
    let _ = lz_percat;
    let mut lz_pm = Vec::new();
    {
        // one encoder per member = no cross-member window (dedicated but
        // dictionary-less); for comparison against the dict line
        let mut acc = 0usize;
        for t in test {
            let one = lzma2_cold(t);
            acc += one.len();
        }
        lz_pm.push(acc);
    }
    println!(
        "per-member lzma2 (no cross window)={} vs concat lzma2={} (window across members)",
        lz_pm[0],
        lz_alone.len()
    );
}
