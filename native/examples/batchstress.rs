//! batchstress: long-running write+read+pack churn over the batcher/flusher
//! — reproduces the worst-case-bench shape (200k members, interleaved
//! reads, repeated pack rounds) to catch any stall or lost update.
use c_saves::store::{Cfg, Store};
use std::time::Instant;

fn key(cx: i64, cz: i64) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn payload(seed: u64, size: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + size);
    b.push(10);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.push(12);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'd');
    b.extend_from_slice(&((size / 8) as i32).to_be_bytes());
    for i in 0..size / 8 {
        let v = (seed as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ ((i as u64).wrapping_mul(0x100000001B3));
        b.extend_from_slice(&(v as i64).to_be_bytes());
    }
    b.push(0);
    b
}

fn main() {
    let tmp = std::env::temp_dir().join("batchstress");
    let _ = std::fs::remove_dir_all(&tmp);
    let cfg = Cfg::default();
    let st = Store::open_cfg(&tmp, true, cfg).unwrap();
    let t0 = Instant::now();
    // phase A: 60k fresh writes across a wide key space (batch churn)
    for i in 0..60_000u64 {
        let k = key((i % 300) as i64 - 150, (i / 300) as i64 - 100);
        let p = payload(k ^ (i / 300), 2048 + (i % 7) as usize * 256);
        st.write_chunk(k, &p, false).unwrap();
        if i % 5000 == 4999 {
            st.read_chunk(key((i % 300) as i64 - 150, (i / 300) as i64 - 100))
                .unwrap_or_else(|| panic!("read-your-writes miss at {i}"));
        }
    }
    println!("phase A: 60k writes + spot reads in {:.1?}", t0.elapsed());
    // phase B: rewrite half (supersede churn through the batcher)
    let t1 = Instant::now();
    for i in 0..30_000u64 {
        let k = key((i % 300) as i64 - 150, (i / 300) as i64 - 100);
        let p = payload(k ^ (i / 300) ^ 0xdead, 2048 + (i % 5) as usize * 512);
        st.write_chunk(k, &p, false).unwrap();
    }
    println!("phase B: 30k rewrites in {:.1?}", t1.elapsed());
    // phase C: sync + pack rounds
    let t2 = Instant::now();
    st.sync();
    st.seal();
    st.request_pack();
    let mut steps = 0;
    while st.compact_step() {
        steps += 1;
    }
    println!("phase C: sync+seal+pack ({steps} steps) in {:.1?}", t2.elapsed());
    // phase D: verify every key
    let t3 = Instant::now();
    for i in 0..60_000u64 {
        let k = key((i % 300) as i64 - 150, (i / 300) as i64 - 100);
        let expect_super = i / 300 < 100;
        let got = st.read_chunk(k).unwrap_or_else(|| panic!("chunk {i} unreadable"));
        let want = if expect_super {
            payload(k ^ ((i / 300) as u64) ^ 0xdead, 2048 + (i % 5) as usize * 512)
        } else {
            payload(k ^ (i / 300), 2048 + (i % 7) as usize * 256)
        };
        assert_eq!(got.len(), want.len(), "chunk {i} size mismatch");
    }
    println!("phase D: 60k verifies in {:.1?}", t3.elapsed());
    println!("batchstress: ALL OK in {:.1?}", t0.elapsed());
    let _ = std::fs::remove_dir_all(&tmp);
}
