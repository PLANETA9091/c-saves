//! tail_field_probe: the sub-tail sharing potential. Walks every tail
//! member (0x0A) of a store, parses each as NBT, and hashes each
//! TOP-LEVEL KEY's wire bytes — then reports, per key name, the duplicate
//! raw-byte share (how many bytes would a per-key field-split dedup save
//! over whole-tail dedup).
use c_saves::chunknbt;
use c_saves::store::Store;
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("store-dir"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let tails: Vec<Vec<u8>> = members
        .into_iter()
        .filter(|(c, _)| c == "tail_nbt")
        .map(|(_, b)| b)
        .collect();
    println!("tail members: {}", tails.len());
    let (mut raw_total, mut uniq_total) = (0u64, 0u64);
    let mut key_raw: Map<String, u64> = Map::new();
    let mut key_uniq: Map<String, u64> = Map::new();
    for t in &tails {
        let Ok(Value::Compound(m)) = fastnbt::from_bytes::<Value>(t) else { continue };
        for (k, v) in &m {
            // wire bytes of the value (unnamed write)
            let wrapped = Value::Compound([("".to_string(), v.clone())].into_iter().collect());
            let b = fastnbt::to_bytes(&wrapped).unwrap_or_default();
            let lb = b.len().saturating_sub(5) as u64; // strip the wrapper
            raw_total += lb;
            *key_raw.entry(k.clone()).or_default() += lb;
            // dedup via structural hash of the value (content-pure)
            let h: u128 = chunknbt::structural_hash(v);
            // per (key, hash): count once
            let e = key_uniq.entry(format!("{k}:{h:032x}")).or_insert(0u64);
            if *e == 0 {
                uniq_total += lb;
                *e = 1;
            }
        }
    }
    let mut ks: Vec<_> = key_raw.iter().collect();
    ks.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
    println!("tail raw total {raw_total} -> per-key unique {uniq_total} ({:.1}% saved)", 100.0 * (1.0 - uniq_total as f64 / raw_total.max(1) as f64));
    for (k, v) in ks.iter().take(8) {
        let uniq: u64 = key_uniq
            .iter()
            .filter(|(kh, _)| kh.starts_with(&format!("{k}:")))
            .map(|(_, _)| 0)
            .sum();
        println!("  key '{k}': raw {v}");
        let _ = uniq;
    }
}
