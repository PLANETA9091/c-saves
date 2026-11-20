//! stages_probe: the write/read stage decomposition on the real JJ corpus —
//! the same pipeline speed_bench drives, with the stage timers printed after
//! every phase so the next lever is picked from numbers, not intuition.
//!
//! Phases (mirroring speed_bench):
//!   C  import ST  -> write_stage_json (split/ensure/recipe + ensx detail)
//!   E  read cold  -> read_stage_json (recipe/tail/sectload/join/decomp)
//!   F  read warm  -> read_stage_json again (cache-hit profile)
//!
//! Usage: cargo run --release --example stages_probe -- <dim-dir>
use c_saves::store::{self, Cfg, Store};
use std::path::PathBuf;

fn load_mca(path: &std::path::Path) -> Vec<(i32, i32, Vec<u8>)> {
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
        let loc = u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
        if loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        if off + 5 > data.len() {
            continue;
        }
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if len < 2 || off + 4 + len > data.len() {
            continue;
        }
        let comp_type = data[off + 4];
        let comp = data[off + 5..off + 4 + len].to_vec();
        let payload = match comp_type {
            1 => {
                let mut v = Vec::new();
                if std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&comp[..]), &mut v).is_err() {
                    continue;
                }
                v
            }
            2 => {
                let mut v = Vec::new();
                if std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&comp[..]), &mut v).is_err() {
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

fn load_corpus(dim: &std::path::Path) -> Vec<(i32, i32, Vec<u8>)> {
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

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: stages_probe <dim-dir>"));
    let corpus = load_corpus(&dim);
    println!("corpus: {} chunks", corpus.len());

    let dir = std::env::temp_dir().join(format!(
        "csaves-stages-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();

    // phase C: import ST
    store::write_stage_reset();
    let raw: u64 = corpus.iter().map(|(_, _, p)| p.len() as u64).sum();
    let t = std::time::Instant::now();
    for (cx, cz, p) in &corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    let wall = t.elapsed();
    println!("C import ST: {:.1} MB/s ({:?})", raw as f64 / 1e6 / wall.as_secs_f64(), wall);
    println!("  write stages: {}", store::write_stage_json());
    println!("  ensx detail: {}", store::ensure_stage_json());

    // pack to steady state
    st.sync();
    st.seal();
    st.request_pack();
    let mut steps = 0usize;
    loop {
        let did = st.compact_step();
        steps += did as usize;
        if !did || steps > 400 {
            break;
        }
    }
    println!("pack steps: {steps}, disk={} B", st.disk_bytes());

    // phase E: read cold (fresh store object = cold caches in the same process)
    drop(st);
    let st = Store::open_cfg(&dir.join("region"), true, Cfg::from_env()).unwrap();
    store::read_stage_reset();
    let t = std::time::Instant::now();
    let mut bytes = 0u64;
    for (cx, cz, _p) in &corpus {
        let got = st.read_chunk(key(*cx, *cz)).unwrap();
        bytes += got.len() as u64;
    }
    let wall = t.elapsed();
    println!("E read cold: {:.1} MB/s ({:?})", bytes as f64 / 1e6 / wall.as_secs_f64(), wall);
    println!("  read stages: {}", store::read_stage_json());

    // phase F: read warm
    store::read_stage_reset();
    let t = std::time::Instant::now();
    for (cx, cz, _p) in &corpus {
        let got = st.read_chunk(key(*cx, *cz)).unwrap();
        bytes += got.len() as u64;
    }
    let wall = t.elapsed();
    println!("F read warm: {:.1} MB/s ({:?})", bytes as f64 / 1e6 / wall.as_secs_f64(), wall);
    println!("  read stages: {}", store::read_stage_json());

    drop(st);
    let _ = std::fs::remove_dir_all(&dir);
}
