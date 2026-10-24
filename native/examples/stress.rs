//! stress: concurrent chunk writes + sync + reads (reproduce server data loss)
use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::sync::Arc;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn payload(cx: i32, cz: i32, v: u64) -> Vec<u8> {
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    let mut secs = Vec::new();
    for y in 0..8i32 {
        let mut s: Map<String, Value> = Map::new();
        s.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![
                Value::String("minecraft:stone".into()),
                Value::String(format!("minecraft:block{}_{}", cx, y)),
            ]),
        );
        let entries: Vec<u8> = (0..4096).map(|i| ((i * 7 + v as usize) % 2) as u8).collect();
        // 1-bit no-straddle packing
        let per = 64usize;
        let longs: Vec<i64> = entries
            .chunks(per)
            .map(|c| {
                let mut x = 0u64;
                for (i, e) in c.iter().enumerate() {
                    x |= (*e as u64) << i;
                }
                x as i64
            })
            .collect();
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(longs)));
        s.insert("block_states".to_string(), Value::Compound(bs));
        secs.push(Value::Compound(s));
    }
    root.insert("sections".to_string(), Value::List(secs));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

fn main() {
    let dir = std::env::temp_dir().join(format!(
        "csaves-stress-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let st = Store::open_cfg(
        &dir,
        true,
        Cfg { compact_min: u64::MAX, ..Cfg::default() }, // no compaction
    )
    .unwrap();

    // 8 threads x 64 chunks, concurrent; a syncer thread loops sync()
    let st2 = st.clone();
    let syncer = std::thread::spawn(move || {
        for _ in 0..200 {
            st2.sync();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    let mut handles = Vec::new();
    for t in 0..8i32 {
        let st3 = st.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..64i32 {
                let cx = t * 100 + i;
                let p = payload(cx, i, (t * 7 + i) as u64);
                st3.write_chunk(key(cx, i), &p, false).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    syncer.join().unwrap();
    st.sync();
    st.seal();

    // verify all 512 chunks
    let mut bad = 0;
    for t in 0..8i32 {
        for i in 0..64i32 {
            let cx = t * 100 + i;
            match st.read_chunk(key(cx, i)) {
                Some(got) => {
                    let want = payload(cx, i, (t * 7 + i) as u64);
                    let a: Value = fastnbt::from_bytes(&got).unwrap();
                    let b: Value = fastnbt::from_bytes(&want).unwrap();
                    if a != b {
                        println!("MISMATCH chunk ({cx},{i})");
                        bad += 1;
                    }
                }
                None => {
                    println!("MISSING chunk ({cx},{i})");
                    bad += 1;
                }
            }
        }
    }
    println!("chunks=512 bad={bad} disk={}B", st.disk_bytes());
    drop(st);
    // reopen and re-verify
    let st4 = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    let mut bad2 = 0;
    for t in 0..8i32 {
        for i in 0..64i32 {
            if st4.read_chunk(key(t * 100 + i, i)).is_none() {
                bad2 += 1;
            }
        }
    }
    println!("reopen: chunks={} bad={bad2}", st4.chunk_count());
    let _ = std::fs::remove_dir_all(&dir);
}
