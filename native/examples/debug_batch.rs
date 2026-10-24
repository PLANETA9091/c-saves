//! debug batched recipes (temporary)
use c_saves::store::{Cfg, Store};

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn main() {
    let dir = std::env::temp_dir().join(format!(
        "csaves-dbg2-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let st = Store::open_cfg(&dir, true, Cfg { dict_train: 10, ..Cfg::default() }).unwrap();
    let mut p: Vec<u8> = vec![];
    // minimal sectioned payload via public API: use any bytes (whole mode)
    p = fastnbt::to_bytes(&fastnbt::Value::Compound(
        [("DataVersion".to_string(), fastnbt::Value::Int(4189)),
         ("Position".to_string(), fastnbt::Value::List(vec![
            fastnbt::Value::Int(1), fastnbt::Value::Int(0), fastnbt::Value::Int(0)])),
         ("Entities".to_string(), fastnbt::Value::List(vec![]))]
        .into_iter().collect(),
    )).unwrap();
    for i in 0..3i32 {
        st.write_chunk(key(i, 0), &p, true).unwrap();
    }
    println!("count={}", st.chunk_count());
    for i in 0..3i32 {
        match st.read_chunk(key(i, 0)) {
            Some(d) => println!("read {i}: {} bytes", d.len()),
            None => println!("read {i}: FAIL  {}", st.debug_blob_state(key(i, 0))),
        }
    }
    st.sync();
    for i in 0..3i32 {
        match st.read_chunk(key(i, 0)) {
            Some(d) => println!("post-sync read {i}: {} bytes", d.len()),
            None => println!("post-sync read {i}: FAIL  {}", st.debug_blob_state(key(i, 0))),
        }
    }
    drop(st);
    let st2 = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
    for i in 0..3i32 {
        match st2.read_chunk(key(i, 0)) {
            Some(d) => println!("reopen read {i}: {} bytes", d.len()),
            None => println!("reopen read {i}: FAIL  {}", st2.debug_blob_state(key(i, 0))),
        }
    }
    // dump the kind-7 record region
    use std::io::Read as _;
    if let Ok(mut f) = std::fs::File::open(dir.join("seg-00000.log")) {
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        println!("seg0 len={}", buf.len());
        // find 0x07 record markers
        for i in 0..buf.len().saturating_sub(5) {
            if buf[i] == 7 && i % 1 == 0 {
                let n = u32::from_le_bytes(buf[i+1..i+5].try_into().unwrap()) as usize;
                if n == 3 && i + 5 < buf.len() && buf[i+5+8+2] == 0xFF {
                    println!("kind-7 at {}: n={} first-sub-0xFF at {}", i, n, i+5+10);
                    println!("bytes[{}..{}]: {:02x?}", i, i+40, &buf[i..i+40]);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
