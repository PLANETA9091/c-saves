//! probe_bad2: reproduce the jj_profile verify bad=2 on chunks (0,9)/(1,9),
//! diffing per-section expanded state vectors against the source.
use c_saves::chunknbt;
use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::fs::File;
use std::io::Read;
use std::path::Path;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
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

fn expand_states(sec: &Value) -> Option<(Vec<String>, Vec<u32>)> {
    let Value::Compound(m) = sec else { return None };
    let Value::Compound(bs) = m.get("block_states")? else { return None };
    let Value::List(pal) = bs.get("palette")? else { return None };
    let n = pal.len();
    let mut names = Vec::with_capacity(n);
    for p in pal {
        match p {
            Value::String(s) => names.push(s.clone()),
            Value::Compound(cm) => {
                let Value::String(nm) = cm.get("Name")? else { return None };
                names.push(nm.clone());
            }
            _ => return None,
        }
    }
    match bs.get("data") {
        Some(Value::LongArray(la)) => {
            let bits = ((la.as_ref().len() as u32) * 64 / 4096).max(1);
            let vox = chunknbt::unpack_nostroddle_wide(la.as_ref(), 4096, bits);
            Some((names, vox.into_iter().map(|x| x as u32).collect()))
        }
        None => Some((names, vec![0; 4096])),
        _ => None,
    }
}

fn y_of(sec: &Value) -> i8 {
    match sec {
        Value::Compound(m) => match m.get("Y") {
            Some(Value::Byte(b)) => *b,
            _ => 0,
        },
        _ => 0,
    }
}

fn main() {
    let dim = Path::new("../bench-corpus/dim");
    let chunks: Vec<_> = load_mca(&dim.join("region/r.0.0.mca"))
        .into_iter()
        .chain(load_mca(&dim.join("region/r.0.-1.mca")))
        .collect();
    let targets: Vec<&(i32, i32, Vec<u8>)> = chunks
        .iter()
        .filter(|(cx, cz, _)| (*cx == 0 && *cz == 9) || (*cx == 1 && *cz == 9))
        .collect();
    println!("targets found: {}", targets.len());
    // codec-level byte-identity sweep: for every section of the bad chunks,
    // canonical encode -> section_to_glob -> glob_any_to_section must
    // reproduce the canonical bytes exactly
    for (cx, cz, p) in &targets {
        let Ok(Value::Compound(a)) = fastnbt::from_bytes::<Value>(p) else { panic!("src parse") };
        let Value::List(sa) = a.get("sections").unwrap() else { panic!("sections") };
        let mut reg = chunknbt::StateRegistry::new();
        let mut non_ident = 0usize;
        let mut forms: Map<String, usize> = Map::new();
        for s in sa.iter() {
            let Some(csec) = chunknbt::section_encode(s) else { continue };
            let Some(g) = chunknbt::section_to_glob(&csec, &mut reg) else { continue };
            let fm = if g[0] == 0xCB {
                "0xCB".to_string()
            } else {
                format!("0xCC/g{}", g[5])
            };
            *forms.entry(fm).or_default() += 1;
            match chunknbt::glob_any_to_section(&g, &reg) {
                Some(back) if back == csec => {}
                Some(back) => {
                    non_ident += 1;
                    if non_ident <= 3 {
                        let y = y_of(s);
                        println!(
                            "  ({cx},{cz}) Y={y}: NON-IDENTICAL canon={} back={} gform={}",
                            csec.len(),
                            back.len(),
                            if g[0] == 0xCB { "CB".into() } else { format!("{}", g[5]) }
                        );
                        let first_diff = back
                            .iter()
                            .zip(csec.iter())
                            .position(|(x, y)| x != y)
                            .unwrap_or(0);
                        println!("    first_diff at byte {first_diff}");
                    }
                }
                _ => {
                    non_ident += 1;
                    println!("  ({cx},{cz}) Y={}: DECODE FAILED", y_of(s));
                }
            }
        }
        println!("chunk ({cx},{cz}): non_identical={non_ident} forms={forms:?}");
    }
    let out = std::env::temp_dir().join("probe_bad2_out");
    let _ = std::fs::remove_dir_all(&out);
    let cfg = Cfg {
        compact_min: 256 * 1024,
        compact_ratio: 0.10,
        dict_train: 200,
        ..Cfg::default()
    };
    let rs = Store::open_cfg(&out, true, cfg).unwrap();
    let strip = true;
    for (cx, cz, p) in &targets {
        rs.write_chunk_stripped(key(*cx, *cz), p, strip).unwrap();
    }
    rs.sync();
    rs.seal();
    rs.request_pack();
    while rs.compact_step() {}
    rs.sync();
    // read back and diff
    for (cx, cz, p) in &targets {
        let d = rs.read_chunk(key(*cx, *cz)).expect("read back");
        let Ok(Value::Compound(a)) = fastnbt::from_bytes::<Value>(p) else { panic!("src parse") };
        let mut a = Value::Compound(a);
        chunknbt::strip_derived_chunk(&mut a);
        let Value::Compound(a) = a else { unreachable!() };
        let Ok(Value::Compound(b)) = fastnbt::from_bytes::<Value>(&d) else { panic!("back parse") };
        let Value::List(sa) = a.get("sections").unwrap() else { panic!("sections") };
        let Value::List(sb) = b.get("sections").unwrap() else { panic!("sections") };
        println!("chunk ({cx},{cz}): {} sections vs {}", sa.len(), sb.len());
        for (ya, yb) in sa.iter().zip(sb.iter()) {
            if !chunknbt::sections_semantically_equal(ya, yb) {
                let (Some((pa, va)), Some((pb, vb))) = (expand_states(ya), expand_states(yb))
                else {
                    println!("  Y?: structural mismatch");
                    continue;
                };
                let mut diffs = 0usize;
                let mut first = None;
                for i in 0..4096 {
                    let (x, y, z) = (i % 16, i / 256, (i / 16) % 16);
                    let sa_ = pa.get(va[i as usize] as usize).cloned().unwrap_or_default();
                    let sb_ = pb.get(vb[i as usize] as usize).cloned().unwrap_or_default();
                    if sa_ != sb_ {
                        diffs += 1;
                        if first.is_none() {
                            first = Some((x, y, z, sa_, sb_));
                        }
                    }
                }
                let ya_i = y_of(ya);
                println!(
                    "  Y={ya_i} MISMATCH: diffs={diffs} pal_a={} pal_b={} first={:?}",
                    pa.len(),
                    pb.len(),
                    first
                );
                // palette histogram both sides
                let mut ha: Map<&str, usize> = Map::new();
                for v in &va {
                    *ha.entry(pa[*v as usize].as_str()).or_default() += 1;
                }
                let mut hb: Map<&str, usize> = Map::new();
                for v in &vb {
                    *hb.entry(pb[*v as usize].as_str()).or_default() += 1;
                }
                println!("    hist_a: {:?}", ha);
                println!("    hist_b: {:?}", hb);
            }
        }
    }
    println!("done");
}
