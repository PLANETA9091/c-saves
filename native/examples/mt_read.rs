//! Multi-threaded read stress: reproduces server-like concurrent IO.
use c_saves::store::{Cfg, Store};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: mt_read <dir> [threads] [secs]");
    let threads: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let st = Store::open_cfg(std::path::Path::new(&dir), true, Cfg::default()).unwrap();
    let keys = Arc::new(st.keys());
    println!("chunks: {}, threads: {}, {}s", keys.len(), threads, secs);
    let reads = Arc::new(AtomicU64::new(0));
    let bad = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for t in 0..threads {
        let st = st.clone();
        let keys = keys.clone();
        let reads = reads.clone();
        let bad = bad.clone();
        hs.push(std::thread::spawn(move || {
            let mut i = t as u64;
            loop {
                let k = keys[(i as usize) % keys.len()];
                match st.read_chunk(k) {
                    Some(d) if !d.is_empty() => {}
                    _ => { bad.fetch_add(1, Ordering::Relaxed); }
                }
                reads.fetch_add(1, Ordering::Relaxed);
                i += 1;
                if i % 1000 == 0 {
                    // small pause so GC-ish patterns appear
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            }
        }));
    }
    let t0 = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let r = reads.load(Ordering::Relaxed);
    println!("reads: {} ({:.0}/s), bad: {}, elapsed {:.1}s", r, r as f64 / t0.elapsed().as_secs_f64(), bad.load(Ordering::Relaxed), t0.elapsed().as_secs_f64());
    std::process::exit(0);
}
