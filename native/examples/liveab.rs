//! liveab: read-your-writes latency A/B — the paper live pattern.
//! For each iteration: write a UNIQUE synthetic chunk (33 KB whole-mode
//! blob) to the active writer segment, then immediately read it back — the
//! read hits a never-before-read blob whose record is still sitting in the
//! writer's append buffer, i.e. exactly the path where the old code flushed
//! the whole buffered tail (seek + write_all of up to BUF_CAP under the
//! writer mutex) before every read, and 0.9.70 answers from the buffer /
//! preads over already-written bytes instead.
use c_saves::store::Store;
use std::time::Instant;

fn key(cx: i64, cz: i64) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }

/// Valid NBT root compound: { "d": LongArray(4096) } — ~33 KB, content
/// unique per (key, round) so no dedup can collapse the writes.
fn payload(seed: u64) -> Vec<u8> {
    let n = 4096usize;
    let mut b = Vec::with_capacity(16 + n * 8);
    b.push(10); b.extend_from_slice(&0u16.to_be_bytes()); // compound ""
    b.push(12); b.extend_from_slice(&1u16.to_be_bytes()); // long array "d"
    b.extend_from_slice(&(n as i32).to_be_bytes());
    for i in 0..n {
        let v = (seed as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ ((i as u64).wrapping_mul(0x100000001B3));
        b.extend_from_slice(&(v as i64).to_be_bytes());
    }
    b.push(0); // TAG_End
    b
}

fn pct(v: &mut Vec<u64>, p: f64) -> u64 {
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i.min(v.len() - 1)]
}

fn main() {
    let iters = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(400usize);
    let tmp = std::env::temp_dir().join("liveab-store");
    let _ = std::fs::remove_dir_all(&tmp);
    let st = Store::open_cfg(&tmp, true, c_saves::store::Cfg::default()).unwrap();
    st.set_whole_mode();

    // measured: write->read pairs on unique payloads
    let mut rd: Vec<u64> = Vec::with_capacity(iters * 3);
    let mut wr: Vec<u64> = Vec::with_capacity(iters * 3);
    let mut ok = 0u64;
    let total = iters * 3;
    for it in 0..total {
        let k = key((it % 61) as i64 - 30, (it / 61) as i64 - 30);
        let p = payload(k ^ ((it / 61 * 61 + 7) as u64));
        let t0 = Instant::now();
        st.write_chunk(k, &p, true).unwrap();
        wr.push(t0.elapsed().as_nanos() as u64);
        let t1 = Instant::now();
        if st.read_chunk(k).as_deref() == Some(p.as_slice()) { ok += 1; }
        rd.push(t1.elapsed().as_nanos() as u64);
        let _ = it;
    }
    let bufread = c_saves::store::ST_BUFREAD.0.load(std::sync::atomic::Ordering::Relaxed);
    let bufbytes = c_saves::store::ST_BUFREAD.1.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "liveab: correct={ok}/{total} rd p50={} p90={} p99={} max={} us | wr p50={} p90={} p99={} max={} us | bufread reads={bufread} bytes={bufbytes}",
        pct(&mut rd, 0.50) / 1000,
        pct(&mut rd, 0.90) / 1000,
        pct(&mut rd, 0.99) / 1000,
        rd.last().copied().unwrap_or(0) / 1000,
        pct(&mut wr, 0.50) / 1000,
        pct(&mut wr, 0.90) / 1000,
        pct(&mut wr, 0.99) / 1000,
        wr.last().copied().unwrap_or(0) / 1000,
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
