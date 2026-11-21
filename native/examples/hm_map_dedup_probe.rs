//! hm_map_dedup_probe: within-tail heightmap map identity. Each tail's
//! Heightmaps compound carries N maps (LongArray-repacked ByteArrays). If
//! maps are byte-identical WITHIN a tail (end: no water => MOTION_BLOCKING
//! == WORLD_SURFACE == OCEAN_FLOOR), a map-level dedup (store each distinct
//! map once per tail, refs for the rest) halves the heightmap raw. This
//! probe counts distinct maps per tail, the raw that map-level dedup saves
//! (within-tail AND cross-tail content dedup), and re-prices the tail
//! stream with dedup'd maps.
//!
//! Usage: hm_map_dedup_probe <store-dir>

use c_saves::store::Store;
use fastnbt::Value;
use liblzma::stream as xs;
use std::collections::HashMap as Map;
use std::path::PathBuf;

fn lzma2_raw(data: &[u8]) -> Option<usize> {
    let mut opts =
        liblzma::stream::LzmaOptions::new_preset(liblzma::stream::PRESET_EXTREME | 6).ok()?;
    opts.literal_context_bits(4)
        .literal_position_bits(0)
        .position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, xs::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, xs::Action::Finish).ok()?;
    Some(out.len())
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: hm_map_dedup_probe <store-dir>"));
    let s = Store::open(&dir, true).unwrap();
    let members = s.slab_members_by_class();
    let tails: Vec<Vec<u8>> = members
        .into_iter()
        .filter(|(c, _)| c == "tail_nbt")
        .map(|(_, b)| b)
        .collect();
    println!("tail members: {}", tails.len());

    let mut tails_with_hm = 0usize;
    let mut map_total = 0usize;
    let mut distinct_in_tail = 0usize;
    let mut hm_raw_total = 0u64;
    let mut hm_dedup_tail_raw = 0u64; // within-tail dedup
    let mut map_names: Map<String, usize> = Map::new();
    // cross-tail map content dedup: hash -> first raw
    let mut seen: Map<u128, Vec<u8>> = Map::new();
    let mut cross_unique_raw = 0u64;

    let mut base_stream = Vec::new();
    let mut dedup_stream = Vec::new();

    for t in &tails {
        let Ok(Value::Compound(m)) = fastnbt::from_bytes::<Value>(t) else {
            continue;
        };
        let Some(Value::Compound(hm)) = m.get("Heightmaps") else {
            continue;
        };
        tails_with_hm += 1;
        let mut per_tail: Map<Vec<u8>, ()> = Map::new();
        let mut rebuilt = std::collections::HashMap::new();
        for (mk, mv) in hm {
            *map_names.entry(mk.clone()).or_default() += 1;
            if let Value::ByteArray(arr) = mv {
                let raw: Vec<u8> = arr.iter().map(|x| *x as u8).collect();
                map_total += 1;
                hm_raw_total += raw.len() as u64;
                per_tail.entry(raw.clone()).or_default();
                // cross-tail content hash (xxh3_128 via the store's hash fn
                // is internal; use a simple fold for census purposes)
                let h = content_hash(&raw);
                if !seen.contains_key(&h) {
                    seen.insert(h, raw.clone());
                    cross_unique_raw += raw.len() as u64;
                }
            }
            rebuilt.insert(mk.clone(), mv.clone());
        }
        distinct_in_tail += per_tail.len();
        hm_dedup_tail_raw += per_tail.keys().map(|k| k.len()).sum::<usize>() as u64;
        // rebuild baseline tail bytes + a map-dedup'd variant (store each
        // distinct map once under its first name, other names -> 1-byte ref
        // marker 0xC5 + varint index-of-first)
        let base = Value::Compound(m.clone());
        if let Ok(b) = fastnbt::to_bytes(&base) {
            base_stream.extend_from_slice(&b);
        }
        let mut nh = std::collections::HashMap::new();
        let mut first_of: Map<Vec<u8>, String> = Map::new();
        for (mk, mv) in hm {
            if let Value::ByteArray(arr) = mv {
                let raw: Vec<u8> = arr.iter().map(|x| *x as u8).collect();
                match first_of.get(&raw) {
                    Some(_) => {
                        // duplicate map: name + single ref byte 0xC5
                        nh.insert(
                            mk.clone(),
                            Value::ByteArray(fastnbt::ByteArray::new(vec![0xC5u8 as i8])),
                        );
                    }
                    None => {
                        first_of.insert(raw.clone(), mk.clone());
                        nh.insert(mk.clone(), mv.clone());
                    }
                }
            } else {
                nh.insert(mk.clone(), mv.clone());
            }
        }
        let mut nm = m.clone();
        nm.insert("Heightmaps".to_string(), Value::Compound(nh));
        if let Ok(b) = fastnbt::to_bytes(&Value::Compound(nm)) {
            dedup_stream.extend_from_slice(&b);
        }
    }
    println!("tails-with-hm {tails_with_hm}, maps {map_total}, distinct-within-tail {distinct_in_tail}");
    println!("map names: {:?}", map_names);
    println!(
        "hm raw {hm_raw_total} -> within-tail dedup {hm_dedup_tail_raw} ({:.1}%) -> cross-tail unique {cross_unique_raw} ({:.1}%)",
        100.0 * hm_dedup_tail_raw as f64 / hm_raw_total as f64,
        100.0 * cross_unique_raw as f64 / hm_raw_total as f64
    );
    let base_c = lzma2_raw(&base_stream).unwrap();
    let dedup_c = lzma2_raw(&dedup_stream).unwrap();
    println!("baseline lzma2 {base_c}");
    println!("map-dedup lzma2 {dedup_c}  ({:.2}%)", 100.0 * dedup_c as f64 / base_c as f64);
}

fn content_hash(b: &[u8]) -> u128 {
    // cheap census hash (fnv-1a 128-ish); NOT the store's xxh3
    let mut h0 = 0xcbf29ce484222325u64;
    let mut h1 = 0x9e3779b97f4a7c15u64;
    for (i, x) in b.iter().enumerate() {
        h0 ^= *x as u64;
        h0 = h0.wrapping_mul(0x100000001b3);
        h1 = (h1 ^ (*x as u64).rotate_left((i % 8) as u32)).wrapping_mul(0x100000001b3);
    }
    ((h0 as u128) << 64) | h1 as u128
}
