//! glob_stable_probe: which synthetic payload classes round-trip the glob
//! reconstruction BYTE-EXACTLY (the 0.9.97 pack-side gate's predicate)?
//! Prints per-variant stable/total so the gated-minting test can pin a
//! genuinely byte-exact class instead of guessing the encoder's palette
//! order behavior. Usage: glob_stable_probe

use c_saves::chunknbt::{self, SecBytes, Split, StateRegistry};
use fastnbt::Value;
use std::collections::HashMap;

type Map = HashMap<String, Value>;

fn build_payload(variant: u32, i: usize) -> Vec<u8> {
    let mut root: Map = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    let mut sec_list = Vec::new();
    for y in 0..24i32 {
        let mut sec: Map = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map = Map::new();
        let mut pal = Vec::new();
        let mut vox = [0u8; 4096];
        match variant {
            // A: the known-divergent sliding-window payload (store.rs test helper)
            0 => {
                for k in 0..20usize {
                    let mut c: Map = Map::new();
                    let id = (i * 7 + y as usize * 3 + k) % 200;
                    c.insert("Name".to_string(), Value::String(format!("minecraft:st_{id}")));
                    let mut props: Map = Map::new();
                    props.insert("f".to_string(), Value::String(format!("{}", k % 2)));
                    c.insert("Properties".to_string(), Value::Compound(props));
                    pal.push(Value::Compound(c));
                }
                for x in 0..16usize {
                    for z in 0..16usize {
                        for yy in 0..16usize {
                            let band = ((x + z + yy) / 4 + i + y as usize) % 20;
                            let speck = (x * 31 + z * 17 + yy * 7 + i) % 19 == 0;
                            vox[x * 256 + z * 16 + yy] =
                                if speck { (band + 5) % 20 } else { band } as u8;
                        }
                    }
                }
            }
            // B: first-reference-ordered — voxel t uses state t%20 in the
            // linear order, palette[k] is the k-th distinct state
            1 => {
                for k in 0..20usize {
                    let mut c: Map = Map::new();
                    let id = (i * 7 + y as usize * 3 + k) % 200;
                    c.insert("Name".to_string(), Value::String(format!("minecraft:st_{id}")));
                    let mut props: Map = Map::new();
                    props.insert("f".to_string(), Value::String(format!("{}", k % 2)));
                    c.insert("Properties".to_string(), Value::Compound(props));
                    pal.push(Value::Compound(c));
                }
                for (t, v) in vox.iter_mut().enumerate() {
                    *v = (t % 20) as u8;
                }
            }
            // C: single-state section (palette [st], no refs beyond 0)
            2 => {
                let mut c: Map = Map::new();
                let id = (i * 7 + y as usize) % 200;
                c.insert("Name".to_string(), Value::String(format!("minecraft:one_{id}")));
                pal.push(Value::Compound(c));
                // vox stays all-zero -> state 0
            }
            _ => unreachable!(),
        }
        bs.insert("palette".to_string(), Value::List(pal));
        let n_states = match variant {
            2 => 1usize,
            _ => 20usize,
        };
        let vbits = if variant == 2 {
            4u32
        } else {
            ((n_states as u32 - 1).ilog2() + 1).max(4)
        };
        let data = chunknbt::dense_to_nostroddle(&vox, 4096, 8, vbits);
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        sec_list.push(Value::Compound(sec));
    }
    root.insert("sections".to_string(), Value::List(sec_list));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

fn main() {
    for variant in 0..3u32 {
        let mut reg = StateRegistry::new();
        let (mut stable, mut total, mut conv) = (0usize, 0usize, 0usize);
        for i in 0..6usize {
            let payload = build_payload(variant, i);
            let Ok(Split::Split { sections, .. }) = chunknbt::split(&payload, false, None, true)
            else {
                panic!("split failed variant {variant}");
            };
            for (h, sb) in &sections {
                total += 1;
                let csec = match sb {
                    SecBytes::Shared(a) => (**a).clone(),
                    SecBytes::Owned(b) => b.clone(),
                };
                let Some(glob) = chunknbt::section_to_glob(&csec, &mut reg) else {
                    continue;
                };
                conv += 1;
                if let Some(direct) = chunknbt::glob_any_to_section(&glob, &reg) {
                    if let Some(canon) = chunknbt::canonical_reencode(&direct) {
                        if canon == csec && chunknbt::hash_of(&canon) == *h {
                            stable += 1;
                            continue;
                        }
                    }
                }
                eprintln!("  v{variant} i={i} h={h:016x} DIVERGED");
            }
        }
        println!("variant {variant}: convertible {conv}/{total}, byte-exact {stable}/{conv}");
    }
}
