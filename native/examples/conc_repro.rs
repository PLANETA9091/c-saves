//! conc_repro: the paper-bench failure mode — the MAINTENANCE pack running
//! CONCURRENTLY with live-path writes (Moonrise I/O threads + the
//! maintenance thread), then close, reopen, read-everything. 3 generations.
use c_saves::store::{Cfg, Store};
use std::path::PathBuf;
use std::sync::Arc;

#[path = "inc/tectonic_corpus.rs"]
mod tc;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let dir = std::path::PathBuf::from("/tmp/conc_repro_store");
    let _ = std::fs::remove_dir_all(&dir);
    let st: Arc<Store> = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();

    // pre-generate the payload pool (deterministic)
    let f = 40i32;
    let mut b = tc::Builder::new(f);
    let mut pool: Vec<(u64, Vec<u8>)> = Vec::new();
    for cz in 0..f {
        for cx in 0..f {
            let (p, _) = b.payload(cx, cz);
            pool.push((key(cx, cz), p));
        }
    }
    // extra chunks for later generations
    let mut pool2: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut b2 = tc::Builder::new(24);
    for cz in 0..24i32 {
        for cx in 0..24i32 {
            let (p, _) = b2.payload(cx + 500, cz + 500);
            pool2.push((key(cx + 500, cz + 500), p));
        }
    }

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // the MAINTENANCE thread: packs sealed material continuously
    let st_m = Arc::clone(&st);
    let stop_m = Arc::clone(&stop);
    let maint = std::thread::spawn(move || {
        while !stop_m.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = st_m.compact_step();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });

    // writer threads (Moonrise I/O semantics: 2 threads, live path)
    let mut writers = Vec::new();
    for t in 0..2usize {
        let st_w = Arc::clone(&st);
        let pool_w = pool.clone();
        writers.push(std::thread::spawn(move || {
            for (i, (k, p)) in pool_w.iter().enumerate() {
                if i % 2 == t {
                    st_w.write_chunk(*k, p, false).unwrap();
                }
            }
        }));
    }
    for w in writers {
        w.join().unwrap();
    }
    println!("phase 1 written: {}", st.chunk_count());

    // generation loop: more live writes WHILE the maintenance packs, then a
    // graceful close + reopen + full read sweep
    let mut gen_st = st;
    for gen in 0..3usize {
        let stop_g = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let st_m2 = Arc::clone(&gen_st);
        let stop_m2 = Arc::clone(&stop_g);
        let maint2 = std::thread::spawn(move || {
            while !stop_m2.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = st_m2.compact_step();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let mut ws = Vec::new();
        for t in 0..2usize {
            let st_w = Arc::clone(&gen_st);
            let pool_w = pool2.clone();
            let pool_o = pool.clone();
            ws.push(std::thread::spawn(move || {
                for (i, (k, p)) in pool_o.iter().enumerate() {
                    if i % 3 == (gen + t) % 3 {
                        st_w.write_chunk(*k, p, false).unwrap(); // relight rewrite
                    }
                }
                for (i, (k, p)) in pool_w.iter().enumerate() {
                    if i % 2 == t {
                        st_w.write_chunk(*k, p, false).unwrap();
                    }
                }
            }));
        }
        for w in ws {
            w.join().unwrap();
        }
        stop_g.store(true, std::sync::atomic::Ordering::Relaxed);
        maint2.join().unwrap();
        gen_st.sync();
        gen_st.seal();
        gen_st.sync();
        let disk = gen_st.disk_bytes();
        drop(gen_st);

        let st2 = Store::open_cfg(&dir, true, Cfg::from_env()).unwrap();
        let inv = st2.slab_invariant_scan();
        let (mut ok, mut miss) = (0usize, 0usize);
        for (k, _) in pool.iter().chain(pool2.iter()) {
            match st2.read_chunk(*k) {
                Some(d) if !d.is_empty() => ok += 1,
                _ => miss += 1,
            }
        }
        println!(
            "gen {gen}: reopen chunks={} invariant={} read ok={ok} miss={miss} disk={disk} B",
            st2.chunk_count(),
            inv.len()
        );
        if miss > 0 {
            println!("=== conc_repro: FAIL at gen {gen} — {} chunks lost ===", miss);
            std::process::exit(1);
        }
        gen_st = st2;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = maint.join();
    drop(gen_st);
    println!("=== conc_repro: PASS ===");
}
