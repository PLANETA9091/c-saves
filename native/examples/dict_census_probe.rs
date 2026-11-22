//! dict_census_probe: WHY do the dict files survive the close (the ~27 KB
//! dicts/states remainder across the three dims)? tail_census_json on the
//! DRAINED store + after a reopen: which dict versions live, who names
//! them (inline tail refs vs blobs vs cur), and what the next-open state is.
use c_saves::store::{Cfg, Store};

#[path = "inc/tectonic_corpus.rs"]
mod tc;
use tc::*;

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let f: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(96);
    let which = std::env::args().nth(2).unwrap_or_else(|| "end".into());
    let dir = std::env::temp_dir().join(format!(
        "csaves-dcens-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    match which.as_str() {
        "end" => {
            let mut b = tc::EndBuilder::new(f);
            for cz in 0..f { for cx in 0..f {
                let (p, _c) = b.payload(cx, cz);
                st.write_chunk(key(cx, cz), &p, false).unwrap();
            }}
        }
        "nether" => {
            let mut b = tc::NetherBuilder::new(f);
            for cz in 0..f { for cx in 0..f {
                let (p, _c) = b.payload(cx, cz);
                st.write_chunk(key(cx, cz), &p, false).unwrap();
            }}
        }
        _ => {
            let mut b = tc::Builder::new(f);
            for cz in 0..f { for cx in 0..f {
                let (p, _c) = b.payload(cx, cz);
                st.write_chunk(key(cx, cz), &p, false).unwrap();
            }}
        }
    }
    drain(&st);
    println!("drained tail_census: {}", st.tail_census_json());
    let ddir = dir.join("region").join("dicts");
    if let Ok(rd) = std::fs::read_dir(&ddir) {
        for e in rd.flatten() {
            println!("  dict file: {} ({} B)", e.file_name().to_string_lossy(), e.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    drop(st);
    // reopen: what does the reopened store see, and what survives on disk?
    let st2 = Store::open_cfg(&dir.join("region"), true, Cfg::from_env()).unwrap();
    println!("reopened tail_census: {}", st2.tail_census_json());
    drop(st2);
    if let Ok(rd) = std::fs::read_dir(&ddir) {
        for e in rd.flatten() {
            println!("  dict file after reopen-drop: {} ({} B)", e.file_name().to_string_lossy(), e.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
}
