//! slab_dump: emit the RAW per-class slab member streams (concatenated in
//! lex walk order — the exact bytes the cold LZMA2 stamp would see) to
//! files for offline codec pricing.
//!
//! Usage: slab_dump <store-dir> <out-prefix>
use c_saves::store::Store;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: slab_dump <store-dir> <out-prefix>"));
    let prefix = std::env::args().nth(2).expect("out-prefix");
    let s = Store::open(&dir, true).unwrap();
    // disk-order dump: (class, body) tuples exactly as the walk sees them
    let members = s.slab_members_by_class();
    let mut disk_sec: Vec<u8> = Vec::new();
    let mut disk_tail: Vec<u8> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<Vec<u8>>> = std::collections::HashMap::new();
    for (cls, b) in members {
        if cls == "tail_nbt" || cls == "other" {
            disk_tail.extend_from_slice(&b);
        } else {
            disk_sec.extend_from_slice(&b);
        }
        map.entry(cls).or_default().push(b);
    }
    std::fs::write(format!("{prefix}_disk_sec.bin"), &disk_sec).unwrap();
    std::fs::write(format!("{prefix}_disk_tail.bin"), &disk_tail).unwrap();
    println!("disk_sec: {} B | disk_tail: {} B", disk_sec.len(), disk_tail.len());
    let mut classes: Vec<(String, Vec<Vec<u8>>)> = map.into_iter().collect();
    classes.sort();
    for (cls, mut list) in classes {
        list.sort();
        let raw: usize = list.iter().map(|b| b.len()).sum();
        let stream: Vec<u8> = list.concat();
        let out = PathBuf::from(format!("{prefix}_{cls}.bin"));
        std::fs::write(&out, &stream).unwrap();
        println!("{cls}: n={} raw={raw} -> {}", list.len(), out.display());
    }
}
