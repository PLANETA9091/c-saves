//! Chunk NBT split/join: the dedup heart of c-saves.
//!
//! A Paper 26.3 chunk payload (uncompressed NBT, `NbtIo.write` form) is split
//! into:
//!   * `sections` — per-section compounds (block_states/biomes/light).
//!                  Position-free => massively shared across chunks. Each is
//!                  addressed by a STRUCTURAL hash (a canonical, order-free
//!                  digest of the NBT tree) so identical sections dedup even
//!                  though fastnbt's serialization order is nondeterministic.
//!   * tail       — everything else (Heightmaps, block_entities,
//!                  DataVersion, ...), stored compressed in the recipe.
//!
//! Entity/POI payloads have no `sections`; they are stored whole as a single
//! blob (identical empty entity chunks cost one shared blob).

use fastnbt::Value;
use xxhash_rust::xxh3::{xxh3_128, Xxh3};

pub type Hash = u128;

/// A split payload, carrying the canonical section bytes so the writer never
/// parses the payload twice.
#[derive(Debug, Clone)]
pub enum Split {
    Whole {
        hash: Hash,
    },
    Split {
        /// (structural hash, serialized NBT bytes) per section, in order.
        sections: Vec<(Hash, Vec<u8>)>,
        tail: Vec<u8>,
    },
}

pub fn to_bytes(v: &Value) -> Vec<u8> {
    fastnbt::to_bytes(v).unwrap_or_default()
}

pub fn hash_of(payload: &[u8]) -> Hash {
    xxh3_128(payload)
}

/// Order-free structural digest of an NBT value. Compounds are fed in sorted
/// key order; strings/types/lengths are mixed in, so any semantic difference
/// changes the hash and any re-serialization keeps it stable.
pub fn structural_hash(v: &Value) -> Hash {
    let mut h = Xxh3::new();
    feed(&mut h, v);
    h.digest128()
}

macro_rules! w {
    ($h:expr, $b:expr) => {
        $h.update($b)
    };
}

fn feed(h: &mut Xxh3, v: &Value) {
    match v {
        Value::Byte(x) => {
            w!(h, &[1]);
            w!(h, &x.to_le_bytes());
        }
        Value::Short(x) => {
            w!(h, &[2]);
            w!(h, &x.to_le_bytes());
        }
        Value::Int(x) => {
            w!(h, &[3]);
            w!(h, &x.to_le_bytes());
        }
        Value::Long(x) => {
            w!(h, &[4]);
            w!(h, &x.to_le_bytes());
        }
        Value::Float(x) => {
            w!(h, &[5]);
            w!(h, &x.to_bits().to_le_bytes());
        }
        Value::Double(x) => {
            w!(h, &[6]);
            w!(h, &x.to_bits().to_le_bytes());
        }
        Value::ByteArray(x) => {
            w!(h, &[7]);
            w!(h, &(x.len() as u64).to_le_bytes());
            // ByteArray derefs to [i8]; hash the raw little-endian bytes.
            for b in x.as_ref() {
                w!(h, &(*b as u8).to_le_bytes());
            }
        }
        Value::String(s) => {
            w!(h, &[8]);
            w!(h, &(s.len() as u64).to_le_bytes());
            w!(h, s.as_bytes());
        }
        Value::List(l) => {
            w!(h, &[9]);
            w!(h, &(l.len() as u64).to_le_bytes());
            for i in l {
                feed(h, i);
            }
        }
        Value::Compound(m) => {
            w!(h, &[10]);
            w!(h, &(m.len() as u64).to_le_bytes());
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                w!(h, &(k.len() as u64).to_le_bytes());
                w!(h, k.as_bytes());
                feed(h, &m[k]);
            }
        }
        Value::IntArray(x) => {
            w!(h, &[11]);
            w!(h, &(x.len() as u64).to_le_bytes());
            for i in x.as_ref() {
                w!(h, &i.to_le_bytes());
            }
        }
        Value::LongArray(x) => {
            w!(h, &[12]);
            w!(h, &(x.len() as u64).to_le_bytes());
            for i in x.as_ref() {
                w!(h, &i.to_le_bytes());
            }
        }
    }
}

/// Split an uncompressed chunk payload into a recipe.
pub fn split(payload: &[u8], whole_mode: bool) -> Result<Split, String> {
    if whole_mode {
        return Ok(Split::Whole { hash: hash_of(payload) });
    }
    let root: Value = fastnbt::from_bytes(payload).map_err(|e| format!("nbt parse: {e}"))?;
    let Value::Compound(map) = &root else {
        return Ok(Split::Whole { hash: hash_of(payload) });
    };
    let Some(Value::List(list)) = map.get("sections") else {
        return Ok(Split::Whole { hash: hash_of(payload) });
    };

    let mut sections = Vec::with_capacity(list.len());
    for s in list {
        let bytes = to_bytes(s);
        sections.push((structural_hash(s), bytes));
    }

    let mut tail_map = map.clone();
    tail_map.remove("sections");
    let tail = to_bytes(&Value::Compound(tail_map));

    Ok(Split::Split { sections, tail })
}

/// Reassemble the uncompressed payload from a split recipe.
pub fn join(sections: &[Vec<u8>], tail: &[u8]) -> Result<Vec<u8>, String> {
    let tail_val: Value = fastnbt::from_bytes(tail).map_err(|e| format!("tail parse: {e}"))?;
    let Value::Compound(mut map) = tail_val else {
        return Err("tail is not a compound".into());
    };
    let mut list = Vec::with_capacity(sections.len());
    for s in sections {
        let v: Value = fastnbt::from_bytes(s).map_err(|e| format!("section parse: {e}"))?;
        list.push(v);
    }
    map.insert("sections".to_string(), Value::List(list));
    Ok(to_bytes(&Value::Compound(map)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn sample_chunk(sections: usize, extra_key: &str) -> Vec<u8> {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert(extra_key.to_string(), Value::Byte(1));
        let mut sec_list = Vec::new();
        for i in 0..sections {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(i as i8));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = Vec::new();
            pal.push(Value::String("minecraft:stone".to_string()));
            bs.insert("palette".to_string(), Value::List(pal));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        let mut be = Vec::new();
        be.push(Value::String("x".to_string()));
        root.insert("block_entities".to_string(), Value::List(be));
        to_bytes(&Value::Compound(root))
    }

    #[test]
    fn roundtrip_semantic_equality() {
        let payload = sample_chunk(6, "Test");
        let s = split(&payload, false).unwrap();
        let Split::Split { sections, tail } = &s else { panic!("expected split") };
        assert_eq!(sections.len(), 6);
        let sec_bytes: Vec<Vec<u8>> = sections.iter().map(|(_, b)| b.clone()).collect();
        let joined = join(&sec_bytes, tail).unwrap();
        let a: Value = fastnbt::from_bytes(&payload).unwrap();
        let b: Value = fastnbt::from_bytes(&joined).unwrap();
        assert_eq!(a, b, "semantic round-trip must be equal");
    }

    #[test]
    fn dedup_identical_sections_across_chunks() {
        let s1 = split(&sample_chunk(8, "T"), false).unwrap();
        let s2 = split(&sample_chunk(8, "T"), false).unwrap();
        let Split::Split { sections: a, .. } = &s1 else { panic!() };
        let Split::Split { sections: b, .. } = &s2 else { panic!() };
        let ha: Vec<Hash> = a.iter().map(|(h, _)| *h).collect();
        let hb: Vec<Hash> = b.iter().map(|(h, _)| *h).collect();
        assert_eq!(ha, hb, "identical chunks must share structural section hashes");
    }

    #[test]
    fn structural_hash_is_order_free() {
        let mut m1: Map<String, Value> = Map::new();
        m1.insert("a".into(), Value::Int(1));
        m1.insert("b".into(), Value::Int(2));
        let h1 = structural_hash(&Value::Compound(m1));
        let mut m2: Map<String, Value> = Map::new();
        m2.insert("b".into(), Value::Int(2));
        m2.insert("a".into(), Value::Int(1));
        let h2 = structural_hash(&Value::Compound(m2));
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_content_different_hash() {
        let mut m1: Map<String, Value> = Map::new();
        m1.insert("a".into(), Value::Int(1));
        let h1 = structural_hash(&Value::Compound(m1));
        let mut m2: Map<String, Value> = Map::new();
        m2.insert("a".into(), Value::Int(2));
        let h2 = structural_hash(&Value::Compound(m2));
        assert_ne!(h1, h2);
    }

    #[test]
    fn no_sections_falls_back_to_whole() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, false).unwrap();
        assert!(matches!(s, Split::Whole { .. }));
    }

    #[test]
    fn empty_sections_list_still_splits() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("sections".to_string(), Value::List(Vec::new()));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, false).unwrap();
        let Split::Split { sections, .. } = s else { panic!("empty list must still split") };
        assert!(sections.is_empty());
    }
}
