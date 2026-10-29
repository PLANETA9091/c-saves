//! jj_exp2: why doesn't batched entity slab compression reach the z19
//! concatenated floor? Isolates order / framing / member-content effects.
//!
//! Usage: cargo run --release --example jj_exp2 -- <dim-dir>

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

fn main() {
    let dim = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let mut chunks = Vec::new();
    for e in std::fs::read_dir(dim.join("entities")).unwrap().flatten() {
        chunks.extend(load_mca(&e.path()));
    }
    chunks.sort_by_key(|(cx, cz, _)| key(*cx, *cz));
    let total: usize = chunks.iter().map(|(_, _, p)| p.len()).sum();
    println!("chunks={} total_raw={total} ({:.2} MiB)", chunks.len(), total as f64 / 1048576.0);

    // size histogram + species check on one chunk
    let mut sizes: Vec<usize> = chunks.iter().map(|(_, _, p)| p.len()).collect();
    sizes.sort_unstable();
    println!(
        "payload size: min={} p50={} p90={} max={}",
        sizes[0],
        sizes[sizes.len() / 2],
        sizes[sizes.len() * 9 / 10],
        sizes[sizes.len() - 1]
    );

    // A: single frame, mca order
    let mut a = Vec::with_capacity(total);
    for (_, _, p) in &chunks {
        a.extend_from_slice(p);
    }
    let ca = zstd::bulk::compress(&a, 19).unwrap();
    println!("A whole-concat z19           : {} (x{:.2})", ca.len(), a.len() as f64 / ca.len() as f64);

    // C: 4 MiB frames in key order
    let mut c_tot = 0usize;
    let mut frame = Vec::new();
    for (_, _, p) in &chunks {
        if !frame.is_empty() && frame.len() + p.len() > 4 * 1024 * 1024 {
            c_tot += zstd::bulk::compress(&frame, 19).unwrap().len();
            frame.clear();
        }
        frame.extend_from_slice(p);
    }
    if !frame.is_empty() {
        c_tot += zstd::bulk::compress(&frame, 19).unwrap().len();
    }
    println!("C 4MiB-frames z19            : {c_tot} (x{:.2})", total as f64 / c_tot as f64);

    // D: simulate slab v2 stream (tab hashes + raws) in key order, 4 MiB
    let mut d_tot = 0usize;
    let mut group: Vec<&Vec<u8>> = Vec::new();
    let mut gbytes = 0usize;
    let flush = |group: &Vec<&Vec<u8>>, d_tot: &mut usize| {
        let mut s = Vec::new();
        for (i, p) in group.iter().enumerate() {
            putv(&mut s, i as u64); // id delta ~1
            putv(&mut s, p.len() as u64);
            s.extend_from_slice(&[0x42u8; 16]); // fake hash
        }
        for p in group {
            s.extend_from_slice(p);
        }
        *d_tot += zstd::bulk::compress(&s, 19).unwrap().len();
    };
    for (_, _, p) in &chunks {
        if !group.is_empty() && gbytes + p.len() > 4 * 1024 * 1024 {
            flush(&group, &mut d_tot);
            group.clear();
            gbytes = 0;
        }
        gbytes += p.len();
        group.push(p);
    }
    if !group.is_empty() {
        flush(&group, &mut d_tot);
    }
    println!("D slabv2-stream 4MiB z19     : {d_tot} (x{:.2})", total as f64 / d_tot as f64);

    // E: per-chunk independent (old behavior)
    let mut e_tot = 0usize;
    for (_, _, p) in &chunks {
        e_tot += zstd::bulk::compress(p, 19).unwrap().len();
    }
    println!("E per-chunk z19              : {e_tot} (x{:.2})", total as f64 / e_tot as f64);

    // G: replicate the ENGINE member stream: split(whole_mode) -> Entities
    // field bytes (wrapped compound) concatenated in key order
    let mut g = Vec::with_capacity(total);
    let mut g_parts = 0usize;
    for (cx, cz, p) in &chunks {
        if let Ok(c_saves::chunknbt::Split::Fields { parts, .. }) =
            c_saves::chunknbt::split(p, true, Some((*cx, *cz)), false)
        {
            for part in parts {
                if !part.inline {
                    g.extend_from_slice(&part.bytes);
                    g_parts += 1;
                }
            }
        }
    }
    let cg = zstd::bulk::compress(&g, 19).unwrap();
    println!(
        "G engine-members z19         : {} parts={} raw={} (x{:.2})",
        cg.len(), g_parts, g.len(), g.len() as f64 / cg.len() as f64
    );

    // H: field-order forensics — original entity field order vs re-serialized
    use fastnbt::Value;
    let (hx, hz, hp) = &chunks[5];
    if let Ok(Value::Compound(root)) = fastnbt::from_bytes::<Value>(hp) {
        if let Some(Value::List(ents)) = root.get("Entities") {
            println!("\nH field-order forensics for chunk ({hx},{hz}), {} entities:", ents.len());
            for (i, e) in ents.iter().enumerate().take(2) {
                if let Value::Compound(m) = e {
                    println!("  original entity[{i}] id={:?} keys:", m.get("id").and_then(|v| if let Value::String(s) = v { Some(s.clone()) } else { None }).unwrap_or_default());
                    println!("    {:?}", m.keys().collect::<Vec<_>>());
                }
            }
        }
    }
    if let Ok(c_saves::chunknbt::Split::Fields { parts, .. }) =
        c_saves::chunknbt::split(hp, true, Some((*hx, *hz)), false)
    {
        for part in &parts {
            if !part.inline {
                if let Ok(Value::Compound(w)) = fastnbt::from_bytes::<Value>(&part.bytes) {
                    if let Some(Value::List(ents)) = w.values().next() {
                        for (i, e) in ents.iter().enumerate().take(2) {
                            if let Value::Compound(m) = e {
                                println!("  reserialized entity[{i}] keys:");
                                println!("    {:?}", m.keys().collect::<Vec<_>>());
                            }
                        }
                    }
                }
            }
        }
    }

    // I: byte-diff two re-serialized same-species entities from different chunks
    let grab = |idx: usize| -> Option<Vec<u8>> {
        let (cx, cz, p) = &chunks[idx];
        if let Ok(c_saves::chunknbt::Split::Fields { parts, .. }) =
            c_saves::chunknbt::split(p, true, Some((*cx, *cz)), false)
        {
            for part in parts {
                if !part.inline {
                    return Some(part.bytes);
                }
            }
        }
        None
    };
    if let (Some(a), Some(b)) = (grab(5), grab(50)) {
        let pa = a.len().min(b.len());
        let mut first_diff = None;
        for i in 0..pa {
            if a[i] != b[i] {
                first_diff = Some(i);
                break;
            }
        }
        println!(
            "\nI member[5] len={} member[50] len={} first_diff={:?} of common {}",
            a.len(),
            b.len(),
            first_diff,
            pa
        );
        if let Some(d) = first_diff {
            let s = d.saturating_sub(24);
            println!("  around diff: A={:02x?}", &a[s..(d + 24).min(a.len())]);
            println!("  around diff: B={:02x?}", &b[s..(d + 24).min(b.len())]);
        }
        // how many bytes are equal in the first 4000?
        let eq = a.iter().zip(b.iter()).take(4000).filter(|(x, y)| x == y).count();
        println!("  equal bytes in first 4000 common: {eq}");
    }


    // P: palette-size histogram + data-bytes-by-class over region sections
    let mut hist: std::collections::BTreeMap<usize, (u64, u64)> = Default::default();
    let mut load2 = |p: &std::path::Path| -> Vec<(i32, i32, Vec<u8>)> { load_mca(p) };
    for e in std::fs::read_dir(dim.join("region")).unwrap().flatten() {
        for (_cx, _cz, p) in load2(&e.path()) {
            if let Ok(Value::Compound(root)) = fastnbt::from_bytes::<Value>(&p) {
                if let Some(Value::List(sects)) = root.get("sections") {
                    for s in sects {
                        if let Value::Compound(sm) = s {
                            if let Some(Value::Compound(bs)) = sm.get("block_states") {
                                let n = match bs.get("palette") {
                                    Some(Value::List(l)) => l.len(),
                                    _ => 0,
                                };
                                let longs = match bs.get("data") {
                                    Some(Value::LongArray(la)) => la.as_ref().len() as u64,
                                    _ => 0,
                                };
                                let e = hist.entry(n).or_default();
                                e.0 += 1;
                                e.1 += longs * 8;
                            }
                        }
                    }
                }
            }
        }
    }
    println!("\nP palette-size histogram (sections, data bytes):");
    let mut tot_cur = 0u64; let mut tot_narrow = 0u64;
    for (n, (cnt, bytes)) in &hist {
        println!("  n={n:>4}: sections={cnt:>6} data_bytes={bytes:>10}");
        let cur_bits = cbits(*n).max(4) as u64;
        let nar_bits = cbits(*n).max(1) as u64;
        tot_cur += bytes;
        tot_narrow += bytes * nar_bits / cur_bits.max(1);
    }
    println!("P data raw total={tot_cur} narrow-packed would be={tot_narrow} (x{:.2} less)", tot_cur as f64 / tot_narrow.max(1) as f64);

    // J: full fastnbt round-trip forensics — diff payload vs re-serialized
    {
        let (jx, jz, jp) = &chunks[5];
        let v: fastnbt::Value = fastnbt::from_bytes(jp).unwrap();
        let rt = fastnbt::to_bytes(&v).unwrap();
        println!("\nJ chunk ({jx},{jz}): original={} roundtrip={} equal_len={}", jp.len(), rt.len(), rt.len() == jp.len());
        let n = jp.len().min(rt.len());
        let mut diffs = 0;
        let mut first = None;
        for i in 0..n {
            if jp[i] != rt[i] {
                diffs += 1;
                if first.is_none() {
                    first = Some(i);
                }
            }
        }
        println!("J byte diffs: {diffs}/{n} first={first:?}");
        if let Some(d) = first {
            let s = d.saturating_sub(16);
            println!("J orig: {:02x?}", &jp[s..(d + 32).min(jp.len())]);
            println!("J rt  : {:02x?}", &rt[s..(d + 32).min(rt.len())]);
        }
        let c0 = zstd::bulk::compress(jp, 19).unwrap();
        let c1 = zstd::bulk::compress(&rt, 19).unwrap();
        println!("J z19: orig {} rt {}", c0.len(), c1.len());
    }

    // F: what does the engine actually store? split a few chunks whole-mode
    for (cx, cz, p) in chunks.iter().take(3) {
        match c_saves::chunknbt::split(p, true, Some((*cx, *cz)), false) {
            Ok(c_saves::chunknbt::Split::Fields { parts, pos_from_key }) => {
                println!("chunk ({cx},{cz}): Fields n={} pos_from_key={pos_from_key}", parts.len());
                for part in &parts {
                    println!(
                        "  {:12} inline={} bytes={}",
                        part.key,
                        part.inline,
                        part.bytes.len()
                    );
                }
            }
            Ok(other) => println!("chunk ({cx},{cz}): {other:?}"),
            Err(e) => println!("split err {e}"),
        }
    }
}

fn putv(v: &mut Vec<u8>, mut x: u64) {
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            v.push(b);
            break;
        }
        v.push(b | 0x80);
    }
}

fn cbits(n: usize) -> u64 {
    let mut b = 1u64;
    while (1u64 << b) < n as u64 { b += 1; }
    b
}
