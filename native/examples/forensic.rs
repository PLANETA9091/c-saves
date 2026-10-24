//! forensic: walk segment records, validate CRCs, dump kind-7 entries
use std::fs::File;
use std::io::Read;

fn crc_of(b: &[u8]) -> u32 {
    let mut c = crc32fast::Hasher::new();
    c.update(b);
    c.finalize()
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: forensic <seg.log>");
    let mut buf = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
    println!("{}: {} bytes", path, buf.len());
    let mut off = 0usize;
    let mut kinds = std::collections::HashMap::new();
    let mut k7_subs = 0usize;
    let mut k7_bad = 0usize;
    let mut torn = false;
    let mut ids_seen = std::collections::HashSet::new();
    let mut ids_max = 0u32;
    while off + 8 <= buf.len() {
        let blen = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        if blen == 0 || off + 8 + blen > buf.len() {
            println!("TORN tail at {} (blen={} need={} have={})", off, blen, off + 8 + blen, buf.len());
            torn = true;
            break;
        }
        let body = &buf[off + 8..off + 8 + blen];
        let ok = crc_of(body) == crc;
        let kind = body[0];
        *kinds.entry(kind).or_insert(0) += 1;
        if !ok {
            println!("CRC FAIL at {} kind {}", off, kind);
        }
        if kind == 4 && body.len() >= 30 {
            let bid = u32::from_le_bytes(body[2..6].try_into().unwrap());
            ids_seen.insert(bid);
            ids_max = ids_max.max(bid);
        }
        if kind == 7 {
            let n = u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize;
            let mut p = 5usize;
            let mut sub_ok = 0;
            for _ in 0..n {
                if p + 10 > body.len() {
                    k7_bad += 1;
                    break;
                }
                let rl = u16::from_le_bytes(body[p + 8..p + 10].try_into().unwrap()) as usize;
                p += 10;
                if p + rl > body.len() || body[p] != 0xFF {
                    k7_bad += 1;
                    println!("  k7 sub BAD at rec-off {} p {} rl {} bodylen {}", off, p, rl, body.len());
                    break;
                }
                p += rl;
                sub_ok += 1;
            }
            k7_subs += sub_ok;
        }
        off += 8 + blen;
    }
    println!("records by kind: {:?} (torn={})", kinds, torn);
    println!("kind-7 sub-entries: ok={} bad={}", k7_subs, k7_bad);
    println!("walked to {} of {}", off, buf.len());
    println!("blob ids: seen={} max={} has261={} has170={} has459={}", ids_seen.len(), ids_max,
        ids_seen.contains(&261), ids_seen.contains(&170), ids_seen.contains(&459));
    // find contiguous missing ranges up to max
    let mut miss_ranges: Vec<(u32, u32)> = Vec::new();
    let mut i = 1u32;
    while i <= ids_max {
        if !ids_seen.contains(&i) {
            let start = i;
            while i <= ids_max && !ids_seen.contains(&i) { i += 1; }
            miss_ranges.push((start, i - 1));
        } else { i += 1; }
    }
    let big: Vec<String> = miss_ranges.iter().filter(|(a,b)| b-a > 20).map(|(a,b)| format!("{}..{}", a, b)).collect();
    println!("missing ranges (>20): {}", big.join(", "));
    println!("total missing: {}", miss_ranges.iter().map(|(a,b)| b-a+1).sum::<u32>());
}
