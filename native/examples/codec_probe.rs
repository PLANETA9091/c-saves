//! codec_probe: family sweep on the biggest class streams — is LZMA2
//! 6|e lc=4/lp=0/pb=0 really the floor, or does another CODEC FAMILY beat
//! it? The preset ladder was probed flat (1928fdd) but only INSIDE LZMA2;
//! this sweep crosses families: LZMA2 lc variants, brotli 11 (quality 9/10/11),
//! zstd 22 with long-distance matching, flate2 zlib-ng -9.
//!
//! Representation-level transforms (delta/DAG/columnar) are probed
//! NEGATIVE (dag_probe +51.7%, gid_dag_probe +51.7%, columnar parked
//! 0.9.105) — the adaptive encoder beats explicit structure. The only
//! honest remaining lever is the codec family itself.
//!
//! Usage: codec_probe <store-dir>

use c_saves::store::Store;
use std::io::Write;
use std::path::PathBuf;

fn lzma2_cfg(data: &[u8], lc: u32, lp: u32, pb: u32, dict: u32) -> Option<Vec<u8>> {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(
        liblzma::stream::PRESET_EXTREME | 6,
    )
    .ok()?;
    opts.literal_context_bits(lc)
        .literal_position_bits(lp)
        .position_bits(pb)
        .dict_size(dict);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: codec_probe <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let mut streams: std::collections::BTreeMap<String, Vec<u8>> = Default::default();
    for (cls, body) in members {
        // per-class streams concatenated in WALK order (shipped adjacency)
        streams.entry(cls).or_default().extend(body);
    }
    for (cls, data) in streams {
        let n = data.len();
        if n < 100_000 {
            continue;
        }
        println!("-- {cls}: {n} B raw --");
        let tuned = lzma2_cfg(&data, 4, 0, 0, 1 << 23).unwrap();
        println!("  lzma2 6e lc4/lp0/pb0 d8M  : {:>9}  (shipped)", tuned.len());
        for (name, lc, lp, pb, dict) in [
            ("lc0/lp0/pb0 d8M", 0u32, 0u32, 0u32, 8u32 << 20),
            ("lc1/lp0/pb0 d8M", 1, 0, 0, 8 << 20),
            ("lc4/lp0/pb2 d8M", 4, 0, 2, 8 << 20),
            ("lc4/lp0/pb0 d64M", 4, 0, 0, 64 << 20),
            ("lc2/lp1/pb0 d8M", 2, 1, 0, 8 << 20),
        ] {
            let c = lzma2_cfg(&data, lc, lp, pb, dict).unwrap();
            println!("  lzma2 6e {:<16}: {:>9}  ({:+.2}%)", name, c.len(), (c.len() as f64 / tuned.len() as f64 - 1.0) * 100.0);
        }
        for q in [9u32, 10, 11] {
            let mut params = brotli::enc::BrotliEncoderParams::default();
            params.quality = q as i32;
            params.lgwin = 24i32;
            let mut out = Vec::new();
            let mut w = brotli::CompressorWriter::with_params(Vec::new(), 4096, &params);
            w.write_all(&data).unwrap();
            out = w.into_inner();
            println!(
                "  brotli q{q:<2} lgwin24      : {:>9}  ({:+.2}%)",
                out.len(),
                (out.len() as f64 / tuned.len() as f64 - 1.0) * 100.0
            );
        }
        // zstd 22 + long
        if let Ok(c) = zstd::bulk::compress(&data, 22) {
            println!(
                "  zstd-22                     : {:>9}  ({:+.2}%)",
                c.len(),
                (c.len() as f64 / tuned.len() as f64 - 1.0) * 100.0
            );
        }
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
        e.write_all(&data).unwrap();
        let c = e.finish().unwrap();
        println!(
            "  zlib-ng -9                  : {:>9}  ({:+.2}%)",
            c.len(),
            (c.len() as f64 / tuned.len() as f64 - 1.0) * 100.0
        );
    }
    println!("CODEC-PROBE done");
}
