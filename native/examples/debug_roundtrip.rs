//! debug: single-chunk roundtrip diff (temporary)
use c_saves::chunknbt::{self};
use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn chunk_payload(cx: i32, cz: i32, variant: u64) -> Vec<u8> {
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
    let mut hm: Map<String, Value> = Map::new();
    let la: Vec<i64> = (0..37).map(|i| (i as i64) * 7 + variant as i64).collect();
    hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(fastnbt::LongArray::new(la)));
    root.insert("Heightmaps".to_string(), Value::Compound(hm));
    let mut sec_list = Vec::new();
    for y in 0..24i32 {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![
                Value::String("minecraft:stone".into()),
                Value::String("minecraft:deepslate".into()),
                Value::String("minecraft:air".into()),
            ]),
        );
        let data: Vec<i64> = (0..64)
            .map(|i| (i as i64).wrapping_mul(0x1234_5678_9abc_def0u64 as i64) ^ (variant as i64))
            .collect();
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        sec_list.push(Value::Compound(sec));
    }
    root.insert("sections".to_string(), Value::List(sec_list));
    c_saves::chunknbt::to_bytes(&Value::Compound(root))
}

fn diff(path: &str, a: &Value, b: &Value) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Compound(ma), Value::Compound(mb)) => {
            for (k, va) in ma {
                match mb.get(k) {
                    Some(vb) => diff(&format!("{path}.{k}"), va, vb),
                    None => println!("DIFF {path}.{k}: missing in got"),
                }
            }
            for k in mb.keys() {
                if !ma.contains_key(k) {
                    println!("DIFF {path}.{k}: extra in got");
                }
            }
        }
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                println!("DIFF {path}: list len {} vs {}", la.len(), lb.len());
                return;
            }
            for (i, (va, vb)) in la.iter().zip(lb.iter()).enumerate() {
                diff(&format!("{path}[{i}]"), va, vb);
            }
        }
        (Value::LongArray(x), Value::LongArray(y)) => {
            println!("DIFF {path}: longarray {} vs {} first-diff {:?}", x.len(), y.len(),
                x.iter().zip(y.iter()).position(|(p, q)| p != q));
        }
        _ => println!("DIFF {path}: {:?}... vs {:?}...", path, b),
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!(
        "csaves-dbg-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let st = Store::open_cfg(&dir, true, Cfg { dict_train: 40, compact_min: 512 * 1024, compact_ratio: 0.30, ..Cfg::default() }).unwrap();
    // multi-chunk pattern from the failing test
    let n = 60usize;
    let mut payloads = Vec::new();
    for i in 0..n {
        let p = chunk_payload(i as i32 - 30, (i as i32 * 7) - 30, (i % 5) as u64);
        payloads.push(p.clone());
        st.write_chunk(key(i as i32 - 30, (i as i32 * 7) - 30), &p, false).unwrap();
    }
    let mut bad = 0;
    for i in 0..n {
        let got = st.read_chunk(key(i as i32 - 30, (i as i32 * 7) - 30)).expect("read");
        let a: Value = fastnbt::from_bytes(&payloads[i]).unwrap();
        let b: Value = fastnbt::from_bytes(&got).unwrap();
        if a != b {
            bad += 1;
            if bad == 1 {
                println!("MISMATCH chunk {i}:");
                diff("root", &a, &b);
            }
        }
    }
    if bad == 0 { println!("section multi OK"); }
    // entity path
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert("Position".to_string(), Value::List(vec![Value::Int(1), Value::Int(0), Value::Int(2)]));
    root.insert("Entities".to_string(), Value::List(vec![]));
    root.insert("OnGround".to_string(), Value::Byte(1));
    let e = chunknbt::to_bytes(&Value::Compound(root));
    st.write_chunk(key(9, 9), &e, true).unwrap();
    let got = st.read_chunk(key(9, 9)).expect("entity read");
    let a: Value = fastnbt::from_bytes(&e).unwrap();
    let b: Value = fastnbt::from_bytes(&got).unwrap();
    if a == b {
        println!("entity path OK");
    } else {
        println!("ENTITY MISMATCH:");
        diff("root", &a, &b);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
