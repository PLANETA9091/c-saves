//! Offline container integrity dump: reads every chunk, reports failures.
use c_saves::store::{Cfg, Store};

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: dump_container <dir>");
    let st = Store::open_cfg(std::path::Path::new(&dir), true, Cfg::default()).unwrap();
    let keys = st.keys();
    println!("chunks indexed: {}", keys.len());
    let mut ok = 0usize;
    let mut bad = 0usize;
    let mut bad_keys = Vec::new();
    for k in &keys {
        match st.read_chunk(*k) {
            Some(d) if !d.is_empty() => ok += 1,
            other => {
                bad += 1;
                if bad_keys.len() < 10 {
                    bad_keys.push((*k, other.map(|d| d.len())));
                }
            }
        }
    }
    println!("read ok={} bad={}", ok, bad);
    for (k, sz) in &bad_keys {
        let cx = (*k >> 32) as i32;
        let cz = (*k as u32) as i32;
        println!("  BAD chunk ({cx},{cz}) k={k:016x} read={:?}", sz);
        println!("    {}", st.debug_blob_state(*k));
    }
    println!("stats: {}", st.stats_json());
}
