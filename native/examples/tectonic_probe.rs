//! tectonic_probe: honest huge-world numbers for the Tectonic-shaped ask —
//! "even a 20k x 20k chunk world must weigh very little".
//!
//! The corpus is STRUCTURAL mimicry, not worldgen: it reproduces the
//! compressible structure of a real Tectonic pregen (v3):
//!   - vast bare-ocean fields whose chunks are byte-identical except the
//!     xPos/zPos pair (the 0.9.102 pos-free-tail lever eats them),
//!   - sparse decoration variants (kelp fields, trees, flower patches) that
//!     REPEAT across coordinates — the way real feature placement does,
//!   - structure starts (shipwrecks) whose tail NBT carries chunkX/chunkZ —
//!     they honestly BREAK recipe identity the way vanilla does,
//!   - mountain ridges whose full band sections repeat while only the
//!     surface bands and heightmaps are unique.
//! Every proportion is a deterministic hash draw over the printed mix; the
//! honest claim is per-class marginal cost, the 20k x 20k table is a linear
//! extrapolation of measured bytes.
//!
//! Baselines measured on disk, not estimated:
//!   - Anvil: the SAME payloads written as real .mca region files
//!     (zlib-9, the strongest honest vanilla treatment),
//!   - c-saves: the production import path (write_chunk_import, DEFAULT
//!     cfg = real storage; the pristine/contract regeneration mode stays
//!     OFF by operator directive) driven through the full drain.
//!
//! Usage: cargo run --release --example tectonic_probe [-- <cx> <cz>]

use c_saves::chunknbt::hash_of;
use c_saves::store::{Cfg, Store};
use fastnbt::{LongArray, Value};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap as Map;
use std::io::Write;
use std::path::{Path, PathBuf};

const DV: i32 = 4189; // 1.21.4-era data version

// ---------------------------------------------------------------- rng

fn sm64(z0: u64) -> u64 {
    let mut z = z0.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn h2(cx: i32, cz: i32, salt: u64) -> u64 {
    sm64(
        (cx as u64).wrapping_mul(0x2545_F491_4F6C_DD1D)
            ^ (cz as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ salt,
    )
}

// ------------------------------------------------------- nbt builders

fn pack_states(vals: &[u8], bits: u32) -> Vec<i64> {
    // vanilla 1.16+ packing: no long straddle, entriesPerLong = 64 / bits
    let bits = bits.max(1) as usize;
    let per = 64 / bits;
    let n = (vals.len() + per - 1) / per;
    let mut out = vec![0i64; n];
    let mask = ((1i64 << bits) - 1) as i64;
    for (i, &v) in vals.iter().enumerate() {
        let li = i / per;
        let sh = (i % per) * bits;
        out[li] |= ((v as i64) & mask) << sh;
    }
    out
}

/// mixed section: >=3-state palette (canonical re-encode keeps the 4-bit
/// packing, so roundtrip stays byte-identical), vanilla layered rows.
fn sec_mixed(y: i32, names: &[&str], vals: &[u8]) -> Value {
    let mut bs: Map<String, Value> = Map::new();
    bs.insert(
        "palette".to_string(),
        Value::List(names.iter().map(|n| Value::String((*n).to_string())).collect()),
    );
    bs.insert(
        "data".to_string(),
        Value::LongArray(LongArray::new(pack_states(vals, 4))),
    );
    let mut s: Map<String, Value> = Map::new();
    s.insert("Y".to_string(), Value::Byte(y as i8));
    s.insert("block_states".to_string(), Value::Compound(bs));
    Value::Compound(s)
}

fn sec_full(y: i32, name: &str) -> Value {
    let mut bs: Map<String, Value> = Map::new();
    bs.insert(
        "palette".to_string(),
        Value::List(vec![Value::String(name.to_string())]),
    );
    let mut s: Map<String, Value> = Map::new();
    s.insert("Y".to_string(), Value::Byte(y as i8));
    s.insert("block_states".to_string(), Value::Compound(bs));
    Value::Compound(s)
}

fn sec_empty(y: i32) -> Value {
    let mut s: Map<String, Value> = Map::new();
    s.insert("Y".to_string(), Value::Byte(y as i8));
    Value::Compound(s)
}

/// 9-bit packed heightmap (37 longs, no straddle) — `vals[i]` = column i.
fn hm9(vals: &[u16]) -> LongArray {
    let per = 7usize; // 64/9
    let mut out = vec![0i64; 37];
    for (i, v) in vals.iter().enumerate() {
        let li = i / per;
        let sh = (i % per) * 9;
        out[li] |= ((*v as i64) & 0x1ff) << sh;
    }
    LongArray::new(out)
}

fn hm_const(v: u16) -> LongArray {
    hm9(&[v; 256])
}

/// heightmap with a per-column override (wrecks: sparse, like vanilla)
fn hm_over(base: u16, over: &[(usize, u16)]) -> LongArray {
    let mut col = vec![base; 256];
    for (i, v) in over {
        col[*i] = *v;
    }
    hm9(&col)
}

fn heightmaps(maps: Vec<(&str, LongArray)>) -> Value {
    let mut m: Map<String, Value> = Map::new();
    for (k, v) in maps {
        m.insert(k.to_string(), Value::LongArray(v));
    }
    Value::Compound(m)
}

/// Full chunk payload: sections + the standard tail field set (the shape
/// the split machinery eats; xPos/zPos are the real root-level Ints the
/// pos-free strip removes).
fn chunk_nbt(
    cx: i32,
    cz: i32,
    secs: Vec<Value>,
    hm: Value,
    starts: Option<Value>,
) -> Vec<u8> {
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(DV));
    root.insert("Status".to_string(), Value::String("minecraft:full".to_string()));
    root.insert("LastUpdate".to_string(), Value::Long(0));
    root.insert("InhabitedTime".to_string(), Value::Long(0));
    root.insert("xPos".to_string(), Value::Int(cx));
    root.insert("zPos".to_string(), Value::Int(cz));
    // the pregen-import shape: isLightOn=0 (c-saves write_chunk_import
    // strips derived light; the server relights from stored blocks —
    // vanilla-standard), no light arrays
    root.insert("isLightOn".to_string(), Value::Byte(0));
    let mut st: Map<String, Value> = Map::new();
    st.insert("starts".to_string(), starts.unwrap_or(Value::Compound(Map::new())));
    st.insert("References".to_string(), Value::Compound(Map::new()));
    root.insert("Structures".to_string(), Value::Compound(st));
    // CSAVES_TECTONIC_NOHM=1: the A/B leg that prices what Heightmaps cost
    // INSIDE the drained container today (the tier's honest upper bound)
    if std::env::var("CSAVES_TECTONIC_NOHM").as_deref() != Ok("1") {
        root.insert("Heightmaps".to_string(), hm);
    }
    root.insert("sections".to_string(), Value::List(secs));
    root.insert("block_entities".to_string(), Value::List(Vec::new()));
    root.insert("block_ticks".to_string(), Value::List(Vec::new()));
    root.insert("fluid_ticks".to_string(), Value::List(Vec::new()));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

// ------------------------------------------------- section content fns

/// bedrock/deepslate bottom section (identical shape in every biome — the
/// global section-dedup story)
fn bedrock_section() -> Value {
    let mut vals = vec![1u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let y = (i >> 8) as i32;
        if y == 0 || (y == 1 && h2(i as i32, 7, 0xBED) % 5 < 2) {
            *v = 0; // bedrock
        }
    }
    sec_mixed(
        -5,
        &["minecraft:bedrock", "minecraft:deepslate", "minecraft:stone"],
        &vals,
    )
}

/// ocean floor: gravel/clay patches over deepslate (one shared shape)
fn ocean_floor_section() -> Value {
    let mut vals = vec![1u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let y = i >> 8;
        if y == 15 {
            let r = h2(x * 31 + z, 3, 0xF10) % 100;
            *v = if r < 55 { 0 } else if r < 80 { 2 } else { 1 };
        }
    }
    sec_mixed(
        -2,
        &["minecraft:gravel", "minecraft:deepslate", "minecraft:clay"],
        &vals,
    )
}

/// water/air surface section (the top water row under open sky)
fn ocean_surface_section() -> Value {
    let mut vals = vec![0u8; 4096]; // water
    for (i, v) in vals.iter_mut().enumerate() {
        if (i >> 8) == 15 {
            *v = 1; // air above the sea
        }
    }
    sec_mixed(
        3,
        &["minecraft:water", "minecraft:air", "minecraft:ice"],
        &vals,
    )
}

/// kelp variant section: sparse kelp columns in water (variant = column
/// pattern seed; every variant repeats across thousands of coordinates —
/// real kelp placement is patchy the same way)
fn kelp_section(variant: u64) -> Value {
    let mut vals = vec![0u8; 4096]; // water
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let y = i >> 8;
        let r = h2(x, z, 0x1E1F ^ variant) % 100;
        if r < 4 {
            *v = if y < 15 { 1 } else { 2 }; // kelp column
        } else if r < 7 && y >= 12 {
            *v = 2; // tall seagrass
        }
    }
    sec_mixed(
        2,
        &["minecraft:water", "minecraft:kelp", "minecraft:tall_seagrass"],
        &vals,
    )
}

/// shipwreck hull + deck sections (unique per wreck)
fn wreck_hull_section(cx: i32, cz: i32) -> Value {
    let mut vals = vec![2u8; 4096]; // water elsewhere
    let bx = 4 + (h2(cx, cz, 0x5EC) % 8) as usize;
    let bz = 4 + (h2(cx, cz, 0x5ED) % 8) as usize;
    for dy in 0..6usize {
        for dx in 0..7usize {
            for dz in 0..5usize {
                let i = (dy << 8) | ((bz + dz) << 4) | (bx + dx);
                let hull = h2((dx * 7 + dz * 3) as i32, dy as i32, 0x5EA) % 10;
                vals[i] = if hull < 5 { 0 } else if hull < 7 { 1 } else { 2 };
            }
        }
    }
    sec_mixed(
        1,
        &["minecraft:oak_planks", "minecraft:oak_log", "minecraft:water", "minecraft:air"],
        &vals,
    )
}

fn wreck_deck_section(cx: i32, cz: i32) -> Value {
    let mut vals = vec![2u8; 4096];
    let bx = 4 + (h2(cx, cz, 0x5EC) % 8) as usize;
    let bz = 4 + (h2(cx, cz, 0x5ED) % 8) as usize;
    for dx in 0..7usize {
        for dz in 0..5usize {
            let i = (6usize << 8) | ((bz + dz) << 4) | (bx + dx);
            vals[i] = (h2(dx as i32, dz as i32, 0x5EB) % 3) as u8;
        }
    }
    sec_mixed(
        2,
        &["minecraft:oak_planks", "minecraft:oak_fence", "minecraft:water", "minecraft:air"],
        &vals,
    )
}

/// plains surface section at ground row g (0..15): grass over dirt over
/// stone; flower variant sprinkles flowers on the row above ground.
fn plains_surface_section(g: usize, flower: Option<u64>) -> Value {
    let g = g as i32;
    let mut vals = vec![2u8; 4096]; // stone default
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let y = (i >> 8) as i32;
        if y < g - 1 {
            *v = 2;
        } else if y < g {
            *v = 1; // dirt layer
        } else if y == g {
            *v = 0; // grass block
        } else if let Some(fv) = flower {
            let r = h2(x, z, 0xF10 ^ fv) % 100;
            *v = if y == g + 1 && r < 12 {
                if r < 5 { 3 } else { 4 }
            } else {
                5 // air
            };
        } else {
            *v = 3; // air
        }
    }
    let names: &[&str] = match flower {
        Some(_) => &[
            "minecraft:grass_block",
            "minecraft:dirt",
            "minecraft:stone",
            "minecraft:poppy",
            "minecraft:tall_grass",
            "minecraft:air",
        ],
        None => &["minecraft:grass_block", "minecraft:dirt", "minecraft:stone", "minecraft:air"],
    };
    sec_mixed(4, names, &vals)
}

fn plains_dirt_section() -> Value {
    let mut vals = vec![0u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let y = i >> 8;
        let r = h2(i as i32, 11, 0xD12) % 100;
        if y >= 3 && r < 25 {
            *v = 1; // andesite patch
        }
    }
    sec_mixed(1, &["minecraft:dirt", "minecraft:andesite", "minecraft:stone"], &vals)
}

/// tree top section (variant = canopy shape seed)
fn tree_section(tv: u64) -> Value {
    let mut vals = vec![2u8; 4096]; // air
    let tx = 4 + (sm64(tv) % 7) as usize;
    let tz = 4 + (sm64(tv ^ 0x77) % 7) as usize;
    let th = 5 + (sm64(tv ^ 0x99) % 5) as usize; // trunk rows in-section
    for dy in 0..th {
        vals[(dy << 8) | (tz << 4) | tx] = 0; // oak log
    }
    let cy = th - 2;
    for dy in cy..(cy + 3).min(16) {
        for dx in 0..5usize {
            for dz in 0..5usize {
                let dyc = (dy - cy) as i32;
                let r = (dx as i32 - 2).abs() + (dz as i32 - 2).abs() + dyc;
                if r <= 3 - dyc {
                    let i = (dy << 8) | ((tz + dz - 1) << 4) | (tx + dx - 1);
                    if vals[i] == 2 {
                        vals[i] = 1; // oak leaves
                    }
                }
            }
        }
    }
    sec_mixed(
        5,
        &["minecraft:oak_log", "minecraft:oak_leaves", "minecraft:air"],
        &vals,
    )
}

/// mountain mid band: stone/andesite/gravel noise (unique per chunk — the
/// near-identical-band frontier)
fn mountain_band_section(y: i32, cx: i32, cz: i32) -> Value {
    let mut vals = vec![0u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        let r = h2(cx * 16 + x, cz * 16 + z, 0xB0 ^ ((y as u64) << 8) ^ (yy as u64)) % 100;
        *v = if r < 88 { 0 } else if r < 96 { 1 } else { 2 };
    }
    sec_mixed(y, &["minecraft:stone", "minecraft:andesite", "minecraft:gravel"], &vals)
}

/// mountain surface section: ridge top at row `top` (±1 per-column noise);
/// snow cap above y=144.
fn mountain_surface_section(y: i32, cx: i32, cz: i32, top: usize, snow: bool) -> Value {
    let top = top as i32;
    let mut vals = vec![3u8; 4096]; // air above the ridge
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        let d = (h2(cx * 16 + x, cz * 16 + z, 0x51F) % 3) as i32;
        let t = top + d - 1;
        *v = if yy > t {
            3
        } else if yy == t {
            if snow { 0 } else { 1 } // snow cap / grass
        } else if yy == t - 1 {
            if snow { 1 } else { 2 } // dirt under snow / stone
        } else {
            2 // stone
        };
    }
    if snow {
        sec_mixed(
            y,
            &["minecraft:snow_block", "minecraft:grass_block", "minecraft:stone", "minecraft:air"],
            &vals,
        )
    } else {
        sec_mixed(
            y,
            &["minecraft:grass_block", "minecraft:dirt", "minecraft:stone", "minecraft:air"],
            &vals,
        )
    }
}

// ---------------------------------------------------- payload assembly

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Class {
    OceanBare,
    OceanKelp,
    OceanWreck,
    Beach,
    Plains,
    PlainsFlower,
    PlainsTree,
    River,
    Mountain,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::OceanBare => "ocean_bare",
            Class::OceanKelp => "ocean_kelp",
            Class::OceanWreck => "ocean_wreck",
            Class::Beach => "beach",
            Class::Plains => "plains",
            Class::PlainsFlower => "plains_flower",
            Class::PlainsTree => "plains_tree",
            Class::River => "river",
            Class::Mountain => "mountain",
        }
    }
}

/// The unique-shape cache: every (class, variant) payload is built ONCE
/// with anchor coords, then only the xPos/zPos bytes are patched per chunk
/// — exactly the real pregen structure where identical chunks differ in the
/// coordinate pair only. Unique classes (wreck, mountain) build per chunk.
pub struct Builder {
    cache: Map<(u8, u64), Vec<u8>>,
    cx_w: i32,
}

impl Builder {
    pub fn new(cx_w: i32) -> Self {
        Builder { cache: Map::new(), cx_w }
    }

    fn classify(&self, cx: i32, cz: i32) -> (Class, u64) {
        if cx < (self.cx_w * 5) / 8 {
            let h = h2(cx, cz, 1);
            let r = h % 1000;
            if r < 700 {
                (Class::OceanBare, 0)
            } else if r < 970 {
                (Class::OceanKelp, (h >> 8) % 9)
            } else {
                (Class::OceanWreck, 0)
            }
        } else if cx < (self.cx_w * 5) / 8 + 3 {
            (Class::Beach, h2(cx, cz, 2) % 3)
        } else if cx < self.cx_w - self.cx_w / 12 {
            if cz == 46 || cz == 47 {
                return (Class::River, h2(cx, cz, 9) % 2);
            }
            if h2(cx, cz, 5) % 100 < 8 {
                (Class::PlainsTree, (h2(cx, cz, 5) >> 8) % 12 * 4 + h2(cx, cz, 3) % 4)
            } else if h2(cx, cz, 6) % 100 < 15 {
                (Class::PlainsFlower, (h2(cx, cz, 6) >> 8) % 6 * 4 + h2(cx, cz, 3) % 4)
            } else {
                (Class::Plains, h2(cx, cz, 3) % 4)
            }
        } else {
            (Class::Mountain, 0)
        }
    }

    fn ocean_shared(&mut self, cls: Class, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (cls as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let mut secs: Vec<Value> = Vec::with_capacity(12);
        secs.push(bedrock_section());
        secs.push(sec_full(-4, "minecraft:deepslate"));
        secs.push(sec_full(-3, "minecraft:deepslate"));
        secs.push(ocean_floor_section());
        for y in -1..=2 {
            secs.push(sec_full(y, "minecraft:water"));
        }
        if cls == Class::OceanKelp {
            secs.pop(); // the y=2 water section becomes the kelp variant
            secs.push(kelp_section(var));
        }
        secs.push(ocean_surface_section());
        for y in 4..=5 {
            secs.push(sec_empty(y));
        }
        // vanilla: MOTION_BLOCKING counts fluids — water surface at 62 -> 63;
        // kelp/seagrass do not block motion, so bare and kelp share the maps.
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(63)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(63)),
            ("OCEAN_FLOOR", hm_const(48)),
            ("WORLD_SURFACE", hm_const(63)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn ocean_wreck(&mut self, cx: i32, cz: i32) -> Vec<u8> {
        let floor = 32 + (h2(cx, cz, 0x1F) % 6) as i32;
        let mut secs: Vec<Value> = Vec::with_capacity(13);
        secs.push(bedrock_section());
        secs.push(sec_full(-4, "minecraft:deepslate"));
        secs.push(sec_full(-3, "minecraft:deepslate"));
        secs.push(ocean_floor_section());
        for y in -1..=0 {
            secs.push(sec_full(y, "minecraft:water"));
        }
        secs.push(wreck_hull_section(cx, cz));
        secs.push(wreck_deck_section(cx, cz));
        secs.push(ocean_surface_section());
        for y in 4..=5 {
            secs.push(sec_empty(y));
        }
        // the structure start carries chunkX/chunkZ — recipe identity breaks
        // exactly the way vanilla structure starts do
        let over: Vec<(usize, u16)> = (0..40)
            .map(|i| {
                (
                    (h2(cx, cz, 0x2F0 + i) % 256) as usize,
                    (floor + 6 + (h2(cx, cz, 0x3F0 + i) % 4) as i32) as u16,
                )
            })
            .collect();
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_over(63, &over)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_over(63, &over)),
            ("OCEAN_FLOOR", hm_const((floor + 1) as u16)),
            ("WORLD_SURFACE", hm_over(63, &over)),
        ]);
        let mut start: Map<String, Value> = Map::new();
        start.insert("id".to_string(), Value::String("minecraft:shipwreck".to_string()));
        start.insert("chunkX".to_string(), Value::Int(cx));
        start.insert("chunkZ".to_string(), Value::Int(cz));
        start.insert("Biome".to_string(), Value::String("minecraft:ocean".to_string()));
        let mut starts: Map<String, Value> = Map::new();
        starts.insert("minecraft:shipwreck".to_string(), Value::Compound(start));
        chunk_nbt(cx, cz, secs, hm, Some(Value::Compound(starts)))
    }

    fn beach(&mut self, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (Class::Beach as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let g = 64 + var as i32; // surface world-y
        let top = g % 16;
        let mut secs: Vec<Value> = Vec::with_capacity(10);
        secs.push(bedrock_section());
        secs.push(sec_full(-4, "minecraft:deepslate"));
        secs.push(sec_full(-3, "minecraft:deepslate"));
        for y in -2..4 {
            secs.push(sec_full(y, "minecraft:stone"));
        }
        let mut vals = vec![3u8; 4096];
        for (i, v) in vals.iter_mut().enumerate() {
            let y = (i >> 8) as i32;
            *v = if y > top {
                3
            } else if y == top {
                0 // sand
            } else if y >= top - 2 {
                1 // sandstone
            } else {
                2
            };
        }
        secs.push(sec_mixed(
            4,
            &["minecraft:sand", "minecraft:sandstone", "minecraft:stone", "minecraft:air"],
            &vals,
        ));
        for y in 5..=6 {
            secs.push(sec_empty(y));
        }
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const((g + 1) as u16)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const((g + 1) as u16)),
            ("OCEAN_FLOOR", hm_const((g + 1) as u16)),
            ("WORLD_SURFACE", hm_const((g + 1) as u16)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn plains(&mut self, cls: Class, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (cls as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let lv = (var % 4) as i32;
        let g = 64 + lv; // ground world-y
        let mut secs: Vec<Value> = Vec::with_capacity(11);
        secs.push(bedrock_section());
        secs.push(sec_full(-4, "minecraft:deepslate"));
        secs.push(sec_full(-3, "minecraft:deepslate"));
        secs.push(sec_full(-2, "minecraft:stone"));
        secs.push(sec_full(-1, "minecraft:stone"));
        secs.push(sec_full(0, "minecraft:stone"));
        secs.push(plains_dirt_section());
        secs.push(plains_surface_section((g % 16) as usize, None));
        let mut mb = g + 1;
        if cls == Class::PlainsTree {
            secs.push(tree_section(var / 4));
        }
        secs.push(sec_empty(6));
        secs.push(sec_empty(7));
        if cls == Class::PlainsTree {
            mb = g + 8;
        }
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(mb as u16)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(if mb > g + 1 { g + 2 } else { mb } as u16)),
            ("OCEAN_FLOOR", hm_const((g + 1) as u16)),
            ("WORLD_SURFACE", hm_const(mb as u16)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn mountain(&self, cx: i32, cz: i32) -> Vec<u8> {
        // unique per chunk: ridge height + unique band/surface sections
        let h = h2(cx, cz, 7);
        let top_y = 96 + (h % 64) as i32; // ridge world-y
        let snow = top_y > 144;
        let y_top = top_y / 16;
        let mut secs: Vec<Value> = Vec::with_capacity(20);
        secs.push(bedrock_section());
        secs.push(sec_full(-4, "minecraft:deepslate"));
        secs.push(sec_full(-3, "minecraft:deepslate"));
        secs.push(sec_full(-2, "minecraft:deepslate"));
        for y in -1..y_top - 1 {
            secs.push(sec_full(y, "minecraft:stone"));
        }
        secs.push(mountain_band_section(y_top - 1, cx, cz));
        secs.push(mountain_surface_section(y_top, cx, cz, (top_y % 16) as usize, snow));
        secs.push(sec_empty(y_top + 1));
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const((top_y + 1) as u16)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const((top_y + 1) as u16)),
            ("OCEAN_FLOOR", hm_const((top_y + 1) as u16)),
            ("WORLD_SURFACE", hm_const((top_y + 1) as u16)),
        ]);
        chunk_nbt(cx, cz, secs, hm, None)
    }

    pub fn payload(&mut self, cx: i32, cz: i32) -> (Vec<u8>, Class) {
        let (cls, var) = self.classify(cx, cz);
        let p = match cls {
            Class::OceanBare | Class::OceanKelp => self.ocean_shared(cls, var, cx, cz),
            Class::OceanWreck => self.ocean_wreck(cx, cz),
            Class::Beach => self.beach(var, cx, cz),
            Class::Plains | Class::PlainsFlower | Class::PlainsTree | Class::River => {
                self.plains(cls, var, cx, cz)
            }
            Class::Mountain => self.mountain(cx, cz),
        };
        (p, cls)
    }
}

/// Patch the root xPos/zPos Int values (the ONLY per-chunk bytes in shared
/// shapes — the exact structure 0.9.102's pos-free tails eat).
fn patch_coords(p: &[u8], cx: i32, cz: i32) -> Vec<u8> {
    let mut out = p.to_vec();
    for (tag, v) in [(&b"xPos"[..], cx), (&b"zPos"[..], cz)] {
        let pat = [3u8, 0, 4, tag[0], tag[1], tag[2], tag[3]];
        let mut at = None;
        let mut i = 0usize;
        while i + 8 <= out.len() {
            if &out[i..i + 7] == &pat {
                at = Some(i + 7);
                break;
            }
            i += 1;
        }
        if let Some(a) = at {
            // NBT Int is BIG-endian (the store's key-verify compares BE)
            out[a..a + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
    out
}

// ------------------------------------------------------- mca writer

fn write_region(dir: &Path, rx: i32, rz: i32, chunks: &[(usize, &[u8])]) -> u64 {
    let mut data = vec![0u8; 8192];
    let mut next_sector = 2usize;
    for (slot, p) in chunks {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
        enc.write_all(p).unwrap();
        let comp = enc.finish().unwrap();
        let len = 1 + comp.len();
        let sectors = (4 + len + 4095) / 4096;
        let loc = ((next_sector as u32) << 8) | (sectors.min(255) as u32);
        data[slot * 4..slot * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        data[4096 + slot * 4..4096 + slot * 4 + 4].copy_from_slice(&1u32.to_be_bytes());
        data.resize(next_sector * 4096 + sectors * 4096, 0);
        let base = next_sector * 4096;
        data[base..base + 4].copy_from_slice(&(len as u32).to_be_bytes());
        data[base + 4] = 2;
        data[base + 5..base + 5 + comp.len()].copy_from_slice(&comp);
        next_sector += sectors;
    }
    std::fs::write(dir.join(format!("r.{rx}.{rz}.mca")), &data).unwrap();
    data.len() as u64
}

// ------------------------------------------------------------- main

fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
    let mut opts =
        liblzma::stream::LzmaOptions::new_preset(liblzma::stream::PRESET_EXTREME | 6).ok()?;
    opts.literal_context_bits(4).literal_position_bits(0).position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

fn z19(data: &[u8]) -> usize {
    let mut c = zstd::Encoder::new(Vec::new(), 19).unwrap();
    c.write_all(data).unwrap();
    c.finish().unwrap().len()
}

fn main() {
    let cx_w: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(96);
    let cz_w: i32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(96);
    let out = PathBuf::from(
        std::env::var("CSAVES_TECTONIC_OUT").unwrap_or_else(|_| "/tmp/tectonic_world".to_string()),
    );
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(out.join("region")).unwrap();

    // 1) build every payload (shared shapes patched per coordinate)
    let t0 = std::time::Instant::now();
    let mut b = Builder::new(cx_w);
    let mut payloads: Vec<(u64, Vec<u8>, Class)> = Vec::with_capacity((cx_w * cz_w) as usize);
    let mut class_n: Map<&'static str, usize> = Map::new();
    let mut raw = 0u64;
    for cz in 0..cz_w {
        for cx in 0..cx_w {
            let (p, cls) = b.payload(cx, cz);
            raw += p.len() as u64;
            *class_n.entry(cls.name()).or_default() += 1;
            payloads.push((((cx as u64) << 32) | (cz as u32) as u64, p, cls));
        }
    }
    println!("== plan ==");
    println!(
        "chunks {}x{} = {} | gen {:.1?} | raw {:.2} MiB ({:.2} KiB/chunk)",
        cx_w,
        cz_w,
        payloads.len(),
        t0.elapsed(),
        raw as f64 / 1048576.0,
        raw as f64 / 1024.0 / payloads.len() as f64
    );
    let mut classes: Vec<(&'static str, usize)> = class_n.into_iter().collect();
    classes.sort_by_key(|c| std::cmp::Reverse(c.1));
    for (k, v) in &classes {
        println!("  {:<14} {:>6} ({:4.1}%)", k, v, 100.0 * *v as f64 / payloads.len() as f64);
    }

    // 2) Anvil baseline: real region files of the same payloads (zlib-9)
    let mut regions: Map<(i32, i32), Vec<(usize, usize)>> = Map::new();
    for (i, (k, _, _)) in payloads.iter().enumerate() {
        let cx = (k >> 32) as i32;
        let cz = (k & 0xffff_ffff) as u32 as i32;
        regions
            .entry((cx.div_euclid(32), cz.div_euclid(32)))
            .or_default()
            .push((((cx.rem_euclid(32)) + (cz.rem_euclid(32)) * 32) as usize, i));
    }
    let mut anvil = 0u64;
    let mut rkeys: Vec<(i32, i32)> = regions.keys().copied().collect();
    rkeys.sort();
    for rk in &rkeys {
        let slots: Vec<(usize, &[u8])> = regions[rk]
            .iter()
            .map(|(s, i)| (*s, payloads[*i].1.as_slice()))
            .collect();
        anvil += write_region(&out.join("region"), rk.0, rk.1, &slots);
    }
    println!("== anvil ==");
    println!(
        "{} region files: {:.2} MiB on disk ({:.0} B/chunk) — zlib-9, real bytes",
        rkeys.len(),
        anvil as f64 / 1048576.0,
        anvil as f64 / payloads.len() as f64
    );

    // 3) c-saves: production import path (default cfg = REAL storage; the
    //    contract/regeneration mode stays off by operator directive)
    let store_dir = out.join("store");
    let st = Store::open_cfg(&store_dir, true, Cfg::from_env()).unwrap();
    let t0 = std::time::Instant::now();
    let mut keys: Vec<u64> = payloads.iter().map(|(k, _, _)| *k).collect();
    keys.sort();
    let by_key: Map<u64, usize> =
        payloads.iter().enumerate().map(|(i, (k, _, _))| (*k, i)).collect();
    for &k in &keys {
        let p = &payloads[by_key[&k]].1;
        st.write_chunk_import(k, p).unwrap();
    }
    let import_t = t0.elapsed();
    let t0 = std::time::Instant::now();
    st.sync();
    st.seal();
    st.request_pack();
    // the maintenance thread keeps stepping in production; mirror that here
    // (a single while-loop can exit through the fallback path before the
    // dead-ratio rewrite gets its turn — observed on the NOHM A/B leg)
    for _ in 0..8 {
        while st.compact_step() {}
    }
    st.sync();
    let pack_t = t0.elapsed();
    let container = st.disk_bytes();
    println!("== c-saves ==");
    println!(
        "import {:.1?} ({:.1} MiB/s) | drain {:.1?} | container {:.2} MiB = {} B",
        import_t,
        raw as f64 / 1048576.0 / import_t.as_secs_f64(),
        pack_t,
        container as f64 / 1048576.0,
        container
    );
    println!(
        "{:.1} B/chunk | ratio vs anvil x{:.2}",
        container as f64 / payloads.len() as f64,
        anvil as f64 / container.max(1) as f64
    );

    // 4) verify every read against the generated payload. The equality
    //    contract is the CI verify's own: compound fields as plain Values,
    //    sections via the palette-permutation-safe comparator (the canonical
    //    CSEC re-encode reorders palettes by frequency — semantically
    //    neutral). Byte-exact is reported separately.
    let chunks_semantic = |a: &[u8], b: &[u8]| -> bool {
        let (Ok(Value::Compound(ma)), Ok(Value::Compound(mb))) =
            (fastnbt::from_bytes::<Value>(a), fastnbt::from_bytes::<Value>(b))
        else {
            return false;
        };
        if ma.len() != mb.len() {
            return false;
        }
        for (k, va) in &ma {
            let Some(vb) = mb.get(k) else { return false };
            if k == "sections" {
                let (Value::List(la), Value::List(lb)) = (va, vb) else { return false };
                if la.len() != lb.len() {
                    return false;
                }
                for (sa, sb) in la.iter().zip(lb.iter()) {
                    if !c_saves::chunknbt::sections_semantically_equal(sa, sb) {
                        return false;
                    }
                }
            } else if va != vb {
                return false;
            }
        }
        true
    };
    let t0 = std::time::Instant::now();
    let (mut ok_s, mut ok_b, mut miss) = (0usize, 0usize, 0usize);
    for &k in &keys {
        let want = &payloads[by_key[&k]].1;
        match st.read_chunk(k) {
            Some(got) => {
                if chunks_semantic(want, &got) {
                    ok_s += 1;
                }
                if got == *want {
                    ok_b += 1;
                }
            }
            None => miss += 1,
        }
    }
    println!(
        "verify: {}/{} semantic OK ({} byte-exact, {} miss) in {:.1?}",
        ok_s,
        payloads.len(),
        ok_b,
        miss,
        t0.elapsed()
    );

    // 5) per-class drained attribution
    println!("== slab attribution ==");
    println!("{}", st.slab_member_attribution_json());
    println!("== profile ==");
    println!("{}", st.profile_json());

    // 6) heightmap columnar pricing on THIS corpus: the tail wire (root
    //    minus sections) with vs without the Heightmaps compound, both under
    //    one big LZMA2 window; unique heightmap wires columnar + 4 B refs.
    let mut tails_all: Vec<u8> = Vec::new();
    let mut tails_no: Vec<u8> = Vec::new();
    let mut hm_seen: Map<u128, ()> = Map::new();
    let mut hm_uniq: Vec<Vec<u8>> = Vec::new();
    let mut n_hm = 0usize;
    let mut per_chunk_z = 0usize;
    for (k, _, _) in payloads.iter() {
        let i = by_key[k];
        let mut root: Value = fastnbt::from_bytes(&payloads[i].1).unwrap();
        // strip sections first: the tail wire is the root compound minus them
        if let Value::Compound(m) = &mut root {
            m.remove("sections");
        }
        tails_all.extend_from_slice(&fastnbt::to_bytes(&root).unwrap());
        if let Value::Compound(m) = &mut root {
            let hm = m.remove("Heightmaps");
            if let Some(hm) = hm {
                let w = fastnbt::to_bytes(&hm).unwrap();
                per_chunk_z += z19(&w);
                let h = (hash_of(&w) as u128) << 64 | w.len() as u128;
                if !hm_seen.contains_key(&h) {
                    hm_seen.insert(h, ());
                    hm_uniq.push(w);
                }
                n_hm += 1;
            }
        }
        tails_no.extend_from_slice(&fastnbt::to_bytes(&root).unwrap());
    }
    let lz_all = lzma2_raw(&tails_all).map(|b| b.len()).unwrap_or(0);
    let lz_no = lzma2_raw(&tails_no).map(|b| b.len()).unwrap_or(0);
    let hm_col: Vec<u8> = hm_uniq.concat();
    let lz_hm = lzma2_raw(&hm_col).map(|b| b.len()).unwrap_or(0);
    let refs = 4 * n_hm;
    println!("== heightmap columnar pricing ==");
    println!(
        "tails: {} chunks, all {:.2} KiB -> lz {} B; no-hm {:.2} KiB -> lz {} B",
        payloads.len(),
        tails_all.len() as f64 / 1024.0,
        lz_all,
        tails_no.len() as f64 / 1024.0,
        lz_no
    );
    println!(
        "heightmaps: {n_hm} carry, {} unique, sum z19 {per_chunk_z} B; unique-columnar lz {lz_hm} B + refs {refs} B = {} B",
        hm_uniq.len(),
        lz_hm + refs,
    );
    println!(
        "tier delta under big-window model: lz_all {} vs (lz_no {} + col {}) = {} -> {:+} B ({:.2}%)",
        lz_all,
        lz_no,
        lz_hm + refs,
        lz_no + lz_hm + refs,
        (lz_no + lz_hm + refs) as i64 - lz_all as i64,
        100.0 * ((lz_no + lz_hm + refs) as i64 - lz_all as i64) as f64 / lz_all.max(1) as f64
    );

    // 7) honest extrapolation: linear per-chunk marginal on the measured
    //    container (the mix, not the area, decides the mass)
    let per_chunk = container as f64 / payloads.len() as f64;
    let anvil_pc = anvil as f64 / payloads.len() as f64;
    let world = 20_000.0f64 * 20_000.0;
    println!("== 20k x 20k extrapolation (same mix, linear) ==");
    println!("measured: {:.0} B/chunk c-saves vs {:.0} B/chunk anvil", per_chunk, anvil_pc);
    println!(
        "20k x 20k = 400M chunks: c-saves ~= {:.1} GiB | anvil ~= {:.2} TiB (x{:.0})",
        per_chunk * world / 1073741824.0,
        anvil_pc * world / 1099511627776.0,
        anvil_pc / per_chunk
    );
}
