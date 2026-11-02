//! Probe: build the scan-path raw tail (replicating scan_root edits) and the
//! old Value-based tail for the same chunk; parse both and diff semantically.

use fastnbt::Value;
use std::io::Read;
use std::collections::HashMap;

fn load_payloads(dir: &std::path::Path, want: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    'outer: for f in files {
        if f.extension().map(|e| e != "mca").unwrap_or(true) {
            continue;
        }
        let data = std::fs::read(&f).unwrap();
        if data.len() < 8192 {
            continue;
        }
        for i in 0..1024usize {
            let loc = u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
            if loc == 0 {
                continue;
            }
            let off = (loc >> 8) as usize * 4096;
            let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            let comp_type = data[off + 4];
            let mut v = Vec::new();
            match comp_type {
                2 => {
                    flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
                        .read_to_end(&mut v)
                        .unwrap();
                }
                _ => continue,
            }
            out.push(v);
            if out.len() >= want {
                break 'outer;
            }
        }
    }
    out
}

// minimal NBT skip (with compound name advance)
fn skip_value(b: &[u8], p: &mut usize, ty: u8, depth: u32) -> Option<()> {
    if depth == 0 { return None; }
    match ty {
        1 => *p += 1,
        2 => *p += 2,
        3 | 5 => *p += 4,
        4 | 6 => *p += 8,
        7 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(4)?;
        }
        12 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(4)?.checked_add(l.checked_mul(8)?)?;
        }
        11 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(4)?.checked_add(l.checked_mul(4)?)?;
        }
        8 => {
            let l = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(2)?;
        }
        9 => {
            let et = *b.get(*p)?;
            let l = i32::from_be_bytes(b.get(*p + 1..*p + 5)?.try_into().ok()?) as usize;
            *p += 5;
            if l == 0 { return Some(()); }
            if et == 0 { return None; }
            for _ in 0..l { skip_value(b, p, et, depth - 1)?; }
        }
        10 => {
            loop {
                let t = *b.get(*p)?;
                *p += 1;
                if t == 0 { break; }
                let l = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                *p = p.checked_add(2)?.checked_add(l)?;
                skip_value(b, p, t, depth - 1)?;
            }
        }
        _ => return None,
    }
    if *p <= b.len() { Some(()) } else { None }
}

fn scan_tail(payload: &[u8]) -> Option<Vec<u8>> {
    let mut p = 0usize;
    if *payload.first()? != 0x0A { return None; }
    p += 1;
    let nl = u16::from_be_bytes(payload.get(p..p + 2)?.try_into().ok()?) as usize;
    p += 2 + nl;
    let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let mut sections: Option<(usize, usize)> = None;
    let mut insert_light = false;
    let mut saw_light = false;
    loop {
        let tag_pos = p;
        let ty = *payload.get(p)?;
        p += 1;
        if ty == 0 {
            if !saw_light { insert_light = true; }
            // assemble
            let (sp0, sp1) = sections?;
            let mut tail = Vec::with_capacity(payload.len());
            let mut pos = 0usize;
            for (ep, el, bytes) in &edits {
                tail.extend_from_slice(payload.get(pos..*ep)?);
                tail.extend_from_slice(bytes);
                pos = ep + el;
            }
            tail.extend_from_slice(payload.get(pos..tag_pos)?);
            if insert_light {
                tail.extend_from_slice(&[1u8, 0x00, 0x09, b'i', b's', b'L', b'i', b'g', b'h', b't', b'O', b'n', 0x00]);
            }
            tail.push(0x00);
            let _ = (sp0, sp1);
            return Some(tail);
        }
        let nl = u16::from_be_bytes(payload.get(p..p + 2)?.try_into().ok()?) as usize;
        let name = payload.get(p + 2..p + 2 + nl)?;
        p += 2 + nl;
        match (ty, name) {
            (9, b"sections") => {
                let vp = p;
                skip_value(payload, &mut p, 9, 24)?;
                sections = Some((vp, p));
                edits.push((tag_pos, p - tag_pos, Vec::new()));
            }
            (10, b"Heightmaps") => {
                loop {
                    let it_pos = p;
                    let it = *payload.get(p)?;
                    p += 1;
                    if it == 0 { break; }
                    let inl = u16::from_be_bytes(payload.get(p..p + 2)?.try_into().ok()?) as usize;
                    let iname = payload.get(p + 2..p + 2 + inl)?.to_vec();
                    p += 2 + inl;
                    let vs = p;
                    skip_value(payload, &mut p, it, 8)?;
                    if it != 12 { continue; }
                    let l = i32::from_be_bytes(payload.get(vs..vs + 4)?.try_into().ok()?) as usize;
                    if l != 37 { continue; }
                    let raw = payload.get(vs + 4..vs + 4 + l * 8)?;
                    let la: Vec<i64> = raw.chunks_exact(8).map(|c| i64::from_be_bytes(c.try_into().unwrap())).collect();
                    // emulate hmap_encode: 9-bit nostroddle unpack + zigzag varint
                    let Some(dense) = hmap_encode_emu(&la) else { continue };
                    let mut rep = Vec::with_capacity(7 + iname.len() + dense.len());
                    rep.push(7u8);
                    rep.extend_from_slice(&(iname.len() as u16).to_be_bytes());
                    rep.extend_from_slice(&iname);
                    rep.extend_from_slice(&(dense.len() as i32).to_be_bytes());
                    rep.extend_from_slice(&dense);
                    edits.push((it_pos, p - it_pos, rep));
                }
            }
            (_, b"isLightOn") => {
                saw_light = true;
                skip_value(payload, &mut p, ty, 8)?;
                if ty == 1 {
                    if payload[p - 1] != 0 {
                        edits.push((p - 1, 1, vec![0u8]));
                    }
                } else {
                    edits.push((tag_pos, p - tag_pos, Vec::new()));
                    insert_light = true;
                }
            }
            _ => {
                skip_value(payload, &mut p, ty, 32)?;
            }
        }
    }
}

fn unpack_nostroddle(la: &[i64], n: usize, bits: u32) -> Vec<u32> {
    let per = (64 / bits) as usize;
    let mut out = vec![0u32; n];
    for (i, e) in out.iter_mut().enumerate() {
        let v = la[i / per] as u64;
        let shift = (i % per) * bits as usize;
        *e = ((v >> shift) & ((1u64 << bits) - 1)) as u32;
    }
    out
}

fn hmap_encode_emu(la: &[i64]) -> Option<Vec<u8>> {
    if la.len() != 37 { return None; }
    let entries = unpack_nostroddle(la, 256, 9);
    if entries.iter().any(|e| *e as u32 >= 512) { return None; }
    // roundtrip check via repack
    let per = 64 / 9;
    let mut repacked: Vec<i64> = Vec::new();
    for c in entries.chunks(per) {
        let mut v: u64 = 0;
        for (i, e) in c.iter().enumerate() {
            v |= (*e as u64) << (i * 9);
        }
        repacked.push(v as i64);
    }
    if repacked != la { return None; }
    let mut out = vec![0xC6u8, 2u8];
    let mut prev: i64 = 0;
    for e in &entries {
        let d = (*e as i64 - prev) << 1 ^ ((*e as i64 - prev) >> 63);
        let mut z = d as u64;
        loop {
            let b = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 { out.push(b); break; }
            out.push(b | 0x80);
        }
        prev = *e as i64;
    }
    Some(out)
}

fn old_tail(payload: &[u8]) -> Vec<u8> {
    let root: Value = fastnbt::from_bytes(payload).unwrap();
    let Value::Compound(mut m) = root else { panic!() };
    m.remove("sections");
    m.insert("isLightOn".to_string(), Value::Byte(0));
    // tail_encode: heightmap LongArrays -> dense ByteArray
    for v in m.values_mut() {
        if let Value::Compound(hm) = v {
            for hv in hm.values_mut() {
                if let Value::LongArray(la) = hv {
                    if let Some(dense) = hmap_encode_emu(la.as_ref()) {
                        *hv = Value::ByteArray(fastnbt::ByteArray::new(dense.iter().map(|x| *x as i8).collect()));
                    }
                }
            }
        }
    }
    fastnbt::to_bytes(&Value::Compound(m)).unwrap()
}

fn norm(v: &Value) -> Value {
    match v {
        Value::Compound(m) => {
            let mut mm: Vec<(&String, &Value)> = m.iter().map(|(k, v)| (k, v)).collect();
            mm.sort_by(|a, b| a.0.cmp(b.0));
            let _ = mm; // HashMap iteration order is irrelevant: diff() sorts keys
            Value::Compound(m.clone())
        }
        Value::List(l) => Value::List(l.iter().map(norm).collect()),
        _ => v.clone(),
    }
}

fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    if a == b { return; }
    match (a, b) {
        (Value::Compound(ma), Value::Compound(mb)) => {
            let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                match (ma.get(k), mb.get(k)) {
                    (Some(va), Some(vb)) => diff(&format!("{path}.{k}"), va, vb, out),
                    (Some(_), None) => out.push(format!("{path}.{k}: MISSING in new")),
                    (None, Some(_)) => out.push(format!("{path}.{k}: EXTRA in new")),
                    _ => {}
                }
                if out.len() > 8 { return; }
            }
        }
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                out.push(format!("{path}: list len {} vs {}", la.len(), lb.len()));
                return;
            }
            for (i, (x, y)) in la.iter().zip(lb.iter()).enumerate() {
                diff(&format!("{path}[{i}]"), x, y, out);
                if out.len() > 8 { return; }
            }
        }
        _ => out.push(format!("{path}: {a:?} != {b:?}")),
    }
}

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let payloads = load_payloads(&std::path::Path::new(&dir).join("region"), 400);
    let mut bad = 0;
    let mut shown = 0;
    for (n, p) in payloads.iter().enumerate() {
        let Some(t) = scan_tail(p) else { println!("chunk {n}: scan failed"); continue; };
        let a: Value = fastnbt::from_bytes(&old_tail(p)).unwrap();
        let b: Value = fastnbt::from_bytes(&t).unwrap();
        let na = norm(&a);
        let nb = norm(&b);
        if na != nb {
            bad += 1;
            if shown < 6 {
                shown += 1;
                let mut d = Vec::new();
                diff("$", &na, &nb, &mut d);
                println!("chunk {n}: DIFF");
                for line in d.iter().take(12) {
                    println!("  {line}");
                }
            }
        }
    }
    println!("checked={} bad={bad}", payloads.len());
    let _ = HashMap::<u8, u8>::new();
}
