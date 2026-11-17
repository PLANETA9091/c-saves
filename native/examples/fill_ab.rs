//! fill_ab: drained-container A/B for the v0.9.100 uniform fill form.
//! Imports the same .mca corpus twice into two fresh stores — fill-off
//! (CSAVES_GLOB_FILL=0, the 0.9.99 pack shape) and fill-on (default,
//! gform 7/8 members) — drives pack/compaction to completion on both, and
//! reports region disk bytes side by side. Each leg runs in its own
//! process because the fill gate resolves once per process.
//!
//! Usage: fill_ab <dim-dir>

use c_saves::store::{Cfg, Store};
use std::io::Read;
use std::path::{Path, PathBuf};

fn load_mca(path: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
    if parts.len() != 3 {
        return vec![];
    }
    let rx: i32 = parts[1].parse().unwrap_or(0);
    let rz: i32 = parts[2].parse().unwrap_or(0);
    let Ok(data) = std::fs::read(path) else {
        return vec![];
    };
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
        out.push((cx, cz, payload));
    }
    out
}

fn load_corpus(dim: &Path) -> Vec<(i32, i32, Vec<u8>)> {
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

/// Import -> seal -> pack-to-completion -> verified read count + disk bytes.
fn leg(tag: &str, corpus: &[(i32, i32, Vec<u8>)]) -> u64 {
    let dir = std::env::temp_dir().join(format!(
        "csaves-fillab-{}-{}",
        tag.split('(').next().unwrap_or(tag),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    for (cx, cz, p) in corpus {
        st.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    st.sync();
    st.seal();
    let mut steps = 0usize;
    loop {
        let did = st.compact_step();
        steps += did as usize;
        if !did || steps > 400 {
            break;
        }
    }
    let mut verified = 0u64;
    for (cx, cz, p) in corpus {
        let got = st.read_chunk(key(*cx, *cz)).expect("chunk present after drain");
        assert!(!got.is_empty() && got.len() <= p.len().max(64) * 2 + 4096);
        verified += 1;
    }
    let disk = st.disk_bytes();
    println!("{tag}: chunks={} verified={} pack_steps={} disk={} B", st.chunk_count(), verified, steps, disk);
    let viol = st.slab_invariant_scan();
    println!("{tag} invariant violations: {}", viol.len());
    for v in viol.iter().take(5) {
        println!("  {v}");
    }
    drop(st);
    if std::env::var("FILL_AB_KEEP").as_deref() == Ok("1") {
        println!("KEPT_DIR={}", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    disk
}

fn main() {
    let dim = PathBuf::from(std::env::args().nth(1).expect("usage: fill_ab <dim-dir>"));
    // child dispatch: FILL_AB_CHILD=1 runs ONLY the fill-on leg (defaults)
    if std::env::var("FILL_AB_CHILD").as_deref() == Ok("1") {
        let corpus = load_corpus(&dim);
        let disk = leg("fill-on(0x100)", &corpus);
        println!("FILL_DISK={disk}");
        return;
    }

    let corpus = load_corpus(&dim);
    let raw: u64 = corpus.iter().map(|(_, _, p)| p.len() as u64).sum();
    println!("corpus: {} chunks, {:.2} MiB raw", corpus.len(), raw as f64 / 1048576.0);

    // parent: force the fill-OFF shape for this process before anything
    // resolves glob_fill_enabled()
    std::env::set_var("CSAVES_GLOB_FILL", "0");
    let off_disk = leg("fill-off(0.9.99)", &corpus);

    let exe = std::env::current_exe().expect("exe");
    let out = std::process::Command::new(exe)
        .arg(&dim)
        .env("FILL_AB_CHILD", "1")
        .env_remove("CSAVES_GLOB_FILL")
        .output()
        .expect("child run");
    let child_out = String::from_utf8_lossy(&out.stdout);
    let on_disk: u64 = child_out
        .lines()
        .find_map(|l| l.strip_prefix("FILL_DISK=").and_then(|v| v.trim().parse().ok()))
        .unwrap_or_else(|| {
            eprintln!("child stdout:\n{child_out}");
            eprintln!("child stderr:\n{}", String::from_utf8_lossy(&out.stderr));
            panic!("fill child produced no FILL_DISK line");
        });
    for l in child_out
        .lines()
        .filter(|l| l.contains("invariant") || l.starts_with("  "))
    {
        println!("{l}");
    }

    println!("=== A/B drained container ===");
    println!("fill-off (0.9.99 shape): {off_disk} B");
    println!("fill-on  (gform 7/8):    {on_disk} B");
    let d = on_disk as i64 - off_disk as i64;
    let pct = (d as f64) / (off_disk as f64) * 100.0;
    println!("delta: {d} B ({pct:+.2}%)");
}
