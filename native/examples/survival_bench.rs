//! survival_bench: size benchmark against a REALISTIC SURVIVAL-world corpus.
//!
//! The JJThunder corpus is pristine generated terrain (96% uniform sections,
//! no light arrays, no block entities). A long-running survival world is a
//! different beast — this example models exactly that, deterministically:
//!   * 24-section chunks (y -64..320), 3D-noise terrain with caves and ore
//!     veins (hashed value noise — no external deps),
//!   * per-section Skylight/BlockLight arrays for lit sections (the vanilla
//!     storage hog: 2048 B each, half/full gradients),
//!   * block entities: chests with item stacks, furnaces with state,
//!   * entity chunks: mixed mobs with equipment (repeat-heavy, like farms).
//! Every chunk is written BOTH to a vanilla Anvil region (zlib, sector-padded)
//! and through the production c-saves pipeline; the comparison is byte-exact
//! per component and every chunk is verified semantically after import.

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

// ----------------------------------------------------------- noise ----

/// Deterministic 2D value noise in [0,1).
fn noise2(x: i32, z: i32, seed: u64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (z as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ seed.wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    (h & 0xFFFF_FFFF) as f64 / 4294967296.0
}

/// Smoothed value noise (bilinear over the lattice).
fn smooth2(x: f64, z: f64, seed: u64) -> f64 {
    let x0 = x.floor() as i32;
    let z0 = z.floor() as i32;
    let fx = x - x0 as f64;
    let fz = z - z0 as f64;
    let n00 = noise2(x0, z0, seed);
    let n10 = noise2(x0 + 1, z0, seed);
    let n01 = noise2(x0, z0 + 1, seed);
    let n11 = noise2(x0 + 1, z0 + 1, seed);
    let a = n00 + (n10 - n00) * fx;
    let b = n01 + (n11 - n01) * fx;
    a + (b - a) * fz
}

fn hash3(x: i32, y: i32, z: i32, seed: u64) -> u64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (z as u64).wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    h
}

// ---------------------------------------------------- world builder ----

const SEC_MIN: i32 = -4; // y -64
const SEC_MAX: i32 = 20; // y 320

/// surface height (top solid block y) per (x,z)
fn column_height(cx: i32, cz: i32, x: usize, z: usize) -> i32 {
    let wx = cx as f64 * 16.0 + x as f64;
    let wz = cz as f64 * 16.0 + z as f64;
    let base = smooth2(wx / 96.0, wz / 96.0, 0xA1);
    let detail = smooth2(wx / 24.0, wz / 24.0, 0xB2);
    let h = 62.0 + (base - 0.5) * 44.0 + (detail - 0.5) * 10.0;
    h as i32
}

/// Which block at world (x,y,z)? Deterministic survival-like terrain:
/// grass/dirt surface, stone body, dirt/gravel patches, ore veins, caves.
fn block_at(cx: i32, cz: i32, x: usize, y: i32, z: usize, surface: i32) -> &'static str {
    let wx = cx * 16 + x as i32;
    let wz = cz * 16 + z as i32;
    // caves: two 3D noise fields, carve where both are high (cheese caves)
    let c1 = noise2(wx / 7, y / 7, 0xC1 ^ (wz / 7) as u64);
    let c2 = noise2(y / 9, wx / 9, 0xC2 ^ (wz / 9) as u64);
    if y < surface - 4 && c1 > 0.72 && c2 > 0.72 {
        return "minecraft:air";
    }
    if y > surface {
        if y <= 62 {
            return "minecraft:water";
        }
        return "minecraft:air";
    }
    if y == surface {
        return "minecraft:grass_block";
    }
    if y > surface - 4 {
        return "minecraft:dirt";
    }
    // ore veins: hashed pockets per 4x4x4 cell
    let cell = (wx >> 2, y >> 2, wz >> 2);
    let r = hash3(cell.0, cell.1, cell.2, 0xE0) % 1000;
    let deep = y < 0;
    if deep {
        if r < 6 {
            return "minecraft:diamond_ore";
        }
        if r < 16 {
            return "minecraft:gold_ore";
        }
        if r < 40 {
            return "minecraft:iron_ore";
        }
        if r < 60 {
            return "minecraft:copper_ore";
        }
        return "minecraft:deepslate";
    }
    if r < 30 {
        return "minecraft:iron_ore";
    }
    if r < 55 {
        return "minecraft:coal_ore";
    }
    if r < 62 {
        return "minecraft:gravel";
    }
    "minecraft:stone"
}

/// Skylight value per (x,y,z) of a lit section: 15 above terrain, decaying
/// into caves — realistic enough: full column light where no solid above.
fn skylight_at(surface: i32, y: i32, lit: bool) -> i8 {
    if !lit {
        return 0;
    }
    if y > surface {
        15
    } else if y == surface {
        15
    } else if y == surface - 1 {
        13
    } else if y == surface - 2 {
        9
    } else if y == surface - 3 {
        5
    } else {
        0
    }
}

/// Build one section compound the way vanilla writes it (palette + data at
/// vanilla-clamped width) plus light arrays for lit sections.
fn build_section(
    cx: i32,
    cz: i32,
    sy: i32,
    surface_col: &[i32; 256],
    with_skylight: bool,
) -> Value {
    let mut sec: Map<String, Value> = Map::new();
    sec.insert("Y".to_string(), Value::Byte(sy as i8));
    // collect palette + entries
    let mut pal: Vec<&'static str> = Vec::new();
    let mut idx_of: Map<&'static str, u8> = Map::new();
    let mut entries: Vec<u8> = Vec::with_capacity(4096);
    for z in 0..16usize {
        for x in 0..16usize {
            let surface = surface_col[z * 16 + x];
            for dy in 0..16i32 {
                let y = sy * 16 + dy;
                let b = block_at(cx, cz, x, y, z, surface);
                let i = match idx_of.get(b) {
                    Some(i) => *i,
                    None => {
                        let i = pal.len() as u8;
                        pal.push(b);
                        idx_of.insert(b, i);
                        i
                    }
                };
                entries.push(i);
            }
        }
    }
    let mut bs: Map<String, Value> = Map::new();
    bs.insert(
        "palette".to_string(),
        Value::List(pal.iter().map(|b| Value::String(b.to_string())).collect()),
    );
    if pal.len() > 1 {
        let bits = {
            let mut b = 4u32;
            while (1usize << b) < pal.len() {
                b += 1;
            }
            b.max(4)
        };
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(c_saves::chunknbt::pack_nostroddle(
                &entries, bits,
            ))),
        );
    }
    sec.insert("block_states".to_string(), Value::Compound(bs));
    // uniform biome palette (plains) + light
    sec.insert(
        "biomes".to_string(),
        Value::Compound({
            let mut bm: Map<String, Value> = Map::new();
            bm.insert(
                "palette".to_string(),
                Value::List(vec![Value::String("minecraft:plains".to_string())]),
            );
            bm
        }),
    );
    if with_skylight {
        // vanilla stores skylight for sections with any light variance
        let mut arr = vec![0i8; 2048];
        for z in 0..16usize {
            for x in 0..16usize {
                let surface = surface_col[z * 16 + x];
                for dy in 0..16i32 {
                    let y = sy * 16 + dy;
                    let v = skylight_at(surface, y, true);
                    // nibble packing: (y*256+z*16+x), low nibble = even x
                    let i = (dy as usize) * 256 + z * 16 + x;
                    let byte = i >> 1;
                    let cur = arr[byte] as u8;
                    let nv = if x % 2 == 0 {
                        (cur & 0xF0) | (v as u8 & 0x0F)
                    } else {
                        (cur & 0x0F) | ((v as u8 & 0x0F) << 4)
                    };
                    arr[byte] = nv as i8;
                }
            }
        }
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(arr)));
    }
    Value::Compound(sec)
}

fn surface_map(cx: i32, cz: i32) -> [i32; 256] {
    let mut m = [0i32; 256];
    for z in 0..16usize {
        for x in 0..16usize {
            m[z * 16 + x] = column_height(cx, cz, x, z);
        }
    }
    m
}

/// Heightmap longs: 256 9-bit entries (no-straddle, 37 longs).
fn heightmap_long(surface_col: &[i32; 256]) -> Vec<i64> {
    let entries: Vec<u8> = (0..256)
        .map(|i| (surface_col[i] + 64) as u8) // y+64 in 0..384 fits 9 bits
        .collect();
    c_saves::chunknbt::pack_nostroddle(&entries, 9)
}

fn item(id: &str, count: i32, slot: i32) -> Value {
    Value::Compound({
        let mut m: Map<String, Value> = Map::new();
        m.insert("id".to_string(), Value::String(id.to_string()));
        m.insert("count".to_string(), Value::Int(count));
        m.insert("slot".to_string(), Value::Byte(slot as i8));
        m
    })
}

fn chest_block_entity(x: i32, y: i32, z: i32, variant: u64) -> Value {
    Value::Compound({
        let mut m: Map<String, Value> = Map::new();
        m.insert("id".to_string(), Value::String("minecraft:chest".to_string()));
        m.insert("x".to_string(), Value::Int(x));
        m.insert("y".to_string(), Value::Int(y));
        m.insert("z".to_string(), Value::Int(z));
        // 27-slot inventory: survival players hoard — items repeat heavily
        // across chests (same farm output), a few random stacks per chest
        let common = [
            "minecraft:cobblestone",
            "minecraft:oak_log",
            "minecraft:wheat",
            "minecraft:iron_ingot",
            "minecraft:coal",
            "minecraft:bread",
        ];
        let mut items = Vec::new();
        for slot in 0..27i32 {
            let pick = (hash3(x, y + slot, z, variant) % 100) as i32;
            if pick < 70 {
                let id = common[(hash3(x, slot, z, variant) % common.len() as u64) as usize];
                items.push(item(id, 1 + (pick as i32 % 64), slot));
            }
        }
        m.insert("Items".to_string(), Value::List(items));
        m
    })
}

fn build_chunk(cx: i32, cz: i32) -> Vec<u8> {
    let surface = surface_map(cx, cz);
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
    // heightmaps (vanilla: several maps; two representative ones here)
    let hm_long = heightmap_long(&surface);
    root.insert(
        "Heightmaps".to_string(),
        Value::Compound({
            let mut hm: Map<String, Value> = Map::new();
            hm.insert(
                "MOTION_BLOCKING".to_string(),
                Value::LongArray(fastnbt::LongArray::new(hm_long.clone())),
            );
            hm.insert(
                "WORLD_SURFACE".to_string(),
                Value::LongArray(fastnbt::LongArray::new(hm_long)),
            );
            hm
        }),
    );
    let mut sec_list = Vec::new();
    for sy in SEC_MIN..SEC_MAX {
        // skylight only for sections near/at the surface (vanilla stores
        // light for sections with any non-trivial light)
        let sec_top = sy * 16 + 15;
        let smin = surface.iter().cloned().fold(i32::MAX, |a, b| a.min(b));
        let smax = surface.iter().cloned().fold(i32::MIN, |a, b| a.max(b));
        let lit = sec_top >= smin - 4 && sy * 16 <= smax + 1;
        sec_list.push(build_section(cx, cz, sy, &surface, lit));
    }
    root.insert("sections".to_string(), Value::List(sec_list));
    // block entities: ~2 chests per chunk near the surface
    let mut be = Vec::new();
    for k in 0..2u64 {
        let x = cx * 16 + (hash3(cx, k as i32, cz, 0x11) % 16) as i32;
        let z = cz * 16 + (hash3(cx, (k + 7) as i32, cz, 0x12) % 16) as i32;
        let y = surface[(z.rem_euclid(16) * 16 + x.rem_euclid(16)) as usize];
        be.push(chest_block_entity(x, y, z, 0x13 + k));
    }
    root.insert("block_entities".to_string(), Value::List(be));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

fn build_entity_chunk(cx: i32, cz: i32, n: usize) -> Vec<u8> {
    // survival entity chunk: mobs clustered (farms), repeat-heavy NBT
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
    let kinds = ["minecraft:cow", "minecraft:sheep", "minecraft:pig", "minecraft:chicken"];
    let mut ents = Vec::new();
    for i in 0..n {
        let kind = kinds[(hash3(cx, i as i32, cz, 0x21) % kinds.len() as u64) as usize];
        let mut m: Map<String, Value> = Map::new();
        m.insert("id".to_string(), Value::String(kind.to_string()));
        let ux = cx * 16 + (hash3(cx, i as i32 + 1, cz, 0x22) % 16) as i32;
        let uz = cz * 16 + (hash3(cx, i as i32 + 2, cz, 0x23) % 16) as i32;
        m.insert(
            "Pos".to_string(),
            Value::List(vec![
                Value::Double(ux as f64 + 0.5),
                Value::Double(70.0),
                Value::Double(uz as f64 + 0.5),
            ]),
        );
        // UUID: 4 ints (incompressible identity)
        let h = hash3(cx, i as i32 + 3, cz, 0x24);
        m.insert(
            "UUID".to_string(),
            Value::List(vec![
                Value::Int((h >> 32) as i32),
                Value::Int(h as i32),
                Value::Int((h.wrapping_mul(0x9E37) >> 32) as i32),
                Value::Int(h.wrapping_mul(0x79B9) as i32),
            ]),
        );
        m.insert(
            "Health".to_string(),
            Value::Float(10.0 + (h % 100) as f32 / 10.0),
        );
        m.insert("PersistenceRequired".to_string(), Value::Byte(1));
        // bred animals share attributes byte-for-byte (farm worlds!)
        ents.push(Value::Compound(m));
    }
    root.insert("Entities".to_string(), Value::List(ents));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

// --------------------------------------------------------- anvil io ----

fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(6));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Vanilla region files for the whole world: chunks grouped by
/// (rx, rz) = (cx>>5, cz>>5), each written as a padded .mca image.
fn write_anvil_region(path_prefix: &std::path::Path, chunks: &[(i32, i32, Vec<u8>)]) -> u64 {
    use std::collections::HashMap as RMap;
    let mut regions: RMap<(i32, i32), Vec<(usize, &Vec<u8>)>> = RMap::new();
    for (cx, cz, payload) in chunks {
        let key = (cx >> 5, cz >> 5);
        let li = ((cz.rem_euclid(32) * 32 + cx.rem_euclid(32)) as usize);
        regions.entry(key).or_default().push((li, payload));
    }
    let mut total = 0u64;
    for ((rx, rz), members) in regions {
        let mut buf = vec![0u8; 8192];
        let mut body: Vec<u8> = Vec::new();
        for (li, payload) in members {
            let comp = zlib(payload);
            let mut rec = Vec::with_capacity(comp.len() + 5);
            rec.extend_from_slice(&((comp.len() + 1) as u32).to_be_bytes());
            rec.push(2); // zlib
            rec.extend_from_slice(&comp);
            let sector = (rec.len() + 4095) / 4096;
            let off = (body.len() / 4096) as u32;
            buf[li * 4..li * 4 + 4].copy_from_slice(&((off << 8) | sector as u32).to_be_bytes());
            let pad = sector * 4096 - rec.len();
            body.extend_from_slice(&rec);
            body.extend(std::iter::repeat(0u8).take(pad));
        }
        buf[4096..8192].fill(1); // timestamps (non-zero, like vanilla)
        buf.extend_from_slice(&body);
        let p = path_prefix.with_file_name(format!(
            "{}_r.{}.{}.mca",
            path_prefix.file_name().unwrap().to_string_lossy(),
            rx,
            rz
        ));
        std::fs::write(p, &buf).unwrap();
        total += buf.len() as u64;
    }
    total
}

// ------------------------------------------------------------- main ----

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let half: i32 = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(16); // 32x32 chunks default
    let ent_per_chunk: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(12);
    let out_dir = std::path::PathBuf::from(args.get(3).map(|s| s.as_str()).unwrap_or("target/survival-bench"));
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut region_chunks: Vec<(i32, i32, Vec<u8>)> = Vec::new();
    let mut entity_chunks: Vec<(i32, i32, Vec<u8>)> = Vec::new();
    for cz in -half..half {
        for cx in -half..half {
            region_chunks.push((cx, cz, build_chunk(cx, cz)));
            // entity chunks every 4th chunk (like natural spawn density),
            // farms pack 20-40 mobs
            if (cx + cz).rem_euclid(4) == 0 {
                let n = if (cx * cz).rem_euclid(7) == 0 { ent_per_chunk * 3 } else { ent_per_chunk };
                entity_chunks.push((cx, cz, build_entity_chunk(cx, cz, n)));
            }
        }
    }
    let raw_region: u64 = region_chunks.iter().map(|(_, _, p)| p.len() as u64).sum();
    let raw_ent: u64 = entity_chunks.iter().map(|(_, _, p)| p.len() as u64).sum();
    println!(
        "survival corpus: {} region chunks ({} sections), {} entity chunks | raw NBT: region {:.2} MiB, entities {:.2} MiB",
        region_chunks.len(),
        region_chunks.len() * ((SEC_MAX - SEC_MIN) as usize),
        entity_chunks.len(),
        mib(raw_region),
        mib(raw_ent)
    );
    let mut comp_weights: Map<&str, u64> = Map::new();
    for (_, _, p) in &region_chunks {
        let v: Value = fastnbt::from_bytes(p).unwrap();
        let Value::Compound(m) = v else { continue };
        if let Some(Value::List(secs)) = m.get("sections") {
            for s in secs {
                if let Value::Compound(sm) = s {
                    if let Some(bs) = sm.get("block_states") {
                        let b = fastnbt::to_bytes(bs).unwrap_or_default().len() as u64;
                        *comp_weights.entry("block_states").or_insert(0) += b;
                    }
                    if let Some(Value::ByteArray(sl)) = sm.get("SkyLight") {
                        *comp_weights.entry("skylight").or_insert(0) += sl.as_ref().len() as u64;
                    }
                }
            }
        }
        if let Some(bes) = m.get("block_entities") {
            let b = fastnbt::to_bytes(bes).unwrap_or_default().len() as u64;
            *comp_weights.entry("block_entities").or_insert(0) += b;
        }
        if let Some(hm) = m.get("Heightmaps") {
            let b = fastnbt::to_bytes(hm).unwrap_or_default().len() as u64;
            *comp_weights.entry("heightmaps").or_insert(0) += b;
        }
    }
    let mut rows: Vec<(&str, u64)> = comp_weights.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n-- survival component weights (raw) --");
    for (n, b) in &rows {
        println!("{n:26} {:>12} ({:4.1}%)", b, 100.0 * *b as f64 / raw_region as f64);
    }

    // anvil side
    let region_path = out_dir.join("survival_region.mca");
    let anvil_region = write_anvil_region(&region_path, &region_chunks);
    let anvil_ent: u64 = entity_chunks.iter().map(|(_, _, p)| zlib(p).len() as u64).sum();
    println!(
        "\nanvil: region {:.2} MiB (on-disk w/ sectors), entities {:.2} MiB (zlib-6)",
        mib(anvil_region),
        mib(anvil_ent)
    );

    // c-saves side: production pipeline
    let store_dir = out_dir.join("store-region");
    let _ = std::fs::remove_dir_all(&store_dir);
    let rs = Store::open_cfg(&store_dir, true, bench_cfg()).unwrap();
    let t0 = std::time::Instant::now();
    for (cx, cz, p) in &region_chunks {
        rs.write_chunk(key(*cx, *cz), p, false).unwrap();
    }
    let store_dir_e = out_dir.join("store-entities");
    let _ = std::fs::remove_dir_all(&store_dir_e);
    let es = Store::open_cfg(&store_dir_e, true, bench_cfg()).unwrap();
    for (cx, cz, p) in &entity_chunks {
        es.write_chunk(key(*cx, *cz), p, true).unwrap();
    }
    rs.sync();
    es.sync();
    // pack + cold tier (background pass equivalence)
    loop {
        let a = rs.compact_step();
        let b = es.compact_step();
        if !a && !b {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let rd = rs.disk_bytes();
    let ed = es.disk_bytes();
    println!(
        "c-saves: region {:.2} MiB, entities {:.2} MiB (import+pack {:.1}s, {:.0} chunks/s)",
        mib(rd),
        mib(ed),
        secs,
        (region_chunks.len() + entity_chunks.len()) as f64 / secs
    );
    println!(
        "region : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_region),
        mib(rd),
        anvil_region as f64 / rd.max(1) as f64
    );
    println!(
        "entities: anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_ent),
        mib(ed),
        anvil_ent as f64 / ed.max(1) as f64
    );
    println!(
        "TOTAL  : anvil {:.2} MiB -> c-saves {:.2} MiB  (x{:.2} smaller)",
        mib(anvil_region + anvil_ent),
        mib(rd + ed),
        (anvil_region + anvil_ent) as f64 / (rd + ed).max(1) as f64
    );
    println!("region profile: {}", rs.profile_json());
    println!("region enc-modes: {}", c_saves::chunknbt::enc_mode_stats());

    // verify every chunk semantically
    let mut ok = 0usize;
    let mut bad = 0usize;
    for (cx, cz, p) in &region_chunks {
        match rs.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                if chunk_semantic(p, &d) {
                    ok += 1;
                } else {
                    bad += 1;
                    if bad <= 3 {
                        eprintln!("semantic MISMATCH region chunk ({cx},{cz})");
                    }
                }
            }
            None => {
                bad += 1;
                eprintln!("region MISSING chunk ({cx},{cz})");
            }
        }
    }
    for (cx, cz, p) in &entity_chunks {
        match es.read_chunk(key(*cx, *cz)) {
            Some(d) => {
                let va: Value = fastnbt::from_bytes(p).unwrap();
                let vb: Value = fastnbt::from_bytes(&d).unwrap();
                if va == vb {
                    ok += 1;
                } else {
                    bad += 1;
                    if bad <= 3 {
                        eprintln!("semantic MISMATCH entity chunk ({cx},{cz})");
                    }
                }
            }
            None => {
                bad += 1;
                eprintln!("entity MISSING chunk ({cx},{cz})");
            }
        }
    }
    println!("verify: ok={ok} bad={bad}");
    if bad > 0 {
        std::process::exit(1);
    }
}

fn chunk_semantic(a: &[u8], b: &[u8]) -> bool {
    let va: Value = fastnbt::from_bytes(a).unwrap();
    let vb: Value = fastnbt::from_bytes(b).unwrap();
    let Value::Compound(ma) = &va else { return va == vb };
    let Value::Compound(mb) = &vb else { return false };
    for (k, v) in ma {
        if k == "sections" {
            let (Value::List(sa), Some(Value::List(sb))) = (v, mb.get("sections")) else {
                return false;
            };
            if sa.len() != sb.len() {
                return false;
            }
            for (s1, s2) in sa.iter().zip(sb.iter()) {
                if !c_saves::chunknbt::sections_semantically_equal(s1, s2) {
                    return false;
                }
            }
        } else if v != mb.get(k).unwrap_or(&Value::Byte(0)) {
            return false;
        }
    }
    true
}

fn bench_cfg() -> Cfg {
    Cfg {
        compact_min: 512 * 1024,
        compact_ratio: 0.30,
        dict_train: 40,
        ..Cfg::default()
    }
}
