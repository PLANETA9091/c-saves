//! keys_probe: distinct root keys and section key-shapes of a corpus dim
//! (what Paper 26.3 actually writes vs what the import rebuild keeps).
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim/region");
    let mut root_keys = std::collections::BTreeMap::<String, usize>::new();
    let mut sec_keys = std::collections::BTreeMap::<String, usize>::new();
    let mut sec_shapes = std::collections::BTreeMap::<String, usize>::new();
    let mut chunks = 0usize;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("mca") {
            continue;
        }
        let data = std::fs::read(&p).unwrap();
        if data.len() < 8192 {
            continue;
        }
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
            let comp = &data[off + 5..off + 4 + len];
            let mut payload = Vec::new();
            match comp_type {
                1 => {
                    if flate2::read::GzDecoder::new(comp).read_to_end(&mut payload).is_err() {
                        continue;
                    }
                }
                2 => {
                    if flate2::read::ZlibDecoder::new(comp).read_to_end(&mut payload).is_err() {
                        continue;
                    }
                }
                3 => payload.extend_from_slice(comp),
                _ => continue,
            }
            let Ok(fastnbt::Value::Compound(root)) = fastnbt::from_bytes::<fastnbt::Value>(&payload) else {
                continue;
            };
            chunks += 1;
            for k in root.keys() {
                *root_keys.entry(k.clone()).or_default() += 1;
            }
            if let Some(fastnbt::Value::List(secs)) = root.get("sections") {
                for s in secs {
                    if let fastnbt::Value::Compound(sm) = s {
                        let set: BTreeSet<&String> = sm.keys().collect();
                        let shape: Vec<&str> = set.iter().map(|s| s.as_str()).collect();
                        *sec_shapes.entry(shape.join("+")).or_default() += 1;
                        for k in sm.keys() {
                            *sec_keys.entry(k.clone()).or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    println!("chunks parsed: {chunks}");
    println!("--- root keys ---");
    for (k, c) in &root_keys {
        println!("{c:6}  {k}");
    }
    println!("--- section keys ---");
    for (k, c) in &sec_keys {
        println!("{c:6}  {k}");
    }
    println!("--- section shapes ---");
    for (k, c) in &sec_shapes {
        println!("{c:6}  {k}");
    }
}
