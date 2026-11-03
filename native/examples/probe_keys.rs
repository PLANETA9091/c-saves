//! Probe: fastnbt's authoritative view of every corpus chunk root — key
//! names/types, sections list count/elem type — cross-checked against the
//! scan-split walk (scan_probe). Any chunk fastnbt parses but scan_root
//! rejects shows up as SCANFAIL with its reason code. Usage:
//! probe_keys <dim-dir>
use c_saves::chunknbt;
use std::io::Read;
use std::path::PathBuf;

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: probe_keys <dim-dir>"));
    let mut shapes: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut n_all = 0usize;
    for r in 0..=31 {
        for z in 0..=31 {
            let f = dim.join("region").join(format!("r.0.{}.mca", r * 32 + z));
            let Ok(data) = std::fs::read(&f) else { continue };
            eprintln!("READ {} bytes={}", f.display(), data.len());
            for c in 0..1024 {
                let off = (c * 4) as usize;
                let oo = (u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) >> 8) as usize * 4096;
                if oo == 0 || oo + 5 > data.len() { continue; }
                let l = u32::from_be_bytes([data[oo], data[oo + 1], data[oo + 2], data[oo + 3]]) as usize;
                if l < 2 || oo + 4 + l > data.len() { continue; }
                let cb = data[oo + 4];
                let raw = &data[oo + 5..oo + 4 + l];
                let payload: Vec<u8> = match cb {
                    2 => flate2::read::ZlibDecoder::new(raw).bytes().collect::<Result<Vec<u8>, _>>().unwrap_or_default(),
                    1 => flate2::read::GzDecoder::new(raw).bytes().collect::<Result<Vec<u8>, _>>().unwrap_or_default(),
                    _ => raw.to_vec(),
                };
                if payload.is_empty() {
                    *shapes.entry(format!("DECOMP_FAIL cb={cb}")).or_default() += 1;
                    continue;
                }
                n_all += 1;
                // scan-walk cross-check: report failures with chunk identity
                let (why, pos, ok) = chunknbt::scan_probe(&payload, false);
                if !ok {
                    eprintln!(
                        "SCANFAIL file=r.0.{}.mca c={} len={} why={} pos={}",
                        r * 32 + z, c, payload.len(), why, pos
                    );
                }
                let Ok(root) = fastnbt::from_bytes::<fastnbt::Value>(&payload) else {
                    *shapes.entry("PARSE_FAIL".into()).or_default() += 1;
                    continue;
                };
                let fastnbt::Value::Compound(m) = &root else {
                    *shapes.entry("ROOT_NOT_COMPOUND".into()).or_default() += 1;
                    continue;
                };
                let mut sig = String::new();
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for k in keys {
                    let t = match m[k] {
                        fastnbt::Value::Byte(_) => "b",
                        fastnbt::Value::Short(_) => "s",
                        fastnbt::Value::Int(_) => "i",
                        fastnbt::Value::Long(_) => "l",
                        fastnbt::Value::Float(_) => "f",
                        fastnbt::Value::Double(_) => "d",
                        fastnbt::Value::ByteArray(_) => "B",
                        fastnbt::Value::String(_) => "S",
                        fastnbt::Value::IntArray(_) => "I",
                        fastnbt::Value::LongArray(_) => "L",
                        fastnbt::Value::List(_) => "T",
                        fastnbt::Value::Compound(_) => "C",
                    };
                    sig.push_str(&format!("{k}:{t} "));
                }
                let sc = match m.get("sections") {
                    Some(fastnbt::Value::List(v)) => format!("n={}", v.len()),
                    Some(_) => "NONLIST".into(),
                    None => "ABSENT".into(),
                };
                *shapes.entry(format!("{sig}|sections {sc}")).or_default() += 1;
            }
        }
    }
    eprintln!("chunks={n_all} distinct-shapes={}", shapes.len());
    for (sig, n) in &shapes {
        println!("{n:6}  {sig}");
    }
    let _ = chunknbt::split_stage_json();
}
