// dir_probe: run ONLY the split/import stage over the corpus to debug the
// v0.9.17 direct sections parser refusals (CSAVES_SCAN_DEBUG=1 prints them).
use std::path::Path;

type Corpus = Vec<(i32, i32, Vec<u8>, Vec<u8>)>;

fn load_mca(path: &Path) -> Corpus {
    use std::io::Read;
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
        out.push((cx, cz, payload, comp));
    }
    out
}

fn load_dir(dir: &Path) -> Corpus {
    let mut all = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    for p in entries {
        if p.extension().map(|e| e == "mca").unwrap_or(false) {
            all.extend(load_mca(&p));
        }
    }
    all
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "../bench-corpus/dim".into());
    let corpus = load_dir(Path::new(&arg));
    println!("dir_probe: chunks={}", corpus.len());
    let mut ok = 0usize;
    let mut fail = 0usize;
    for (_, _, payload, _) in &corpus {
        match c_saves::chunknbt::split(payload, false, None, true) {
            Ok(_) => ok += 1,
            Err(e) => { fail += 1; println!("split err: {e}"); }
        }
    }
    println!("dir_probe: split_ok={ok} split_fail={fail}");
    println!("stages: {}", c_saves::chunknbt::split_stage_json());
}
