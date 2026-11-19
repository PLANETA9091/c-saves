//! gate_census: per-member hashless-GLOB gate census on a store (the
//! hybrid-row win sizing). Modes:
//!   gate_census <store-dir>            — open the store, run the census
//!   gate_census <dim-dir> <out-dir>    — import the .mca corpus first
//! (a dim-dir contains region/*.mca; the import uses the production path
//! and a full drain, exactly like full_world_probe).
use c_saves::store::{Cfg, Store};
use std::path::Path;

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

fn read_region(rx: i32, rz: i32, dir: &Path) -> Vec<(i32, i32, Vec<u8>)> {
    let path = dir.join(format!("r.{}.{}.mca", rx, rz));
    let Ok(data) = std::fs::read(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..1024usize {
        let e = i * 4;
        let off = ((data[e] as usize) << 16) | ((data[e + 1] as usize) << 8) | data[e + 2] as usize;
        let cnt = data[e + 3] as usize;
        if off == 0 || cnt == 0 {
            continue;
        }
        let a = off * 4096;
        if a + 5 > data.len() {
            continue;
        }
        let len = u32::from_be_bytes([data[a], data[a + 1], data[a + 2], data[a + 3]]) as usize;
        if len == 0 || a + 5 + len > data.len() {
            continue;
        }
        let payload = match data[a + 4] {
            2 => {
                let mut d = Vec::with_capacity(len);
                let mut d2 = flate2::read::ZlibDecoder::new(&data[a + 5..a + 5 + len]);
                std::io::Read::read_to_end(&mut d2, &mut d).ok();
                d
            }
            _ => continue,
        };
        let cx = rx * 32 + (i as i32 & 31);
        let cz = rz * 32 + (i as i32 >> 5);
        out.push((cx, cz, payload));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let store_dir = if args.len() >= 3 {
        // import mode: <dim-dir> <out-dir>
        let dim = Path::new(&args[1]);
        let out = Path::new(&args[2]);
        let st = Store::open_cfg(out, true, Cfg::from_env()).unwrap();
        let mut n = 0usize;
        let rdir = dim.join("region");
        let mut region_files: Vec<(i32, i32)> = std::fs::read_dir(&rdir)
            .expect("region dir")
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let p: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
                if p.len() == 3 && p[0] == "r" {
                    Some((p[1].parse::<i32>().ok()?, p[2].parse::<i32>().ok()?))
                } else {
                    None
                }
            })
            .collect();
        region_files.sort();
        for (rx, rz) in region_files {
            for (cx, cz, payload) in read_region(rx, rz, &rdir) {
                let k = ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff);
                st.write_chunk_import(k, &payload).unwrap();
                n += 1;
            }
        }
        drain(&st);
        println!("imported {n} chunks from {}", dim.display());
        drop(st);
        out.to_path_buf()
    } else {
        Path::new(&args[1]).to_path_buf()
    };
    let st = Store::open(&store_dir, true).unwrap();
    println!("store: {}", store_dir.display());
    println!("chunks: {}", st.chunk_count());
    println!("gate: {}", st.glob_hashless_gate_json());
    println!("profile: {}", st.profile_json());
}
