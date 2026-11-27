//! spatial_probe: attribute every deduped slab member of a corpus to the
//! 24-chunk column band that first introduced it. Every band writes, then
//! a full drain (sync+seal+request_pack+compact) makes the slab frames
//! disk-visible; a member-set snapshot diffs the new identities and stamps
//! them with the band coords. Dumps (class, hash, cx, cz, body) so the
//! offline pricing can reorder the SAME member set lex vs spatial and
//! LZMA-price both. Diagnostic only — no store code moves.

use c_saves::store::{Cfg, Store};
use std::hash::{Hash, Hasher};

#[path = "inc/tectonic_corpus.rs"]
mod tc;
use tc::*;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

struct Member {
    class: String,
    hash: u128,
    cx: i32,
    cz: i32,
    body: Vec<u8>,
}

fn drain(st: &Store) {
    st.sync();
    st.seal();
    st.request_pack();
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
}

fn main() {
    let f: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(192);
    let which = std::env::args().nth(2).unwrap_or_else(|| "end".into());
    let band: i32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    println!("== spatial_probe {which} {f}x{f} band={band} ==");
    let dir = std::env::temp_dir().join(format!(
        "csaves-spatial-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.02, ..Cfg::from_env() };
    let st = Store::open_cfg(&dir.join("region"), true, cfg).unwrap();

    let mut members: Vec<Member> = Vec::new();
    // the walk's stored identity hashes are 0 for hashless rows (the tab
    // optimization) — dedup on BODY CONTENT (SipHash 64; collision risk at
    // ~1e3 members is nil) instead.
    let mut seen: std::collections::HashSet<(String, u64)> = Default::default();
    let body_id = |class: &str, body: &[u8]| -> (String, u64) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        class.hash(&mut h);
        body.hash(&mut h);
        (class.to_string(), h.finish())
    };
    let mut written = 0usize;
    let t0 = std::time::Instant::now();

    // one builder for the whole run (stateful cross-chunk structure gen)
    enum B {
        End(EndBuilder),
        Nether(NetherBuilder),
        Over(Builder),
    }
    let mut b = match which.as_str() {
        "end" => B::End(EndBuilder::new(f)),
        "nether" => B::Nether(NetherBuilder::new(f)),
        _ => B::Over(Builder::new(f)),
    };

    for cz in 0..f {
        let mut cx = 0;
        while cx < f {
            let cx1 = (cx + band).min(f);
            for x in cx..cx1 {
                let (p, _c): (Vec<u8>, ()) = match &mut b {
                    B::End(b) => {
                        let (v, _c) = b.payload(x, cz);
                        (v, ())
                    }
                    B::Nether(b) => {
                        let (v, _c) = b.payload(x, cz);
                        (v, ())
                    }
                    B::Over(b) => {
                        let (v, _c) = b.payload(x, cz);
                        (v, ())
                    }
                };
                st.write_chunk(key(x, cz), &p, false).unwrap();
                written += 1;
            }
            cx = cx1;
            // band attribution snapshot: members new after this band belong
            // to it (cx = band start, cz exact)
            drain(&st);
            let walk = st.slab_members_with_hashes();
            if std::env::var("CSAVES_SPATIAL_DEBUG").as_deref() == Ok("1") {
                eprintln!("band ({cx},{cz}): walk={} seen={} members={}", walk.len(), seen.len(), members.len());
            }
            for (class, _stamp, h, body) in walk {
                if seen.insert(body_id(&class, &body)) {
                    members.push(Member { class, hash: h, cx, cz, body });
                }
            }
        }
    }
    let dt = t0.elapsed();
    println!(
        "import {} chunks in {:.1}s, {} unique members attributed",
        written,
        dt.as_secs_f32(),
        members.len()
    );

    let mut by_class: std::collections::BTreeMap<String, Vec<&Member>> = Default::default();
    for m in &members {
        by_class.entry(m.class.clone()).or_default().push(m);
    }
    std::fs::create_dir_all("/tmp/spatial_dump").unwrap();
    for (cls, v) in &by_class {
        let raw: usize = v.iter().map(|m| m.body.len()).sum();
        if raw < 256 {
            continue;
        }
        let mut sorted: Vec<&Member> = v.clone();
        sorted.sort_by(|a, b| a.body.cmp(&b.body));
        let mut bin = Vec::with_capacity(raw);
        let mut idx = Vec::with_capacity(sorted.len() * 4);
        let mut crd = Vec::with_capacity(sorted.len() * 24);
        for m in &sorted {
            bin.extend_from_slice(&m.body);
            idx.extend_from_slice(&(m.body.len() as u32).to_le_bytes());
            crd.extend_from_slice(&m.hash.to_le_bytes());
            crd.extend_from_slice(&(m.cx as i32).to_le_bytes());
            crd.extend_from_slice(&(m.cz as i32).to_le_bytes());
        }
        let path = format!("/tmp/spatial_dump/{}_{}.bin", which, cls);
        std::fs::write(&path, &bin).unwrap();
        std::fs::write(format!("{}.idx", path), &idx).unwrap();
        std::fs::write(format!("{}.crd", path), &crd).unwrap();
        println!("{:<16} n={:<6} raw={:<9} -> {}", cls, v.len(), raw, path);
    }
}
