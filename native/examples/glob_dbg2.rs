//! glob_dbg2: the DEFINITIVE drift localization. Rebuild the corpus
//! payload for chunk (0,0) (deterministic builder), split it exactly like
//! the import path, print each section's canon hash + wire bytes; then
//! open the store and print the identity hashes the slab tabs carry for
//! that chunk's sections.
#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::chunknbt::{self, Split};
use c_saves::store::Store;
use std::path::PathBuf;

fn main() {
    let store = PathBuf::from(std::env::args().nth(1).expect("store-dir"));
    // 1) the fresh payload + split
    let mut b = tc::Builder::new(96);
    let (payload, _) = b.payload(0, 0);
    let split = chunknbt::split(&payload, false, Some((0, 0)), true).unwrap();
    let Split::Split { sections, tail, .. } = split else {
        panic!("no split");
    };
    println!("chunk (0,0): {} sections, tail {} B", sections.len(), tail.len());
    for (h, sec) in sections.iter().take(6) {
        let sb = sec.as_slice();
        let kind = sb.first().map(|b| format!("{b:02x}")).unwrap_or_default();
        println!("  sec hash={h:032x} kind={kind} len={}", sb.len());
    }
    // 2) the store's tab identities for hashed glob slabs
    let s = Store::open(&store, true).unwrap();
    println!("slabs: {}", s.slab_tab_head_json());

    let members = s.slab_members_with_hashes();
    let mut shown = 0;
    for (cls, stamp, stored, body) in members.iter() {
        if cls.starts_with("glob_g") && shown < 8 {
            let reg = s.glob_reg_snapshot();
            let direct = chunknbt::glob_any_to_section(body, &reg);
            let re = direct.as_ref().and_then(|d| chunknbt::canonical_reencode(d));
            let rh = re.as_ref().map(|c| chunknbt::hash_of(c));
            let ok = rh.map(|r| r == *stored).unwrap_or(false);
            println!(
                "  member cls={cls} stamp={stamp:02x} len={} stored={stored:032x} rehash={rh:032x?} match={ok}",
                body.len()
            );
            shown += 1;
        }
    }
}
