//! nether_store_probe: classify the csec3 members of a REAL drained nether
//! store — why did each stay non-converted?
//!
//! Usage: nether_store_probe <store-dir>
use c_saves::chunknbt;
use c_saves::store::Store;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage"));
    let st = Store::open(&dir, true).unwrap();
    let members = st.slab_members_by_class();
    {
        let mut hist: std::collections::HashMap<&String, (usize, usize)> = std::collections::HashMap::new();
        for (cls, body) in &members {
            let e = hist.entry(cls).or_insert((0, 0));
            e.0 += 1;
            e.1 += body.len();
        }
        let mut v: Vec<_> = hist.iter().collect();
        v.sort_by_key(|(k, _)| (*k).clone());
        for (k, (n, r)) in &v {
            println!("class {k}: n={n} raw={r}");
        }
    }
    let mut reg = chunknbt::StateRegistry::new();
    let (mut void, mut rle_fail, mut dense_fail, mut conv_ok, mut unstable, mut other, mut shift) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut n_hist: Vec<usize> = Vec::new();
    for (cls, body) in &members {
        if cls == "shift" || cls.starts_with("shift") {
            shift += 1;
            continue;
        }
        if cls != "csec3" {
            continue;
        }
        // void members: 4-byte csec3 with f1=0
        if body.len() == 4 {
            void += 1;
            continue;
        }
        match chunknbt::section_glob_pre(body) {
            None => {
                // classify by f1/f2
                let f1 = body.get(1).copied().unwrap_or(0);
                let f2 = body.get(2).copied().unwrap_or(0);
                if f2 & 48 != 0 {
                    rle_fail += 1;
                } else {
                    dense_fail += 1;
                }
                let c4 = body[0] == 0xC8;
                let off = if c4 { 5 } else { 4 };
                if body.len() >= off + 2 {
                    n_hist.push(u16::from_le_bytes([body[off], body[off + 1]]) as usize);
                }
                let _ = f1;
            }
            Some(pre) => {
                let Some(_pgid) = chunknbt::section_glob_resolve(&pre, &mut reg) else {
                    other += 1;
                    continue;
                };
                match chunknbt::section_to_glob(body, &mut reg) {
                    None => other += 1,
                    Some(g) => match chunknbt::glob_any_to_section(&g, &reg) {
                        Some(direct) => match chunknbt::canonical_reencode(&direct) {
                            Some(canon) if &canon == body => conv_ok += 1,
                            _ => unstable += 1,
                        },
                        None => unstable += 1,
                    },
                }
            }
        }
    }
    n_hist.sort();
    println!(
        "csec3 members: void={void} rle_fail={rle_fail} dense_fail={dense_fail} conv_ok={conv_ok} unstable={unstable} other={other} shift={shift}"
    );
    if !n_hist.is_empty() {
        println!(
            "refused palette n: p10={} p50={} p90={} max={}",
            n_hist[n_hist.len() / 10],
            n_hist[n_hist.len() / 2],
            n_hist[n_hist.len() * 9 / 10],
            n_hist[n_hist.len() - 1]
        );
    }
    drop(st);
}
