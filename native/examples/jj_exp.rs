//! jj_exp: offline slab-grouping experiments on the real JJThunder corpus.
//!
//! Measures how slab MEMBER ORDERING affects final compressed size:
//!   A: global shuffle (what the engine does today: blobs map iteration order)
//!   B: chunk-local groups (sections of one chunk adjacent, Y-ordered)
//!   C: B + tail inline after its chunk's sections
//! Also: zstd level sweep (19/22) and long-distance matching on group C.
//!
//! Usage: cargo run --release --example jj_exp -- <dim-dir>

use c_saves::chunknbt;
use c_saves::store::Store;
use std::collections::HashMap as Map;
use std::io::Read;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn load_mca(path: &std::path::Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    let (rx, rz): (i32, i32) = (parts[1].parse().unwrap(), parts[2].parse().unwrap());
    let data = std::fs::read(path).unwrap();
    let mut out = vec![];
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
        if loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        let ct = data[off + 4];
        let mut payload = Vec::new();
        match ct {
            2 => {
                flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut payload)
                    .ok()
                    .unwrap_or(0);
            }
            1 => {
                flate2::read::GzDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut payload)
                    .ok()
                    .unwrap_or(0);
            }
            _ => continue,
        }
        out.push((rx * 32 + (i as i32 & 31), rz * 32 + (i as i32 >> 5), payload));
    }
    out
}

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

/// cut an ordered stream of byte-chunks into slab-sized groups and compress
fn compress_grouped(stream: &[(u64, Vec<u8>)], slab_raw: usize, level: i32, ldm: bool) -> u64 {
    let mut total = 0u64;
    let mut cur: Vec<u8> = Vec::with_capacity(slab_raw);
    fn flush(cur: &mut Vec<u8>, total: &mut u64, slab_raw: usize, level: i32, ldm: bool) {
        if cur.is_empty() {
            return;
        }
        let comp = if ldm {
            let mut c = zstd::bulk::Compressor::new(level).unwrap();
            c.set_parameter(zstd_safe::CParameter::WindowLog(27)).unwrap();
            c.compress(cur).unwrap()
        } else {
            zstd::bulk::compress(cur, level).unwrap()
        };
        *total += 8 + 12 + comp.len() as u64;
        cur.clear();
    }
    for (_, bytes) in stream {
        if !cur.is_empty() && cur.len() + bytes.len() > slab_raw {
            flush(&mut cur, &mut total, slab_raw, level, ldm);
        }
        cur.extend_from_slice(bytes);
    }
    flush(&mut cur, &mut total, slab_raw, level, ldm);
    total
}

fn main() {
    let dim = std::env::args().nth(1).unwrap_or_else(|| "/home/z/jj/server/world/dimensions/minecraft/overworld".into());
    let dir = std::path::Path::new(&dim);
    let mut chunks = vec![];
    for f in std::fs::read_dir(dir.join("region")).unwrap().flatten() {
        let p = f.path();
        if p.to_string_lossy().ends_with(".mca") {
            chunks.extend(load_mca(&p));
        }
    }
    chunks.sort_by_key(|(cx, cz, _)| (*cx, *cz));
    println!("chunks: {}", chunks.len());

    // split every chunk through the real codec; collect (chunk_key, order, bytes)
    let mut per_chunk: Vec<(u64, Vec<Vec<u8>>, Vec<u8>)> = Vec::with_capacity(chunks.len());
    let mut seen: Map<Vec<u8>, ()> = Map::new();
    let mut raw_total = 0u64;
    for (cx, cz, p) in &chunks {
        let split = chunknbt::split(p, false, Some((*cx, *cz))).unwrap();
        if let chunknbt::Split::Split { sections, tail } = split {
            let mut bytes = Vec::with_capacity(sections.len());
            for (_, b) in sections {
                let b = b.as_slice().to_vec();
                if seen.insert(b.clone(), ()).is_none() {
                    raw_total += b.len() as u64;
                }
                bytes.push(b);
            }
            per_chunk.push((key(*cx, *cz), bytes, tail));
        }
    }
    println!("unique sections: {} raw {:.2} MiB", seen.len(), mib(raw_total));

    let slab = 192 * 1024;
    // A: global shuffle (deterministic xorshift)
    let mut flat: Vec<(u64, Vec<u8>)> = per_chunk
        .iter()
        .flat_map(|(k, secs, _)| secs.iter().map(move |b| (*k, b.clone())))
        .collect();
    let mut st = 0x9E3779B97F4A7C15u64;
    for i in (1..flat.len()).rev() {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        let j = (st as usize) % (i + 1);
        flat.swap(i, j);
    }
    let a19 = compress_grouped(&flat, slab, 19, false);

    // B: chunk-local order (sections already Y-ordered per chunk)
    let b_stream: Vec<(u64, Vec<u8>)> = per_chunk
        .iter()
        .flat_map(|(k, secs, _)| secs.iter().map(move |b| (*k, b.clone())))
        .collect();
    let b19 = compress_grouped(&b_stream, slab, 19, false);
    let b22 = compress_grouped(&b_stream, slab, 22, false);
    let b19ldm = compress_grouped(&b_stream, slab, 19, true);

    // C: B + tail after its chunk sections
    let mut c_stream: Vec<(u64, Vec<u8>)> = Vec::new();
    for (k, secs, tail) in &per_chunk {
        for b in secs {
            c_stream.push((*k, b.clone()));
        }
        c_stream.push((*k, tail.clone()));
    }
    let c19 = compress_grouped(&c_stream, slab, 19, false);

    // D: chunk-grouped with 64KB slabs (finer locality)
    let d19 = compress_grouped(&b_stream, 64 * 1024, 19, false);
    // E: whole-world single group (upper bound reference)
    let e19 = {
        let all: Vec<u8> = b_stream.iter().flat_map(|(_, b)| b.iter().copied()).collect();
        (8 + 12 + zstd::bulk::compress(&all, 19).unwrap().len()) as u64
    };

    println!("A shuffle      z19      : {:>10} ({:.2} MiB)", a19, mib(a19));
    println!("B chunk-local  z19      : {:>10} ({:.2} MiB)  x{:.2} vs A", b19, mib(b19), a19 as f64 / b19 as f64);
    println!("B chunk-local  z22      : {:>10} ({:.2} MiB)  x{:.2} vs A", b22, mib(b22), a19 as f64 / b22 as f64);
    println!("B chunk-local  z19+LDM  : {:>10} ({:.2} MiB)  x{:.2} vs A", b19ldm, mib(b19ldm), a19 as f64 / b19ldm as f64);
    println!("C B+tail       z19      : {:>10} ({:.2} MiB)  x{:.2} vs A", c19, mib(c19), a19 as f64 / c19 as f64);
    println!("D 64KB slabs   z19      : {:>10} ({:.2} MiB)  x{:.2} vs A", d19, mib(d19), a19 as f64 / d19 as f64);
    println!("E single group z19 (ref): {:>10} ({:.2} MiB)", e19, mib(e19));

    // dict effect on top of B
    let dict_train: Vec<&[u8]> = b_stream.iter().take(400).map(|(_, b)| b.as_slice()).collect();
    if let Ok(dict) = zstd::dict::from_samples(&dict_train, 112 * 1024) {
        let mut total = 0u64;
        let mut cur: Vec<u8> = Vec::with_capacity(slab);
        for (_, b) in &b_stream {
            if !cur.is_empty() && cur.len() + b.len() > slab {
                let mut c = zstd::bulk::Compressor::new(19).unwrap();
                c.set_dictionary(19, &dict).unwrap();
                total += 8 + 12 + c.compress(&cur).unwrap().len() as u64;
                cur.clear();
            }
            cur.extend_from_slice(b);
        }
        if !cur.is_empty() {
            let mut c = zstd::bulk::Compressor::new(19).unwrap();
            c.set_dictionary(19, &dict).unwrap();
            total += 8 + 12 + c.compress(&cur).unwrap().len() as u64;
        }
        println!("B chunk-local  z19+dict : {:>10} ({:.2} MiB)  x{:.2} vs A", total, mib(total), a19 as f64 / total as f64);
    }


    // ---- prefix-dict experiment: compress each slab against a shared raw
    // prefix (stored once as a blob). Emulates cross-slab matching.
    {
        for &prefix_kb in &[256usize, 512, 1024] {
            let prefix_len = prefix_kb * 1024;
            let mut prefix: Vec<u8> = Vec::with_capacity(prefix_len);
            for (_, b) in &b_stream {
                if prefix.len() >= prefix_len { break; }
                let take = (prefix_len - prefix.len()).min(b.len());
                prefix.extend_from_slice(&b[..take]);
            }
            let mut total = prefix.len() as u64 + 8 + 30; // prefix blob + framing
            let mut cur: Vec<u8> = Vec::with_capacity(slab);
            let mut flush = |cur: &mut Vec<u8>, total: &mut u64| {
                if cur.is_empty() { return; }
                let mut c = zstd::bulk::Compressor::new(19).unwrap();
                c.set_dictionary(19, &prefix).unwrap();
                *total += 8 + 12 + c.compress(cur).unwrap().len() as u64;
                cur.clear();
            };
            for (_, b) in &b_stream {
                if !cur.is_empty() && cur.len() + b.len() > slab { flush(&mut cur, &mut total); }
                cur.extend_from_slice(b);
            }
            flush(&mut cur, &mut total);
            println!("B chunk-local  z19+prefix{prefix_kb:>4}K: {total:>10} ({:.2} MiB)  x{:.2} vs A", mib(total), a19 as f64 / total as f64);
        }
        // entities: same trick on whole-mode payloads
        let mut ent_chunks = vec![];
        for f in std::fs::read_dir(dir.join("entities")).unwrap().flatten() {
            let p2 = f.path();
            if p2.to_string_lossy().ends_with(".mca") { ent_chunks.extend(load_mca(&p2)); }
        }
        ent_chunks.sort_by_key(|(cx, cz, _)| (*cx, *cz));
        let mut ent_stream: Vec<(u64, Vec<u8>)> = Vec::new();
        for (cx, cz, p) in &ent_chunks {
            if let chunknbt::Split::Fields { parts, .. } = chunknbt::split(p, true, Some((*cx, *cz))).unwrap() {
                for part in parts {
                    if !part.inline { ent_stream.push((key(*cx, *cz), part.bytes)); }
                }
            }
        }
        let ent_raw: usize = ent_stream.iter().map(|(_, b)| b.len()).sum();
        // baseline: per-blob z19+dict (current engine behavior)
        let dict = zstd::dict::from_samples(&ent_stream.iter().take(200).map(|(_, b)| b.as_slice()).collect::<Vec<_>>(), 112 * 1024).unwrap();
        let mut base = 0u64;
        for (_, b) in &ent_stream {
            let mut c = zstd::bulk::Compressor::new(19).unwrap();
            c.set_dictionary(19, &dict).unwrap();
            base += 8 + 30 + c.compress(b).unwrap().len() as u64;
        }
        println!("entities raw {:.2} MiB | per-blob z19+dict: {} ({:.2} MiB)", mib(ent_raw as u64), base, mib(base));
        for &prefix_kb in &[256usize, 512] {
            let prefix_len = prefix_kb * 1024;
            let mut prefix: Vec<u8> = Vec::new();
            for (_, b) in &ent_stream {
                if prefix.len() >= prefix_len { break; }
                let take = (prefix_len - prefix.len()).min(b.len());
                prefix.extend_from_slice(&b[..take]);
            }
            let mut total = prefix.len() as u64 + 8 + 30;
            let mut cur: Vec<u8> = Vec::with_capacity(slab);
            let mut flush = |cur: &mut Vec<u8>, total: &mut u64| {
                if cur.is_empty() { return; }
                let mut c = zstd::bulk::Compressor::new(19).unwrap();
                c.set_dictionary(19, &prefix).unwrap();
                *total += 8 + 12 + c.compress(cur).unwrap().len() as u64;
                cur.clear();
            };
            for (_, b) in &ent_stream {
                if !cur.is_empty() && cur.len() + b.len() > slab { flush(&mut cur, &mut total); }
                cur.extend_from_slice(b);
            }
            flush(&mut cur, &mut total);
            println!("entities z19+prefix{prefix_kb:>4}K: {total:>10} ({:.2} MiB)  x{:.2} vs per-blob", mib(total), base as f64 / total as f64);
        }
    }


    // ---- deduped-stream slab-size + ordering sweep ----
    {
        // unique sections only, first occurrence per canonical bytes
        let mut uniq_seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut uniq: Vec<(u64, Vec<u8>)> = Vec::new();
        for (k, b) in &b_stream {
            if uniq_seen.insert(b.clone()) {
                uniq.push((*k, b.clone()));
            }
        }
        let raw_u: usize = uniq.iter().map(|(_, b)| b.len()).sum();
        println!("deduped stream: {} sections, {:.2} MiB raw", uniq.len(), mib(raw_u as u64));
        for &slab_kb in &[256usize, 512, 1024] {
            let t = compress_grouped(&uniq, slab_kb * 1024, 19, false);
            println!("  uniq lex-order   z19 slab {slab_kb:>4}K: {t:>10} ({:.2} MiB)", mib(t));
        }
        // Morton order (2D locality)
        let morton = |k: u64| -> u64 {
            let cx = (k >> 32) as u32 as u64;
            let cz = k & 0xffff_ffff;
            let spread = |x: u64| -> u64 {
                let mut v = x & 0xffff;
                v = (v | (v << 8)) & 0x00FF00FF;
                v = (v | (v << 4)) & 0x0F0F0F0F;
                v = (v | (v << 2)) & 0x33333333;
                v = (v | (v << 1)) & 0x55555555;
                v
            };
            spread(cx) | (spread(cz) << 1)
        };
        let mut mort: Vec<(u64, Vec<u8>)> = uniq.clone();
        mort.sort_by_key(|(k, _)| morton(*k));
        for &slab_kb in &[256usize, 1024] {
            let t = compress_grouped(&mort, slab_kb * 1024, 19, false);
            println!("  uniq morton-order z19 slab {slab_kb:>4}K: {t:>10} ({:.2} MiB)", mib(t));
        }
        // per-chunk section groups are natural; try chunk=slab granularity
        let t = compress_grouped(&uniq, 160 * 1024, 19, false);
        println!("  uniq lex-order   z19 slab  160K: {t:>10} ({:.2} MiB)", mib(t));
    }

    // sanity: the store pipeline number for the same corpus
    let _ = Store::open(std::path::Path::new("/tmp/jjexp-probe"), false);
    let _ = key(0, 0);
}
