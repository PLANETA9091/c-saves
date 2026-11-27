//! end_factor_probe: the piece-factoring census on the END tail mass — the
//! tail_nbt class (n=1239, raw ~1.12 MB, est_comp ~92 KB) is 75% of the
//! end's close weight, and end cities repeat structure NBT across chunks.
//! The question: how much of the tail's raw bytes sit under EXACT repeated
//! subtrees (canonical key-sorted serialization, every compound/list node)?
//! Reports the repeated mass per top key and depth, and the top shapes
//! with (size, count) — the design input for a template+ref tail form.
//!
//! Usage: cargo run --release --example end_factor_probe [footprint]

#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::chunknbt::to_bytes;
use fastnbt::Value;
use std::collections::HashMap as Map;

/// canonical serialization: compounds sorted by key, lists in order
fn canon(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Compound(m) => {
            out.push(0x0A);
            let mut ks: Vec<&String> = m.keys().collect();
            ks.sort();
            for k in ks {
                let b = k.as_bytes();
                out.extend_from_slice(&(b.len() as u16).to_be_bytes());
                out.extend_from_slice(b);
                canon(&m[k], out);
            }
            out.push(0x00);
        }
        Value::List(l) => {
            out.push(0x09);
            out.extend_from_slice(&(l.len() as u32).to_be_bytes());
            for x in l {
                canon(x, out);
            }
        }
        _ => out.extend_from_slice(&to_bytes(v)),
    }
}

fn size_of(v: &Value) -> usize {
    let mut b = Vec::new();
    canon(v, &mut b);
    b.len()
}

fn main() {
    let f: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(192);
    let mut b = tc::EndBuilder::new(f);
    let mut node: Map<u64, (u64, usize, String)> = Map::new(); // hash -> (count, size, path)
    let mut top_mass: Map<String, u64> = Map::new();
    let mut depth_mass: [u64; 8] = [0; 8];
    let mut depth_rep: [u64; 8] = [0; 8];
    let mut total: u64 = 0;
    let mut chunks = 0u64;
    for cz in 0..f {
        for cx in 0..f {
            let (p, _) = b.payload(cx, cz);
            let root = match fastnbt::from_bytes::<Value>(&p) {
                Ok(Value::Compound(m)) => m,
                _ => continue,
            };
            chunks += 1;
            for (k, v) in &root {
                if k == "sections" {
                    continue;
                }
                let sz = size_of(v);
                total += sz as u64;
                *top_mass.entry(k.clone()).or_default() += sz as u64;
                census_node(v, k, 0, &mut node, &mut depth_mass, &mut depth_rep);
            }
        }
    }

    let mut rep_mass = 0u64;
    let mut shapes: Vec<(u64, u64, usize, String)> = Vec::new(); // (count*size, count, size, path)
    for (_, (count, size, path)) in &node {
        if *count >= 2 {
            rep_mass += *count * *size as u64;
            shapes.push((*count * *size as u64, *count, *size, path.clone()));
        }
    }
    shapes.sort_by_key(|x| std::cmp::Reverse(x.0));
    println!(
        "end tails: chunks={chunks} total-serialized={total} ({:.1} B/chunk) repeated-mass={rep_mass} ({:.1}% of total)",
        total as f64 / chunks as f64,
        100.0 * rep_mass as f64 / total.max(1) as f64
    );
    let mut tm: Vec<_> = top_mass.into_iter().collect();
    tm.sort_by_key(|x| std::cmp::Reverse(x.1));
    println!("mass by top key: {}", tm.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "));
    print!("mass by depth: ");
    for d in 0..8 {
        print!("d{d}={}/rep={} ", depth_mass[d], depth_rep[d]);
    }
    println!();
    println!("top-20 repeated shapes (count*size, count, size, path):");
    for (mass, count, size, path) in shapes.iter().take(20) {
        println!("  {mass} B = {count}x{size} B  {path}");
    }
}

/// walk every compound/list node; record (hash -> count, size, exemplar path)
fn census_node(
    v: &Value,
    path: &str,
    depth: usize,
    node: &mut Map<u64, (u64, usize, String)>,
    dm: &mut [u64; 8],
    dr: &mut [u64; 8],
) {
    let is_node = matches!(v, Value::Compound(_) | Value::List(_));
    if is_node && depth < 8 {
        let mut b = Vec::new();
        canon(v, &mut b);
        let sz = b.len();
        let h = xxhash_rust::xxh3::xxh3_64(&b);
        let e = node.entry(h).or_insert((0, sz, path.to_string()));
        e.0 += 1;
        dm[depth] += sz as u64;
        if e.0 >= 2 {
            dr[depth] += sz as u64;
        }
    }
    match v {
        Value::Compound(m) => {
            for (k, x) in m {
                census_node(x, &format!("{path}.{k}"), depth + 1, node, dm, dr);
            }
        }
        Value::List(l) => {
            for (i, x) in l.iter().enumerate().take(4) {
                census_node(x, &format!("{path}[{i}]"), depth + 1, node, dm, dr);
            }
        }
        _ => {}
    }
}
