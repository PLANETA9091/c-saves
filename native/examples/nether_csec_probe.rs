//! nether_csec_probe: why do the nether's sections stay csec3 (the
//! non-convertible class = 99% of the nether slab mass)? Split a real
//! nether corpus, try section_to_glob per section, classify the refusals.
#[path = "inc/tectonic_corpus.rs"]
mod tc;
use tc::*;

fn main() {
    let f: i32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(48);
    let mut b = NetherBuilder::new(f);
    let mut total = 0usize;
    let mut conv = 0usize;
    let mut no_pre = 0usize;
    let mut no_resolve = 0usize;
    let mut no_finish = 0usize;
    let mut unstable = 0usize;
    let mut palette_sizes: Vec<usize> = Vec::new();
    let mut refused_heads: Vec<usize> = Vec::new();
    let mut refused_kinds: std::collections::HashMap<u8, ()> = std::collections::HashMap::new();
    let mut refused_lens: std::collections::HashMap<usize, i32> = std::collections::HashMap::new();
    let mut reg = c_saves::chunknbt::StateRegistry::new();
    use c_saves::chunknbt::{SecBytes, Split};
    for cz in 0..f {
        for cx in 0..f {
            let (p, _c) = b.payload(cx, cz);
            let Ok(Split::Split { sections, .. }) =
                c_saves::chunknbt::split(&p, false, None, true)
            else {
                continue;
            };
            for (_h, sb) in &sections {
                let csec = match sb {
                    SecBytes::Shared(a) => &**a,
                    SecBytes::Owned(b) => b,
                };
                total += 1;
                let pre = c_saves::chunknbt::section_glob_pre(csec);
                let Some(pre) = pre else {
                    no_pre += 1;
                    if refused_heads.len() < 24 {
                        refused_heads.push(csec.len().min(16));
                    }
                    refused_kinds.entry(csec.first().copied().unwrap_or(0)).or_default();
                    *refused_lens.entry(csec.len() / 64).or_insert(0) += 1;
                    continue;
                };
                let Some(_pgid) = c_saves::chunknbt::section_glob_resolve(&pre, &mut reg) else {
                    no_resolve += 1;
                    continue;
                };
                let Some(g) = c_saves::chunknbt::section_to_glob(csec, &mut reg) else {
                    no_finish += 1;
                    continue;
                };
                // stability gate
                match c_saves::chunknbt::glob_any_to_section(&g, &reg) {
                    Some(direct) => match c_saves::chunknbt::canonical_reencode(&direct) {
                        Some(canon) if &canon == csec => {
                            conv += 1;
                        }
                        _ => unstable += 1,
                    },
                    None => unstable += 1,
                }
            }
        }
    }
    palette_sizes.sort();
    let (p10, p50, p90, pmax) = (
        palette_sizes.get(palette_sizes.len() / 10).copied().unwrap_or(0),
        palette_sizes.get(palette_sizes.len() / 2).copied().unwrap_or(0),
        palette_sizes.get(palette_sizes.len() * 9 / 10).copied().unwrap_or(0),
        palette_sizes.last().copied().unwrap_or(0),
    );
    println!(
        "nether sections total={total} converted={conv} no_pre={no_pre} no_resolve={no_resolve} no_finish={no_finish} unstable={unstable}"
    );
    // store-level check: import the same corpus, drain, print the attribution
    {
        let dir = "/tmp/nether_probe_store";
        let _ = std::fs::remove_dir_all(dir);
        let st = c_saves::store::Store::open_cfg(
            std::path::Path::new(dir),
            true,
            c_saves::store::Cfg::from_env(),
        )
        .unwrap();
        let mut b2 = NetherBuilder::new(f);
        let mut k = 0u64;
        for cz in 0..f {
            for cx in 0..f {
                let (p, _c) = b2.payload(cx, cz);
                st.write_chunk_import(k, &p).unwrap();
                k += 1;
            }
        }
        st.sync();
        st.seal();
        st.request_pack();
        for _ in 0..8 {
            while st.compact_step() {}
        }
        st.sync();
        println!("nether store attribution: {}", st.slab_member_attribution_json());
        println!("nether store profile: {}", st.profile_json());
        drop(st);
        let _ = std::fs::remove_dir_all(dir);
    }
    println!("no_pre palette sizes: p10={p10} p50={p50} p90={p90} max={pmax}");
    println!("refused kinds: {:?} first-heads {:?} len/64 hist {:?}",
        refused_kinds.keys().collect::<Vec<_>>(), refused_heads, refused_lens);
}
