//! vanilla_dead: count DEAD PALETTE ENTRIES in REAL Paper chunk sections
//! (the bench corpus .mca files) — is the drift class a synthetic artifact
//! or vanilla reality? Parses every section's block_states palette and
//! data longs directly from the vanilla NBT (vanilla packing, no csec).
use flate2::read::ZlibDecoder;
use fastnbt::{LongArray, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

fn read_region(rx: i32, rz: i32, dir: &Path) -> Vec<Vec<u8>> {
    let path = dir.join(format!("r.{}.{}.mca", rx, rz));
    let Ok(data) = std::fs::read(&path) else { return Vec::new() };
    let mut out = Vec::new();
    for i in 0..1024usize {
        let e = i * 4;
        let off = ((data[e] as usize) << 16) | ((data[e + 1] as usize) << 8) | data[e + 2] as usize;
        if off == 0 || data[e + 3] == 0 { continue; }
        let a = off * 4096;
        if a + 5 > data.len() { continue; }
        let len = u32::from_be_bytes([data[a], data[a + 1], data[a + 2], data[a + 3]]) as usize;
        if len == 0 || a + 5 + len > data.len() { continue; }
        if data[a + 4] == 2 {
            let mut d = Vec::with_capacity(len);
            if ZlibDecoder::new(&data[a + 5..a + 5 + len]).read_to_end(&mut d).is_ok() {
                out.push(d);
            }
        }
    }
    out
}

fn unpack_vanilla(data: &[i64], bits: u32, n: usize) -> Vec<u32> {
    let per = (64 / bits) as usize;
    let mask = ((1u64 << bits) - 1) as i64;
    (0..n)
        .map(|i| {
            let li = i / per;
            let sh = (i % per) * bits as usize;
            ((data[li] >> sh) & mask) as u32
        })
        .collect()
}

fn main() {
    let dim_arg = std::env::args().nth(1).expect("dim-dir");
    let dim = Path::new(&dim_arg);
    let rdir = dim.join("region");
    let mut regions: Vec<(i32, i32)> = std::fs::read_dir(&rdir)
        .expect("region dir")
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let p: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
            if p.len() == 3 && p[0] == "r" {
                Some((p[1].parse().ok()?, p[2].parse().ok()?))
            } else { None }
        })
        .collect();
    regions.sort();
    let (mut secs, mut dead_secs, mut dead_entries, mut plain_secs) = (0u64, 0u64, 0u64, 0u64);
    for (rx, rz) in &regions {
        for payload in read_region(*rx, *rz, &rdir) {
            let Ok(Value::Compound(root)) = fastnbt::from_bytes::<Value>(&payload) else { continue };
            let Some(Value::List(seclist)) = root.get("sections") else { continue };
            for s in seclist {
                let Value::Compound(sm) = s else { continue };
                let Some(Value::Compound(bs)) = sm.get("block_states") else { continue };
                let Value::List(pal) = bs.get("palette").unwrap_or(&Value::Byte(0)) else { continue };
                let n = pal.len();
                secs += 1;
                if n <= 1 {
                    plain_secs += 1;
                    continue;
                }
                let used: HashSet<usize> = match bs.get("data") {
                    Some(Value::LongArray(la)) => {
                        let bits = ((n as u32 - 1).ilog2() + 1).max(4);
                        unpack_vanilla(la.as_ref(), bits, 4096)
                            .into_iter()
                            .filter(|&v| (v as usize) < n)
                            .map(|v| v as usize)
                            .collect()
                    }
                    _ => HashSet::new(),
                };
                let dead = n - used.len();
                if dead > 0 {
                    dead_secs += 1;
                    dead_entries += dead as u64;
                }
            }
        }
    }
    println!("regions={} sections={secs} plain(n<=1)={plain_secs}", regions.len());
    println!("sections with dead palette entries: {dead_secs} ({:.2}%)", 100.0 * dead_secs as f64 / secs.max(1) as f64);
    println!("total dead entries: {dead_entries}");
}
