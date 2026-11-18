//! tectonic_corpus: the shared Tectonic-shaped corpus builder (structural
//! mimicry of a real pregen: bare-ocean fields, repeating decoration
//! variants, honest wreck starts, gradient mountain strata). Extracted from
//! tectonic_probe so every probe drives the SAME generator (A/B stays
//! comparable with the shipped numbers).

use c_saves::chunknbt::hash_of;
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

/// mountain mid band: stone/andesite/gravel bands over a SMOOTH gradient
/// (real Tectonic ridges are smooth — the band pattern runs in long rows;
/// per-column hash noise would overstate section uniqueness)
fn mountain_band_section(y: i32, cx: i32, cz: i32) -> Value {
    let (gx, gz) = ridge_gradient(cx, cz);
    let mut vals = vec![0u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        // band boundary drifts smoothly across the section; rare deterministic
        // intrusions (gravel pockets) stay sparse like real mountain rock
        let band = yy * 4 + ((x - 8) * gx + (z - 8) * gz) / 4;
        let r = h2(cx * 16 + x, cz * 16 + z, 0xB1 + yy as u64) % 100;
        *v = if r < 4 {
            2 // sparse gravel pocket
        } else if band % 7 < 2 {
            1 // andesite stratum
        } else {
            0 // stone
        };
    }
    sec_mixed(y, &["minecraft:stone", "minecraft:andesite", "minecraft:gravel"], &vals)
}

/// integer gradient of the ridge at a chunk: smooth across neighbors, small
/// deterministic variation per chunk — the terrain continues coherently.
fn ridge_gradient(cx: i32, cz: i32) -> (i32, i32) {
    let h = h2(cx, cz, 0x610);
    ((h % 5) as i32 - 2, ((h >> 8) % 5) as i32 - 2)
}

/// mountain surface section: ridge top at row `top`; the top boundary
/// drifts SMOOTHLY across x/z (integer gradient — real ridges are smooth
/// lines, not per-column noise); snow cap above y=144.
fn mountain_surface_section(y: i32, cx: i32, cz: i32, top: usize, snow: bool) -> Value {
    let top = top as i32;
    let (gx, gz) = ridge_gradient(cx, cz);
    let mut vals = vec![3u8; 4096]; // air above the ridge
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        // smooth boundary drift across the section + rare deterministic dips
        let d = ((x - 8) * gx + (z - 8) * gz) / 6 + if h2(cx * 16 + x, cz * 16 + z, 0x51F) % 100 < 8 { -1 } else { 0 };
        let t = top + d;
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
    pub fn name(self) -> &'static str {
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

    pub fn classify(&self, cx: i32, cz: i32) -> (Class, u64) {
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

pub fn write_region(dir: &Path, rx: i32, rz: i32, chunks: &[(usize, &[u8])]) -> u64 {
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

pub fn lzma2_raw(data: &[u8]) -> Option<Vec<u8>> {
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

pub fn z19(data: &[u8]) -> usize {
    let mut c = zstd::Encoder::new(Vec::new(), 19).unwrap();
    c.write_all(data).unwrap();
    c.finish().unwrap().len()
}
