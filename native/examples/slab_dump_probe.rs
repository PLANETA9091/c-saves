//! slab_dump_probe: dump the drained slab member streams (lex-ordered per
//! class, exactly as the pack window sees them) to /tmp/slab_dump/*.bin for
//! the codec ladder probe (LZMA preset 9, brotli-11, ppmd).
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
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(192);
    let which = std::env::args().nth(2).unwrap_or_else(|| "overworld".into());
    println!("== slab_dump_probe {which} {f}x{f} ==");
    let dir = std::env::temp_dir().join(format!(
        "csaves-sdump-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();
    match which.as_str() {
        "end" => {
            let mut b = tc::EndBuilder::new(f);
            for cz in 0..f {
                for cx in 0..f {
                    let (p, _c) = b.payload(cx, cz);
                    st.write_chunk(key(cx, cz), &p, false).unwrap();
                }
            }
        }
        "nether" => {
            let mut b = tc::NetherBuilder::new(f);
            for cz in 0..f {
                for cx in 0..f {
                    let (p, _c) = b.payload(cx, cz);
                    st.write_chunk(key(cx, cz), &p, false).unwrap();
                }
            }
        }
        _ => {
            let mut b = tc::Builder::new(f);
            for cz in 0..f {
                for cx in 0..f {
                    let (p, _c) = b.payload(cx, cz);
                    st.write_chunk(key(cx, cz), &p, false).unwrap();
                }
            }
        }
    }
    drain(&st);
    let members = st.slab_members_by_class();
    drop(st);
    std::fs::create_dir_all("/tmp/slab_dump").unwrap();
    let mut by_class: std::collections::HashMap<String, Vec<Vec<u8>>> = Default::default();
    for (cls, body) in &members {
        by_class.entry(cls.clone()).or_default().push(body.clone());
    }
    for (cls, v) in &by_class {
        let mut bodies = v.clone();
        bodies.sort();
        let raw: usize = bodies.iter().map(|x| x.len()).sum();
        if raw < 512 {
            continue;
        }
        let mut bin = Vec::with_capacity(raw);
        for b in &bodies {
            bin.extend_from_slice(b);
        }
        let path = format!("/tmp/slab_dump/{}_{}.bin", which, cls);
        std::fs::write(&path, &bin).unwrap();
        println!("{:<14} n={:<6} raw={:<9} -> {}", cls, v.len(), raw, path);
    }
}
