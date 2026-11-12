//! repro972: live batch commits -> sync -> reopen -> read everything.
//! Mimics the paper run1 pattern: many section-carrying chunks written over
//! time (batch jobs committed by the LIVE batcher repeatedly), then a final
//! sync, then a fresh reopen must serve every chunk.
use c_saves::store::{Cfg, Store};

fn key(cx: i32, cz: i32) -> u64 { ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff) }

/// A sectioned chunk: 24 sections with real palettes -> recipes big enough
/// to cross the 192 KiB batch cap mid-run (live batcher commits).
fn chunk_payload(cx: i32, cz: i32, variant: u64) -> Vec<u8> {
    use fastnbt::{LongArray, Value};
    use std::collections::HashMap as Map;
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
    let mut hm: Map<String, Value> = Map::new();
    let la: Vec<i64> = (0..37).map(|i| (i as i64) * 7 + variant as i64).collect();
    hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(LongArray::new(la)));
    root.insert("Heightmaps".to_string(), Value::Compound(hm));
    let mut sec_list = Vec::new();
    for y in 0..24i32 {
        let shared = (y % 4) != 0;
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        pal.push(Value::String("minecraft:stone".to_string()));
        pal.push(Value::String("minecraft:deepslate".to_string()));
        pal.push(Value::String("minecraft:air".to_string()));
        bs.insert("palette".to_string(), Value::List(pal));
        let data: Vec<i64> = (0..64)
            .map(|i| {
                if shared {
                    (i as i64).wrapping_mul(0x1234_5678_9abc_def0u64 as i64) ^ (variant as i64)
                } else {
                    (i as i64).wrapping_mul(0x1234_9abc_def0_5678i64)
                        ^ (cx as i64) ^ (cz as i64) ^ (y as i64)
                }
            })
            .collect();
        bs.insert("data".to_string(), Value::LongArray(LongArray::new(data)));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        sec_list.push(Value::Compound(sec));
    }
    root.insert("sections".to_string(), Value::List(sec_list));
    let mut be = Vec::new();
    let mut chest: Map<String, Value> = Map::new();
    chest.insert("id".to_string(), Value::String("minecraft:chest".to_string()));
    chest.insert("x".to_string(), Value::Int(cx * 16 + 5));
    chest.insert("y".to_string(), Value::Int(64));
    chest.insert("z".to_string(), Value::Int(cz * 16 + 5));
    be.push(Value::Compound(chest));
    root.insert("block_entities".to_string(), Value::List(be));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

fn main() {
    let dir = std::env::temp_dir().join("repro972");
    let _ = std::fs::remove_dir_all(&dir);
    let st = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    // paper runs a POI scratchpad store with elide semantics in parallel
    let pdir = std::env::temp_dir().join("repro972-poi");
    let _ = std::fs::remove_dir_all(&pdir);
    let poi = Store::open_cfg(&pdir, true, Cfg::default()).unwrap();
    poi.set_poi_elide();
    let n = 600usize;
    let mut payloads = Vec::with_capacity(n);
    for i in 0..n {
        let (cx, cz) = ((i % 25) as i32 - 12, (i / 25) as i32 - 12);
        let p = chunk_payload(cx, cz, i as u64);
        payloads.push((cx, cz, p));
    }
    // poi elide store sees the same writes (absent = absent semantics)
    for (cx, cz, p) in &payloads {
        poi.write_chunk(key(*cx, *cz), p, true).ok();
    }
    // Moonrise-style: chunk writes from an IO thread pool
    std::thread::scope(|sc| {
        for chunk_of_work in payloads.chunks(50) {
            let st = &st;
            sc.spawn(move || {
                for (cx, cz, p) in chunk_of_work {
                    st.write_chunk(key(*cx, *cz), p, false).unwrap();
                }
            });
        }
    });
    // paper autosaves run CONCURRENTLY with chunk writes
    let st_sync = st.clone();
    let stop_sync = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop_sync.clone();
    let syncer = std::thread::spawn(move || {
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            st_sync.sync();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    std::thread::scope(|sc| {
        for chunk_of_work in payloads.chunks(50) {
            let st = &st;
            sc.spawn(move || {
                for (cx, cz, p) in chunk_of_work {
                    st.write_chunk(key(*cx, *cz), p, false).unwrap();
                }
            });
        }
    });
    stop_sync.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = syncer.join();
    st.sync();
    // PAPER AUTOSAVE: re-write EVERY key (fresh worldgen products again) —
    // supersede + live batch commits on the same keys
    std::thread::scope(|sc| {
        for chunk_of_work in payloads.chunks(50) {
            let st = &st;
            sc.spawn(move || {
                for (cx, cz, _p) in chunk_of_work.iter() {
                    let p2 = chunk_payload(*cx, *cz, 1_000_000 + *cx as u64 * 31 + *cz as u64);
                    st.write_chunk(key(*cx, *cz), &p2, false).unwrap();
                }
            });
        }
    });
    st.sync();
    println!("written {} chunks — now DROP WITHOUT SYNC (paper stop path)", n);
    drop(st);
    drop(poi);
    let _ = 0u8;
    // reopen: fresh open of the same dir
    let opened = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    let mut bad = 0usize;
    for (cx, cz, p) in &payloads {
        match opened.read_chunk(key(*cx, *cz)) {
            Some(got) => {
                if got.len() != p.len() { bad += 1; }
            }
            None => bad += 1,
        }
    }
    println!("repro972: reopen read ok={} bad={}", n - bad, bad);
    let _ = std::fs::remove_dir_all(&dir);
}
