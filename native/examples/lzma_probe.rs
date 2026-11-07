//! Research probe: raw LZMA2 (liblzma, static vendored) on the real corpus
//! payload. Verifies the API roundtrip and measures size/wall vs the python
//! liblzma numbers (docs/compression-research.md: 1,185,140 bytes, 3.79 s).

use liblzma::stream::{Action, LzmaOptions, Stream};
use liblzma::stream::Filters;
use std::time::Instant;

fn lzma2_filters(preset: u32) -> Filters {
    let opts = LzmaOptions::new_preset(preset).expect("lzma preset");
    let mut f = Filters::new();
    f.lzma2(&opts);
    f
}

fn lzma2_compress(data: &[u8], preset: u32) -> Result<Vec<u8>, String> {
    let mut stream = Stream::new_raw_encoder(&lzma2_filters(preset)).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(data.len() / 4 + 64);
    stream
        .process_vec(data, &mut out, Action::Run)
        .map_err(|e| e.to_string())?;
    stream
        .process_vec(&[], &mut out, Action::Finish)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn lzma2_decompress(data: &[u8], raw_len: usize, preset: u32) -> Result<Vec<u8>, String> {
    let mut stream = Stream::new_raw_decoder(&lzma2_filters(preset)).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(raw_len);
    stream
        .process_vec(data, &mut out, Action::Run)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn main() {
    let data = std::fs::read("/tmp/corpus_payload.bin").expect("payload dump");
    println!("payload: {} bytes", data.len());
    let block = 4 * 1024 * 1024;
    for preset in [3u32, 6u32] {
        let t0 = Instant::now();
        let mut total = 0usize;
        let mut roundtrip_ok = true;
        for chunk in data.chunks(block) {
            let comp = lzma2_compress(chunk, preset).expect("compress");
            let back = lzma2_decompress(&comp, chunk.len(), preset).expect("decompress");
            if back != chunk {
                roundtrip_ok = false;
            }
            total += comp.len();
        }
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "lzma2 preset {preset}: {total} bytes ({:.4} of raw) in {dt:.2}s ({:.1} MB/s) roundtrip={roundtrip_ok}",
            total as f64 / data.len() as f64,
            data.len() as f64 / dt / 1e6
        );
    }
}
