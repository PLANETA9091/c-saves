//! enrich — add the survival-world payload (loot-chest block entities +
//! entity chunks) DIRECTLY to an anvil world folder, without booting a
//! server. Deterministic; used by the bigworld-survival bench so the merged
//! tile artifacts can be enriched in-place.
//!
//!   * chests: ~n block entities with realistic Items NBT appended to the
//!     `block_entities` lists of region chunks within the radius (no block
//!     placement — the BE payload bytes are what the storage benchmark
//!     measures; palette impact of 2000 blocks over 262k chunks is noise),
//!   * entities: entity-chunk region files under `entities/` are (re)built
//!     with n entities in a golden-angle spiral (same corpus shape as the
//!     RCON spawner: villagers/sheep/zombies/armor stands,
//!     PersistenceRequired, deterministic UUIDs).
//!
//! Usage: enrich <dim-dir> [n_chests=2000] [n_entities=20000] [radius=250]

use fastnbt::Value;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap as Map;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------- anvil io ---

fn load_mca(path: &Path) -> Vec<(usize, Vec<u8>)> {
    // returns (local_slot, raw_payload) pairs
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    if data.len() < 8192 {
        return vec![];
    }
    let mut out = Vec::new();
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
        if off + 5 > data.len() {
            continue;
        }
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        if len < 2 || off + 4 + len > data.len() {
            continue;
        }
        let comp_type = data[off + 4];
        let payload = match comp_type {
            1 => {
                let mut v = Vec::new();
                if ZlibDecoder::new(&data[off + 5..off + 4 + len]).read_to_end(&mut v).is_err() {
                    continue;
                }
                v
            }
            2 => {
                let mut v = Vec::new();
                if flate2::read::ZlibDecoder::new(&data[off + 5..off + 4 + len])
                    .read_to_end(&mut v)
                    .is_err()
                {
                    continue;
                }
                v
            }
            3 => data[off + 5..off + 4 + len].to_vec(),
            _ => continue,
        };
        out.push((i, payload));
    }
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(6));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Rewrite one .mca with the given (slot, payload) chunks (zlib-6, sector
/// padded — vanilla layout).
fn write_mca(path: &Path, chunks: &[(usize, Vec<u8>)]) -> u64 {
    let mut header = vec![0u8; 8192];
    let mut body: Vec<u8> = Vec::new();
    for (li, payload) in chunks {
        let comp = zlib(payload);
        let mut rec = Vec::with_capacity(comp.len() + 5);
        rec.extend_from_slice(&((comp.len() + 1) as u32).to_be_bytes());
        rec.push(2); // zlib
        rec.extend_from_slice(&comp);
        let sector = rec.len().div_ceil(4096);
        let off = (body.len() / 4096) as u32;
        header[li * 4..li * 4 + 4]
            .copy_from_slice(&((off << 8) | sector as u32).to_be_bytes());
        body.extend_from_slice(&rec);
        body.extend(std::iter::repeat_n(0u8, sector * 4096 - rec.len()));
    }
    header[4096..8192].fill(1); // timestamps, non-zero like vanilla
    header.extend_from_slice(&body);
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(path, &header).map(|_| header.len() as u64).unwrap_or(0)
}

// ------------------------------------------------------------ generators --

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const COMMON: &[(&str, i32, i32, u32)] = &[
    ("minecraft:cobblestone", 12, 64, 20),
    ("minecraft:dirt", 8, 64, 8),
    ("minecraft:oak_log", 4, 32, 10),
    ("minecraft:oak_planks", 8, 48, 8),
    ("minecraft:wheat", 6, 48, 10),
    ("minecraft:bread", 1, 12, 10),
    ("minecraft:iron_ingot", 2, 40, 9),
    ("minecraft:coal", 5, 64, 10),
    ("minecraft:stick", 4, 64, 6),
    ("minecraft:torch", 6, 64, 8),
    ("minecraft:string", 1, 24, 5),
    ("minecraft:arrow", 4, 64, 5),
    ("minecraft:bone", 1, 20, 4),
    ("minecraft:gunpowder", 1, 12, 3),
    ("minecraft:rotten_flesh", 1, 16, 3),
    ("minecraft:leather", 1, 12, 3),
    ("minecraft:wheat_seeds", 1, 24, 5),
    ("minecraft:diamond", 1, 8, 2),
    ("minecraft:emerald", 1, 12, 2),
    ("minecraft:ender_pearl", 1, 6, 2),
    ("minecraft:book", 1, 8, 3),
];
const TOOLS: &[&str] = &[
    "minecraft:iron_pickaxe",
    "minecraft:iron_axe",
    "minecraft:iron_shovel",
    "minecraft:iron_sword",
    "minecraft:bow",
    "minecraft:fishing_rod",
    "minecraft:shield",
    "minecraft:bucket",
    "minecraft:shears",
    "minecraft:compass",
    "minecraft:spyglass",
    "minecraft:lead",
];

fn item_value(id: &str, count: i32, slot: i32) -> Value {
    Value::Compound({
        let mut m: Map<String, Value> = Map::new();
        m.insert("id".to_string(), Value::String(id.to_string()));
        m.insert("count".to_string(), Value::Int(count));
        m.insert("slot".to_string(), Value::Byte(slot as i8));
        m
    })
}

fn make_chest(x: i32, y: i32, z: i32, rng: &mut Rng) -> Value {
    let total_w: u32 = COMMON.iter().map(|c| c.3).sum();
    let n_items = 6 + rng.range(17) as usize; // 6..22
    let mut slots: Vec<i32> = (0..27).collect();
    // deterministic partial shuffle
    for i in (1..n_items.min(27)).rev() {
        let j = rng.range((i + 1) as u64) as usize;
        slots.swap(i, j);
    }
    let mut items = Vec::with_capacity(n_items);
    for &slot in slots.iter().take(n_items) {
        if rng.range(100) < 8 {
            let id = TOOLS[rng.range(TOOLS.len() as u64) as usize];
            items.push(item_value(id, 1, slot));
        } else {
            let mut pick = rng.range(total_w as u64) as u32;
            let mut chosen = COMMON[0];
            for c in COMMON {
                if pick < c.3 {
                    chosen = *c;
                    break;
                }
                pick -= c.3;
            }
            let cnt = chosen.1 + rng.range((chosen.2 - chosen.1 + 1) as u64) as i32;
            items.push(item_value(chosen.0, cnt.min(64), slot));
        }
    }
    Value::Compound({
        let mut m: Map<String, Value> = Map::new();
        m.insert("id".to_string(), Value::String("minecraft:chest".into()));
        m.insert("x".to_string(), Value::Int(x));
        m.insert("y".to_string(), Value::Int(y));
        m.insert("z".to_string(), Value::Int(z));
        m.insert("Items".to_string(), Value::List(items));
        m
    })
}

const ENT_KINDS: &[&str] = &[
    "minecraft:villager",
    "minecraft:sheep",
    "minecraft:zombie",
    "minecraft:armor_stand",
];

fn make_entity(kind: &str, x: f64, y: f64, z: f64, rng: &mut Rng) -> Value {
    let h = {
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        (x.to_bits()).hash(&mut hs);
        (z.to_bits()).hash(&mut hs);
        rng.next().hash(&mut hs);
        hs.finish()
    };
    let uuid = vec![
        Value::Int((h >> 32) as i32),
        Value::Int(h as i32),
        Value::Int((h.wrapping_mul(0x9E37_79B9) >> 32) as i32),
        Value::Int(h.wrapping_mul(0x7F4A_7C15) as i32),
    ];
    let mut m: Map<String, Value> = Map::new();
    m.insert("id".to_string(), Value::String(kind.to_string()));
    m.insert(
        "Pos".to_string(),
        Value::List(vec![
            Value::Double(x),
            Value::Double(y),
            Value::Double(z),
        ]),
    );
    m.insert(
        "Motion".to_string(),
        Value::List(vec![Value::Double(0.0), Value::Double(0.0), Value::Double(0.0)]),
    );
    m.insert(
        "Rotation".to_string(),
        Value::List(vec![Value::Float(0.0), Value::Float(0.0)]),
    );
    m.insert("UUID".to_string(), Value::List(uuid));
    m.insert("Health".to_string(), Value::Float(10.0 + (rng.range(100) as f32) / 10.0));
    m.insert("PersistenceRequired".to_string(), Value::Byte(1));
    m.insert("Tags".to_string(), Value::List(vec![Value::String("survb".into())]));
    if kind == "minecraft:armor_stand" {
        m.insert("ShowArms".to_string(), Value::Byte(1));
    }
    Value::Compound(m)
}

// --------------------------------------------------------------- main ----

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).cloned().unwrap_or_default());
    let n_chests: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(2000);
    let n_ents: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20000);
    let radius: i32 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(250);

    let region_dir = dim.join("region");
    let entity_dir = dim.join("entities");

    // ---- chests: append block_entities to region chunks in the radius ----
    let mut files: Vec<PathBuf> = std::fs::read_dir(&region_dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    let mut rng = Rng(0x5EED_9092);
    let mut total_chests = 0u64;
    let mut per_file: Vec<(PathBuf, Vec<(usize, Vec<u8>)>)> = Vec::new();
    for f in &files {
        let chunks = load_mca(f);
        if chunks.is_empty() {
            continue;
        }
        let mut new_chunks = Vec::with_capacity(chunks.len());
        let mut touched = 0usize;
        for (slot, payload) in chunks {
            let mut p: Value = match fastnbt::from_bytes(&payload) {
                Ok(v) => v,
                Err(_) => {
                    new_chunks.push((slot, payload));
                    continue;
                }
            };
            let Value::Compound(m) = &mut p else {
                new_chunks.push((slot, payload));
                continue;
            };
            // chunk coords from root keys (vanilla stores xPos/zPos)
            let (cx, cz) = match (m.get("xPos"), m.get("zPos")) {
                (Some(Value::Int(a)), Some(Value::Int(b))) => (*a, *b),
                _ => {
                    new_chunks.push((slot, payload));
                    continue;
                }
            };
            // is this chunk within the chest radius? (sample one target per
            // chunk with probability proportional to coverage)
            let dist = (cx * 16).abs().max((cz * 16).abs());
            if dist > radius + 16 {
                new_chunks.push((slot, payload));
                continue;
            }
            // expected chests per chunk = n_chests / chunks_in_radius
            let rx = ((radius * 2 + 1) / 16 + 1) as f64;
            let expected = n_chests as f64 / (rx * rx);
            let mut placed_here = 0usize;
            if rng.range(1000) < (expected * 1000.0) as u64 {
                placed_here = 1;
                // occasionally 2-3 chests in one chunk (storage rooms)
                if rng.range(100) < 15 {
                    placed_here += 1 + rng.range(2) as usize;
                }
            }
            if placed_here == 0 {
                new_chunks.push((slot, payload));
                continue;
            }
            let bes = m
                .entry("block_entities".to_string())
                .or_insert_with(|| Value::List(Vec::new()));
            if let Value::List(list) = bes {
                for k in 0..placed_here {
                    let x = cx * 16 + rng.range(16) as i32;
                    let z = cz * 16 + rng.range(16) as i32;
                    let y = 40 + rng.range(80) as i32;
                    list.push(make_chest(x, y, z, &mut rng));
                    total_chests += 1;
                }
            }
            touched += 1;
            new_chunks.push((slot, fastnbt::to_bytes(&p).unwrap()));
        }
        if touched > 0 {
            per_file.push((f.clone(), new_chunks));
        }
    }
    let mut written = 0u64;
    for (path, chunks) in &per_file {
        written += write_mca(path, chunks);
    }
    println!(
        "enrich: {} chests into {} of {} region files ({} bytes on disk)",
        total_chests,
        per_file.len(),
        files.len(),
        written
    );

    // ---- entities: build entity-chunk region files from scratch ----
    let mut ent_regions: Map<(i32, i32), Vec<(usize, Vec<u8>)>> = Map::new();
    let mut ent_index: Map<(i32, i32), usize> = Map::new(); // (cx,cz) -> idx
    let mut placed_ents = 0u64;
    for i in 0..n_ents {
        let kind = ENT_KINDS[(i % ENT_KINDS.len() as u64) as usize];
        // golden-angle spiral around spawn
        let a = i as f64 * 2.399963;
        let r = 380.0 * ((i as f64 + 0.5) / n_ents as f64).sqrt();
        let x = 8.0 + r * a.cos() + rng.range(3) as f64 - 1.0;
        let z = 8.0 + r * a.sin() + rng.range(3) as f64 - 1.0;
        let y = 120.0;
        let cx = (x.floor() as i32) >> 4;
        let cz = (z.floor() as i32) >> 4;
        let slot = ((cz.rem_euclid(32) * 32 + cx.rem_euclid(32)) as usize);
        let ent = make_entity(kind, x, y, z, &mut rng);
        let rx = cx >> 5;
        let rz = cz >> 5;
        let regs = ent_regions.entry((rx, rz)).or_default();
        match ent_index.get(&(cx, cz)) {
            Some(&idx) => {
                let (_, payload) = &mut regs[idx];
                if let Ok(mut v) = fastnbt::from_bytes::<Value>(payload) {
                    if let Value::Compound(m) = &mut v {
                        let list = m
                            .entry("Entities".to_string())
                            .or_insert_with(|| Value::List(Vec::new()));
                        if let Value::List(l) = list {
                            l.push(ent);
                            placed_ents += 1;
                            if let Ok(nb) = fastnbt::to_bytes(&v) {
                                *payload = nb;
                                continue;
                            }
                        }
                    }
                }
            }
            None => {
                ent_index.insert((cx, cz), regs.len());
                let mut m: Map<String, Value> = Map::new();
                m.insert("DataVersion".to_string(), Value::Int(4189));
                m.insert(
                    "Position".to_string(),
                    Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
                );
                m.insert("Entities".to_string(), Value::List(vec![ent]));
                regs.push((slot, fastnbt::to_bytes(&Value::Compound(m)).unwrap()));
                placed_ents += 1;
            }
        }
    }
    let mut ent_bytes = 0u64;
    for ((rx, rz), chunks) in &ent_regions {
        let p = entity_dir.join(format!("r.{rx}.{rz}.mca"));
        ent_bytes += write_mca(&p, chunks);
    }
    println!(
        "enrich: {} entities into {} entity-chunk records, {} region files ({} bytes on disk)",
        placed_ents,
        ent_regions.values().map(|v| v.len()).sum::<usize>(),
        ent_regions.len(),
        ent_bytes
    );
}
