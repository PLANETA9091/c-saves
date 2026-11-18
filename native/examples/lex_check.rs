//! lex_check: does the lex walk actually land in the drained container?
//! Opens a drained store dir, walks slabs, prints per-run class runs,
//! sortedness vs the previous body, and the run boundaries (slab splits).
//!
//! Usage: lex_check <store-dir>

use c_saves::store::Store;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: lex_check <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    println!("total members walked: {}", members.len());
    let mut i = 0usize;
    while i < members.len() {
        let cls = members[i].0.clone();
        let mut j = i;
        while j < members.len() && members[j].0 == cls {
            j += 1;
        }
        let raw: usize = members[i..j].iter().map(|(_, b)| b.len()).sum();
        let sorted = members[i..j].windows(2).all(|w| w[0].1 <= w[1].1);
        let first_bytes: Vec<u8> = members[i..j]
            .iter()
            .filter_map(|(_, b)| b.first().copied())
            .take(8)
            .collect();
        println!(
            "run [{i}..{j}) class={cls} n={} raw={raw} sorted={} heads={:02x?}",
            j - i,
            sorted,
            first_bytes
        );
        i = j;
    }
}
