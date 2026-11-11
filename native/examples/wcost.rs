//! wcost: where does the worst-case verify's ~22 ms/chunk go?
//! Times the suspects separately on the real probe path:
//! wgen::chunk_payload (expected-content worldgen), read_chunk (container
//! read), fastnbt parse + canonical_contract_hash (the comparison).
use c_saves::chunknbt::canonical_contract_hash;
use c_saves::store::{Cfg, Store};
use std::time::Instant;

mod wgen;

fn main() {
    let keys_n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(64);
    let world = wgen::World::new(0x5EED);
    let dir = std::env::temp_dir().join("wcost-store");
    let _ = std::fs::remove_dir_all(&dir);
    let st = Store::open_cfg(&dir, false, Cfg::default()).unwrap();
    st.set_whole_mode();
    let mut keys = Vec::with_capacity(keys_n);
    let mut payloads = Vec::with_capacity(keys_n);
    for i in 0..keys_n {
        let cx = (i as i32 % 16) - 8;
        let cz = (i as i32 / 16) - 4;
        let p = wgen::chunk_payload(&world, cx, cz, 1);
        keys.push((((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)));
        payloads.push(p);
    }
    for (i, p) in payloads.iter().enumerate() {
        st.write_chunk(keys[i], p, true).unwrap();
    }
    st.sync(); st.seal(); st.request_pack();
    while st.compact_step() {}
    st.sync();

    // 1) expected-content worldgen (the verify regenerates it per key)
    let t0 = Instant::now();
    let mut gen_bytes = 0u64;
    let mut gen_pay: Vec<Vec<u8>> = Vec::with_capacity(keys_n);
    for &k in &keys {
        let cx = (k >> 32) as i32;
        let cz = (k & 0xffff_ffff) as u32 as i32;
        let p = wgen::chunk_payload(&world, cx, cz, 1);
        gen_bytes += p.len() as u64;
        gen_pay.push(p);
    }
    let t_gen = t0.elapsed();
    // 2) container read (warm cache)
    let t0 = Instant::now();
    let mut read_bytes = 0u64;
    let mut reads: Vec<Vec<u8>> = Vec::with_capacity(keys_n);
    for &k in &keys {
        reads.push(st.read_chunk(k).unwrap());
    }
    for r in &reads { read_bytes += r.len() as u64; }
    let t_read = t0.elapsed();
    // 3) canonical hash of both sides + compare (the probe's equality)
    let t0 = Instant::now();
    let mut matched = 0usize;
    for i in 0..keys_n {
        let eh = canonical_contract_hash(&gen_pay[i]).expect("expect parses");
        let gh = canonical_contract_hash(&reads[i]).expect("stored parses");
        if eh == gh { matched += 1; }
    }
    let t_sem = t0.elapsed();
    let n = keys_n as f64;
    println!(
        "wcost n={keys_n}: worldgen {:.2?} ({:.2} ms/chunk, {:.1} KiB/chunk) | read {:.2?} ({:.3} ms/chunk, {:.1} KiB/chunk) | hash-cmp {:.2?} ({:.2} ms/chunk) | matched={matched}/{keys_n}",
        t_gen, t_gen.as_secs_f64() * 1000.0 / n, gen_bytes as f64 / 1024.0 / n,
        t_read, t_read.as_secs_f64() * 1000.0 / n, read_bytes as f64 / 1024.0 / n,
        t_sem, t_sem.as_secs_f64() * 1000.0 / n,
    );
    let _ = std::fs::remove_dir_all(&dir);
}
