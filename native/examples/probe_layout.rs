//! Probe: dump the root layout of the first corpus chunks — key names, tag
//! types, value byte lengths — to see what scan_root must handle.

use fastnbt::Value;
use std::io::Read;

fn load_first_payloads(dir: &std::path::Path, want: usize) -> Vec<Vec<u8>> {
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

fn ty_name(v: &Value) -> &'static str {
    match v {
        Value::Byte(_) => "Byte",
        Value::Short(_) => "Short",
        Value::Int(_) => "Int",
        Value::Long(_) => "Long",
        Value::Float(_) => "Float",
        Value::Double(_) => "Double",
        Value::ByteArray(_) => "ByteArray",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Compound(_) => "Compound",
        Value::IntArray(_) => "IntArray",
        Value::LongArray(_) => "LongArray",
    }
}

fn size_of(v: &Value) -> usize {
    match v {
        Value::Byte(_) => 1,
        Value::Short(_) => 2,
        Value::Int(_) | Value::Float(_) => 4,
        Value::Long(_) | Value::Double(_) => 8,
        Value::ByteArray(b) => b.len(),
        Value::String(s) => s.len(),
        Value::List(l) => l.iter().map(size_of).sum(),
        Value::Compound(m) => m.iter().map(|(k, v)| k.len() + 1 + size_of(v)).sum(),
        Value::IntArray(a) => a.len() * 4,
        Value::LongArray(a) => a.len() * 8,
    }
}

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let payloads = load_first_payloads(&std::path::Path::new(&dir).join("region"), 3);
    for (n, p) in payloads.iter().enumerate() {
        println!("--- chunk {n}: {} bytes", p.len());
        emu_scan(p);
        emu_wrap(p);
        let root: Value = fastnbt::from_bytes(p).unwrap();
        let Value::Compound(m) = root else { panic!() };
        let mut keys: Vec<_> = m.iter().collect();
        keys.sort_by_key(|(k, _)| *k);
        for (k, v) in keys {
            let extra = match (k.as_str(), v) {
                ("sections", Value::List(l)) => format!(" len={} elem0={}", l.len(), ty_name(l.first().unwrap_or(&Value::Byte(0)))),
                ("Heightmaps", Value::Compound(h)) => format!(
                    " inner={{{}}}",
                    h.iter().map(|(k, v)| format!("{k}: {}", ty_name(v))).collect::<Vec<_>>().join(", ")
                ),
                ("isLightOn", v) => format!(" val={v:?}"),
                _ => String::new(),
            };
            println!("  {:28} {:10} ~{}B{}", k, ty_name(v), size_of(v), extra);
        }
    }
}

// ---- scan emulation: where does the header-arithmetic walk fail? ----

fn skip_value(b: &[u8], p: &mut usize, ty: u8, depth: u32) -> Option<()> {
    if depth == 0 { println!("      FAIL depth0 ty={ty} p={p}"); return None; }
    let entry = *p;
    let r = (|| -> Option<()> {
        match ty {
            1 => *p += 1,
            2 => *p += 2,
            3 | 5 => *p += 4,
            4 | 6 => *p += 8,
            7 => {
                if *p + 4 > b.len() { println!("      FAIL hdr7 p={p}"); return None; }
                let l = i32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
                *p = p.checked_add(l)?.checked_add(4)?;
            }
            12 => {
                if *p + 4 > b.len() { println!("      FAIL hdr12 p={p}"); return None; }
                let l = i32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
                *p = p.checked_add(l)?.checked_add(4)?.checked_add(l.checked_mul(8)?)?;
            }
            11 => {
                if *p + 4 > b.len() { println!("      FAIL hdr11 p={p}"); return None; }
                let l = i32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
                *p = p.checked_add(l)?.checked_add(4)?.checked_add(l.checked_mul(4)?)?;
            }
            8 => {
                if *p + 2 > b.len() { println!("      FAIL hdr8 p={p}"); return None; }
                let l = u16::from_be_bytes(b[*p..*p + 2].try_into().unwrap()) as usize;
                *p = p.checked_add(l)?.checked_add(2)?;
            }
            9 => {
                if *p + 5 > b.len() { println!("      FAIL hdr9 p={p}"); return None; }
                let et = b[*p];
                let l = i32::from_be_bytes(b[*p + 1..*p + 5].try_into().unwrap()) as usize;
                *p += 5;
                if l == 0 { return Some(()); }
                if et == 0 { println!("      FAIL et0 l={l} p={p}"); return None; }
                for i in 0..l {
                    let before = *p;
                    if skip_value(b, p, et, depth - 1).is_none() {
                        println!("      FAIL elem {i}/{l} ty={et} p={before}->{p} depth={depth}");
                        return None;
                    }
                }
            }
            10 => {
                loop {
                    if *p >= b.len() { println!("      FAIL compound overrun p={p}"); return None; }
                    let t = b[*p];
                    *p += 1;
                    if t == 0 { break; }
                    if *p + 2 > b.len() { println!("      FAIL cname p={p}"); return None; }
                    let l = u16::from_be_bytes(b[*p..*p + 2].try_into().unwrap()) as usize;
                    *p += 2;
                    let before = *p;
                    if skip_value(b, p, t, depth - 1).is_none() {
                        println!("      FAIL cval t={t} p={before}->{p} depth={depth}");
                        return None;
                    }
                }
            }
            _ => { println!("      FAIL badty ty={ty} p={entry}"); return None; }
        }
        Some(())
    })();
    if r.is_some() && *p > b.len() { println!("      FAIL overrun-final ty={ty} p={entry}->{p}"); return None; }
    r
}

fn emu_scan(payload: &[u8]) {
    let mut p = 0usize;
    if *payload.first().unwrap() != 0x0A { println!("  EMU: root not compound"); return; }
    p += 1;
    let nl = u16::from_be_bytes(payload[p..p + 2].try_into().unwrap()) as usize;
    p += 2 + nl;
    let mut found = false;
    loop {
        let ty = *payload.get(p).unwrap();
        p += 1;
        if ty == 0 { println!("  EMU: END at {p}, sections={found}"); return; }
        let nl = u16::from_be_bytes(payload[p..p + 2].try_into().unwrap()) as usize;
        let name = &payload[p + 2..p + 2 + nl];
        p += 2 + nl;
        if name == b"sections" {
            if ty != 9 { println!("  EMU: sections ty={ty}"); return; }
            let vp = p;
            println!("  EMU: sections hdr bytes[{}..{}]: {:02x?}", vp-10, vp+12, &payload[vp-10..vp+12]);
            match skip_value(payload, &mut p, 9, 16) {
                Some(()) => { println!("  EMU: sections ok [{vp}..{p}]"); found = true; }
                None => { println!("  EMU: sections SKIP FAILED at p={p}"); return; }
            }
        } else {
            match skip_value(payload, &mut p, ty, 32) {
                Some(()) => {}
                None => {
                    println!("  EMU: skip FAILED on key {} ty={ty} at p={p}", String::from_utf8_lossy(name));
                    return;
                }
            }
        }
    }
}

fn emu_wrap(payload: &[u8]) {
    // find sections range via emu walk again (quick)
    let mut p = 0usize;
    p += 1;
    let nl = u16::from_be_bytes(payload[p..p + 2].try_into().unwrap()) as usize;
    p += 2 + nl;
    loop {
        let ty = *payload.get(p).unwrap();
        p += 1;
        if ty == 0 { return; }
        let nl = u16::from_be_bytes(payload[p..p + 2].try_into().unwrap()) as usize;
        let name = &payload[p + 2..p + 2 + nl];
        p += 2 + nl;
        if name == b"sections" {
            let vp = p;
            let mut q = p;
            skip_value(payload, &mut q, 9, 16).unwrap();
            let mut buf = Vec::with_capacity(6 + (q - vp));
            buf.extend_from_slice(&[0x0A, 0x00, 0x00, 0x09, 0x00, 0x00]);
            buf.extend_from_slice(&payload[vp..q]);
            buf.push(0x00);
            match fastnbt::from_bytes::<Value>(&buf) {
                Ok(Value::Compound(m)) => match m.get("") {
                    Some(Value::List(l)) => println!("  EMU: wrapper parse OK, {} sections", l.len()),
                    _ => println!("  EMU: wrapper parsed but wrong shape"),
                },
                Ok(_) => println!("  EMU: wrapper parsed, not compound"),
                Err(e) => println!("  EMU: wrapper parse FAIL: {e}"),
            }
            return;
        }
        skip_value(payload, &mut p, ty, 32).unwrap();
    }
}
