//! probe_sec — dump the NBT shape of the first few sections of the committed
//! bench corpus (2.5 MB, seconds). Diagnostic for the classifier: why does
//! bm.get("palette") never fire / to_bytes return 0?

use fastnbt::Value;
use flate2::read::ZlibDecoder;
use std::io::Read;

fn main() {
    let path = "../bench-corpus/dim/region/r.0.0.mca";
    let data = std::fs::read(path).unwrap();
    let mut n = 0;
    for i in 0..1024usize {
        let loc = u32::from_be_bytes([
            data[i * 4],
            data[i * 4 + 1],
            data[i * 4 + 2],
            data[i * 4 + 3],
        ]);
        if loc == 0 {
            continue;
        }
        let off = (loc >> 8) as usize * 4096;
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        let payload = {
            let mut v = Vec::new();
            ZlibDecoder::new(&data[off + 5..off + 4 + len])
                .read_to_end(&mut v)
                .unwrap();
            v
        };
        let v: Value = fastnbt::from_bytes(&payload).unwrap();
        let Value::Compound(m) = v else { continue };
        println!("-- chunk keys: {:?}", m.keys().collect::<Vec<_>>());
        let Some(Value::List(secs)) = m.get("sections") else { continue };
        println!("   sections: {} list items", secs.len());
        for s in secs.iter().take(2) {
            let Value::Compound(sm) = s else { continue };
            for (k, sv) in sm.iter() {
                let tname = match sv {
                    Value::Byte(_) => "Byte",
                    Value::Short(_) => "Short",
                    Value::Int(_) => "Int",
                    Value::Long(_) => "Long",
                    Value::Float(_) => "Float",
                    Value::Double(_) => "Double",
                    Value::ByteArray(a) => return_print("ByteArray", a.len()),
                    Value::String(_) => "String",
                    Value::List(_) => "List",
                    Value::Compound(_) => "Compound",
                    Value::LongArray(a) => return_print("LongArray", a.len()),
                    Value::IntArray(a) => return_print("IntArray", a.len()),
                };
                println!("   sec key {k:16} {tname}");
            }
            if let Some(bs) = sm.get("block_states") {
                match bs {
                    Value::Compound(bm) => {
                        println!("   block_states keys: {:?}", bm.keys().collect::<Vec<_>>());
                        if let Some(pal) = bm.get("palette") {
                            let b = fastnbt::to_bytes(pal);
                            match pal {
                                Value::List(items) => {
                                    println!(
                                        "   palette: List len={} to_bytes={b:?}",
                                        items.len()
                                    );
                                    if let Some(Value::Compound(e)) = items.first() {
                                        println!(
                                            "   palette[0] keys: {:?}",
                                            e.keys().collect::<Vec<_>>()
                                        );
                                    }
                                }
                                _ => println!("   palette NOT a list: to_bytes={b:?}"),
                            }
                        } else {
                            println!("   block_states has NO palette key!");
                        }
                    }
                    _ => println!("   block_states is NOT a compound: {bs:?} (truncated)"),
                }
            }
        }
        n += 1;
        if n >= 2 {
            break;
        }
    }
}

fn return_print(name: &str, len: usize) -> &'static str {
    println!("      ({} len {})", name, len);
    ""
}
