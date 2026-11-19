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
/// (real Tectonic ridges are smooth — the band pattern runs in long rows).
/// v0.9.110 corpus calibration: the intrusions are VEIN-CLUSTERED — vanilla
/// worldgen lays gravel/andesite as CONTIGUOUS blobs (VeinSize 8-32), not
/// per-column white noise; the white model overstated strata entropy (the
/// LZ window lost a match every ~25 columns to an isolated literal). 1-3
/// vein blobs per section, radius 2-4 columns, deterministic per chunk.
fn mountain_band_section(y: i32, cx: i32, cz: i32) -> Value {
    let (gx, gz) = ridge_gradient(cx, cz);
    let vein_n = (h2(cx, cz, 0xB2) % 3) as i32 + 1;
    let mut veins: Vec<(i32, i32, i32)> = Vec::with_capacity(vein_n as usize);
    for v in 0..vein_n {
        let h = h2(cx, cz, 0xB3 + v as u64);
        veins.push(((h % 16) as i32, ((h >> 8) % 16) as i32, 2 + ((h >> 16) % 3) as i32));
    }
    let mut vals = vec![0u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        // band boundary drifts smoothly across the section; the gravel veins
        // cluster like vanilla worldgen blobs
        let band = yy * 4 + ((x - 8) * gx + (z - 8) * gz) / 4;
        let in_vein = veins
            .iter()
            .any(|(vx, vz, r)| (x - vx) * (x - vx) + (z - vz) * (z - vz) <= r * r);
        *v = if in_vein {
            2 // gravel vein blob
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
/// lines); v0.9.110: the dips cluster into 1-2 erosion arcs per chunk
/// (real erosion gullies are contiguous, not per-column noise); snow cap
/// above y=144.
fn mountain_surface_section(y: i32, cx: i32, cz: i32, top: usize, snow: bool) -> Value {
    let top = top as i32;
    let (gx, gz) = ridge_gradient(cx, cz);
    // erosion dips: 1-2 arcs of contiguous columns (a gully cross-section),
    // deterministic per chunk
    let dip_n = (h2(cx, cz, 0x51E) % 2) as i32 + 1;
    let mut dips: Vec<(i32, i32, i32)> = Vec::with_capacity(dip_n as usize); // (x, z, radius)
    for v in 0..dip_n {
        let h = h2(cx, cz, 0x51F + v as u64);
        dips.push(((h % 16) as i32, ((h >> 8) % 16) as i32, 1 + ((h >> 16) % 2) as i32));
    }
    let mut vals = vec![3u8; 4096]; // air above the ridge
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let yy = (i >> 8) as i32;
        // smooth boundary drift across the section + clustered erosion dips
        let in_dip = dips
            .iter()
            .any(|(dx, dz, r)| (x - dx) * (x - dx) + (z - dz) * (z - dz) <= r * r);
        let d = ((x - 8) * gx + (z - 8) * gz) / 6 + if in_dip { -1 } else { 0 };
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

// ================================================== nether builders
//
// The nether is a CAVERN: bedrock floor + bedrock ceiling, netherrack
// strata with clustered vein blobs (the same calibrated vanilla-vein
// model as the overworld mountains), a lava sea filling the open space
// below y=10, glowstone hanging under the ceiling, fortresses crossing
// chunks with real structure starts. Sections y=0..7 (128 tall).
// Heightmaps are CONST 128 in the whole dimension — the bedrock ceiling
// is the top motion-blocking surface from above (true vanilla behavior),
// so the tail dedups globally.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum NetherClass {
    LavaSea,
    NetherPlains,
    SoulValley,
    GlowCeiling,
    Fortress,
}

impl NetherClass {
    pub fn name(self) -> &'static str {
        match self {
            NetherClass::LavaSea => "nether_lava_sea",
            NetherClass::NetherPlains => "nether_plains",
            NetherClass::SoulValley => "nether_soul_valley",
            NetherClass::GlowCeiling => "nether_glow_ceiling",
            NetherClass::Fortress => "nether_fortress",
        }
    }
}

const NB_NAMES: &[&str] = &[
    "minecraft:netherrack",   // 0
    "minecraft:blackstone",   // 1
    "minecraft:gravel",       // 2
    "minecraft:soul_sand",    // 3
    "minecraft:glowstone",    // 4
    "minecraft:lava",         // 5
    "minecraft:air",          // 6
    "minecraft:bedrock",      // 7
    "minecraft:nether_bricks", // 8
];

fn nether_gradient(cx: i32, cz: i32, salt: u64) -> (i32, i32) {
    let h = h2(cx, cz, salt);
    ((h % 3) as i32 - 1, ((h >> 8) % 3) as i32 - 1)
}

/// cavern terrain: below `floor` = netherrack with clustered vein blobs,
/// row `floor` = surface block, above = air.
fn nether_terrain_section(
    y: i32,
    cx: i32,
    cz: i32,
    floor: i32,
    surface: u8,
    salt: u64,
) -> Value {
    let (gx, gz) = nether_gradient(cx, cz, salt);
    let vein_n = (h2(cx, cz, salt ^ 0xB2) % 3) as i32 + 1;
    let mut veins: Vec<(i32, i32, i32, u8)> = Vec::with_capacity(vein_n as usize);
    for v in 0..vein_n {
        let h = h2(cx, cz, (salt ^ 0xB3) + v as u64);
        veins.push((
            (h % 16) as i32,
            ((h >> 8) % 16) as i32,
            2 + ((h >> 16) % 3) as i32,
            if (h >> 24) % 2 == 0 { 1 } else { 2 },
        ));
    }
    let lo = y * 16;
    let mut vals = vec![6u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        let drift = ((x - 8) * gx + (z - 8) * gz) / 6;
        let f = floor + drift;
        let mut vb = 1u8;
        let in_vein = veins.iter().any(|(vx, vz, r, b)| {
            if (x - vx) * (x - vx) + (z - vz) * (z - vz) <= r * r {
                vb = *b;
                true
            } else {
                false
            }
        });
        *v = if w < f {
            if in_vein {
                vb
            } else {
                0 // netherrack
            }
        } else if w == f {
            surface
        } else {
            6 // air
        };
    }
    sec_mixed(y, NB_NAMES, &vals)
}

/// y=0 of a lava sea: bedrock rows + netherrack up to row 9, lava rows
/// 10..=15 — the byte-identical mass of the whole nether.
fn nether_lava_floor_section() -> Value {
    let mut vals = vec![5u8; 4096]; // lava
    for (i, v) in vals.iter_mut().enumerate() {
        let yy = (i >> 8) as i32;
        if yy <= 9 {
            *v = 0; // netherrack
        }
        if yy == 0 || (yy == 1 && h2(i as i32, 7, 0xB3D) % 5 < 2) {
            *v = 7; // bedrock
        }
    }
    sec_mixed(0, NB_NAMES, &vals)
}

/// y=0 of the solid floor: bedrock noise over netherrack — shared shape
/// across every non-lava class.
fn nether_bedrock_floor_section() -> Value {
    let mut vals = vec![0u8; 4096]; // netherrack
    for (i, v) in vals.iter_mut().enumerate() {
        let yy = (i >> 8) as i32;
        if yy == 0 || (yy == 1 && h2(i as i32, 7, 0xB3D) % 5 < 2) {
            *v = 7; // bedrock
        }
    }
    sec_mixed(0, NB_NAMES, &vals)
}

/// y=7: netherrack rows 0..=13 + bedrock rows 14..=15 — shared everywhere.
fn nether_ceiling_section() -> Value {
    let mut vals = vec![0u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        let yy = (i >> 8) as i32;
        if yy >= 14 {
            *v = 7; // bedrock
        }
    }
    sec_mixed(7, NB_NAMES, &vals)
}

/// ceiling boundary: netherrack above `ceil`, air below; glowstone
/// clusters hang under the ceiling (GlowCeiling variant sprinkle).
fn nether_ceiling_boundary_section(
    y: i32,
    cx: i32,
    cz: i32,
    ceil: i32,
    glow: Option<u64>,
) -> Value {
    let (gx, gz) = nether_gradient(cx, cz, 0xCE1);
    let cl_n = (h2(cx, cz, 0x61A) % 3) as i32 + 1;
    let mut cls: Vec<(i32, i32, i32)> = Vec::with_capacity(cl_n as usize);
    if let Some(var) = glow {
        for v in 0..cl_n {
            let h = h2(cx, cz, (0x61B + v as u64) ^ var);
            cls.push(((h % 16) as i32, ((h >> 8) % 16) as i32, 1 + ((h >> 16) % 2) as i32));
        }
    }
    let lo = y * 16;
    let mut vals = vec![6u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        let drift = ((x - 8) * gx + (z - 8) * gz) / 6;
        let c = ceil + drift;
        let in_cl = cls
            .iter()
            .any(|(cx2, cz2, r)| (x - cx2) * (x - cx2) + (z - cz2) * (z - cz2) <= r * r);
        *v = if w >= c {
            0 // netherrack ceiling mass
        } else if in_cl && w >= c - 3 {
            4 // glowstone
        } else {
            6 // air
        };
    }
    sec_mixed(y, NB_NAMES, &vals)
}

/// fortress walkway: a nether-brick box crossing the chunk (floor, roof,
/// parapet walls), unique per chunk and per span — vanilla fortress
/// corridors never repeat byte-identically.
fn fortress_section(y: i32, cx: i32, cz: i32, span: u64) -> Value {
    let bx = 2 + (h2(cx, cz, 0xF04 + span) % 3) as usize;
    let bz = 4 + (h2(cx, cz, 0xF14 + span) % 3) as usize;
    let floor_row = 4usize;
    let mut vals = vec![6u8; 4096]; // air (the cavern)
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as usize;
        let z = ((i >> 4) & 15) as usize;
        let yy = (i >> 8) as usize;
        let in_x = x >= bx && x < bx + 11;
        let in_z = z >= bz && z < bz + 8;
        if !(in_x && in_z) {
            continue;
        }
        let edge = x == bx || x == bx + 10 || z == bz || z == bz + 7;
        if yy == floor_row || yy == floor_row + 5 {
            *v = 8; // nether bricks
        } else if yy > floor_row && yy < floor_row + 5 && edge {
            *v = 8;
        }
    }
    sec_mixed(y, NB_NAMES, &vals)
}

pub struct NetherBuilder {
    cache: Map<(u8, u64), Vec<u8>>,
    cx_w: i32,
}

impl NetherBuilder {
    pub fn new(cx_w: i32) -> Self {
        NetherBuilder { cache: Map::new(), cx_w }
    }

    pub fn classify(&self, cx: i32, cz: i32) -> (NetherClass, u64) {
        let h = h2(cx, cz, 0x7E7);
        if h % 100 < 2 {
            (NetherClass::Fortress, 0)
        } else if cx < (self.cx_w * 5) / 8 {
            // the lava-sea band mirrors the overworld ocean band: the vast
            // flat mass whose chunks repeat across coordinates
            (NetherClass::LavaSea, 0)
        } else if h % 100 < 15 {
            (NetherClass::SoulValley, (h >> 8) % 6)
        } else if h % 100 < 27 {
            (NetherClass::GlowCeiling, (h >> 16) % 6)
        } else {
            (NetherClass::NetherPlains, (h >> 24) % 6)
        }
    }

    fn nether_heightmaps() -> Value {
        heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(128)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(128)),
            ("OCEAN_FLOOR", hm_const(128)),
            ("WORLD_SURFACE", hm_const(128)),
        ])
    }

    fn shared(&mut self, cls: NetherClass, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (cls as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let floor: i32 = if cls == NetherClass::SoulValley { 26 } else { 28 };
        let ceil: i32 = 104 + (var as i32 % 3) * 4;
        let surface: u8 = if cls == NetherClass::SoulValley { 3 } else { 0 };
        let salt = 0x100 + var * 0x10;
        let mut secs: Vec<Value> = Vec::with_capacity(8);
        if cls == NetherClass::LavaSea {
            secs.push(nether_lava_floor_section());
        } else {
            secs.push(nether_bedrock_floor_section());
        }
        secs.push(nether_terrain_section(1, cx, cz, floor, surface, salt));
        secs.push(nether_terrain_section(2, cx, cz, floor, surface, salt));
        secs.push(sec_empty(3));
        secs.push(sec_empty(4));
        secs.push(sec_empty(5));
        secs.push(nether_ceiling_boundary_section(
            6,
            cx,
            cz,
            ceil,
            if cls == NetherClass::GlowCeiling { Some(var) } else { None },
        ));
        secs.push(nether_ceiling_section());
        let p = chunk_nbt(cx, cz, secs, Self::nether_heightmaps(), None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn fortress(&mut self, cx: i32, cz: i32) -> Vec<u8> {
        let floor = 28;
        let mut secs: Vec<Value> = Vec::with_capacity(8);
        secs.push(nether_bedrock_floor_section());
        secs.push(nether_terrain_section(1, cx, cz, floor, 0, 0x190));
        secs.push(nether_terrain_section(2, cx, cz, floor, 0, 0x190));
        secs.push(fortress_section(3, cx, cz, 0));
        secs.push(fortress_section(4, cx, cz, 1));
        secs.push(sec_empty(5));
        secs.push(nether_ceiling_boundary_section(6, cx, cz, 104, None));
        secs.push(nether_ceiling_section());
        // the structure start carries chunkX/chunkZ — recipe identity
        // breaks the way vanilla fortress starts do
        let mut start: Map<String, Value> = Map::new();
        start.insert("id".to_string(), Value::String("minecraft:fortress".to_string()));
        start.insert("chunkX".to_string(), Value::Int(cx));
        start.insert("chunkZ".to_string(), Value::Int(cz));
        let mut starts: Map<String, Value> = Map::new();
        starts.insert("minecraft:fortress".to_string(), Value::Compound(start));
        chunk_nbt(cx, cz, secs, Self::nether_heightmaps(), Some(Value::Compound(starts)))
    }

    pub fn payload(&mut self, cx: i32, cz: i32) -> (Vec<u8>, NetherClass) {
        let (cls, var) = self.classify(cx, cz);
        let p = match cls {
            NetherClass::Fortress => self.fortress(cx, cz),
            _ => self.shared(cls, var, cx, cz),
        };
        (p, cls)
    }
}

// ===================================================== end builders
//
// The End is a VOID with islands: pure-void chunks have NO sections at
// all (vanilla omits empty sections; the whole chunk is a bare tail),
// the main island is flat endstone slabs, outer islands repeat a small
// family of shapes across coordinates (real outer-end island shapes),
// obsidian pillars rise from the void with height variants, end cities
// break identity with real structure starts. Sections y=0..15 (256 tall).

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EndClass {
    Void,
    MainIsland,
    OuterIsland,
    Pillar,
    EndCity,
}

impl EndClass {
    pub fn name(self) -> &'static str {
        match self {
            EndClass::Void => "end_void",
            EndClass::MainIsland => "end_main_island",
            EndClass::OuterIsland => "end_outer_island",
            EndClass::Pillar => "end_pillar",
            EndClass::EndCity => "end_city",
        }
    }
}

const END_NAMES: &[&str] = &[
    "minecraft:endstone",        // 0
    "minecraft:obsidian",        // 1
    "minecraft:purpur_block",    // 2
    "minecraft:end_stone_bricks", // 3
    "minecraft:air",             // 4
];

fn end_island_core(y: i32, var: u64, bot_row: i32, top_row: i32) -> Value {
    // endstone ellipse slab: rows bot..=top inside the ellipse (the outer
    // island shape family — a handful of seeds repeating across the void)
    let rx = 4 + (var % 3) as i32;
    let rz = 4 + ((var >> 4) % 3) as i32;
    let lo = y * 16;
    let mut vals = vec![4u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        let inside = ((x - 7) * (x - 7)) * (rz * rz) + ((z - 7) * (z - 7)) * (rx * rx)
            <= (rx * rz) * (rx * rz);
        if inside && w >= lo + bot_row && w <= lo + top_row {
            *v = 0; // endstone
        }
    }
    sec_mixed(y, END_NAMES, &vals)
}

fn end_main_surface(y: i32) -> Value {
    // flat main-island top: endstone row 0, air above
    let mut vals = vec![4u8; 4096];
    for (i, v) in vals.iter_mut().enumerate() {
        if (i >> 8) == 0 {
            *v = 0; // endstone
        }
    }
    sec_mixed(y, END_NAMES, &vals)
}

fn end_pillar_section(y: i32, bot: i32, top: i32) -> Value {
    // obsidian column, radius 2 at the chunk center, from y=76 up to top
    let lo = y * 16;
    let mut vals = vec![4u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        if (x - 7) * (x - 7) + (z - 7) * (z - 7) <= 4 && w >= bot && w <= top {
            *v = 1; // obsidian
        }
    }
    sec_mixed(y, END_NAMES, &vals)
}

/// end-city ground floor: the island slab (rows up to 56) MERGED with the
/// tower base (a chunk has ONE section per Y).
fn end_city_base_section(y: i32, var: u64, city_top: i32) -> Value {
    let rx = 4 + (var % 3) as i32;
    let rz = 4 + ((var >> 4) % 3) as i32;
    let lo = y * 16;
    let mut vals = vec![4u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        let inside = ((x - 7) * (x - 7)) * (rz * rz) + ((z - 7) * (z - 7)) * (rx * rx)
            <= (rx * rz) * (rx * rz);
        if inside && w <= 56 {
            *v = 0; // endstone island
            continue;
        }
        let box_ = (x - 7).abs() <= 3 && (z - 7).abs() <= 3;
        let ring = (x - 7).abs() == 3 || (z - 7).abs() == 3;
        if box_ && w >= 56 && w <= city_top {
            if ring {
                *v = 2; // purpur walls
            } else if w == 56 || w % 4 == 0 {
                *v = 3; // end stone brick floors
            }
        }
    }
    sec_mixed(y, END_NAMES, &vals)
}

fn end_city_tower_section(y: i32, city_top: i32) -> Value {
    let lo = y * 16;
    let mut vals = vec![4u8; 4096]; // air
    for (i, v) in vals.iter_mut().enumerate() {
        let x = (i & 15) as i32;
        let z = ((i >> 4) & 15) as i32;
        let w = lo + (i >> 8) as i32;
        let box_ = (x - 7).abs() <= 3 && (z - 7).abs() <= 3;
        let ring = (x - 7).abs() == 3 || (z - 7).abs() == 3;
        if box_ && w <= city_top {
            if ring {
                *v = 2; // purpur
            } else if w % 4 == 0 {
                *v = 3; // floors
            }
        }
    }
    sec_mixed(y, END_NAMES, &vals)
}

pub struct EndBuilder {
    cache: Map<(u8, u64), Vec<u8>>,
    cx_w: i32,
}

impl EndBuilder {
    pub fn new(cx_w: i32) -> Self {
        EndBuilder { cache: Map::new(), cx_w }
    }

    pub fn classify(&self, cx: i32, cz: i32) -> (EndClass, u64) {
        let cc = self.cx_w / 2;
        let d = (cx - cc).abs().max((cz - cc).abs());
        let h = h2(cx, cz, 0xE0);
        if d <= 5 {
            (EndClass::MainIsland, 0)
        } else if d <= 9 && h % 100 < 45 {
            (EndClass::Pillar, (h >> 8) % 3)
        } else if h % 100 < 16 {
            if (h >> 16) % 100 < 22 {
                (EndClass::EndCity, (h >> 24) % 4)
            } else {
                (EndClass::OuterIsland, (h >> 8) % 6)
            }
        } else {
            (EndClass::Void, 0)
        }
    }

    fn void_chunk(&mut self, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (EndClass::Void as u8, 0);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        // vanilla void: NO sections at all, heightmaps all zero
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(0)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(0)),
            ("OCEAN_FLOOR", hm_const(0)),
            ("WORLD_SURFACE", hm_const(0)),
        ]);
        let p = chunk_nbt(cx, cz, Vec::new(), hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn main_island(&mut self, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (EndClass::MainIsland as u8, 0);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let mut secs: Vec<Value> = Vec::with_capacity(16);
        for y in 0..=4 {
            secs.push(sec_full(y, "minecraft:endstone"));
        }
        secs.push(end_main_surface(5));
        for y in 6..=15 {
            secs.push(sec_empty(y));
        }
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(81)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(81)),
            ("OCEAN_FLOOR", hm_const(80)),
            ("WORLD_SURFACE", hm_const(81)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn outer_island(&mut self, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (EndClass::OuterIsland as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let mut secs: Vec<Value> = Vec::with_capacity(16);
        secs.push(sec_empty(0));
        secs.push(sec_empty(1));
        secs.push(end_island_core(2, var, 8, 15));
        secs.push(end_island_core(3, var, 0, 8));
        for y in 4..=15 {
            secs.push(sec_empty(y));
        }
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const(57)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const(57)),
            ("OCEAN_FLOOR", hm_const(56)),
            ("WORLD_SURFACE", hm_const(57)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn pillar(&mut self, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let ck = (EndClass::Pillar as u8, var);
        if let Some(p) = self.cache.get(&ck) {
            return patch_coords(p, cx, cz);
        }
        let top = 96 + var as i32 * 4;
        let mut secs: Vec<Value> = Vec::with_capacity(16);
        for y in 0..=3 {
            secs.push(sec_empty(y));
        }
        secs.push(end_pillar_section(4, 76, top));
        secs.push(end_pillar_section(5, 80, top));
        secs.push(end_pillar_section(6, 96, top));
        for y in 7..=15 {
            secs.push(sec_empty(y));
        }
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_const((top + 1) as u16)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_const((top + 1) as u16)),
            ("OCEAN_FLOOR", hm_const(0)),
            ("WORLD_SURFACE", hm_const((top + 1) as u16)),
        ]);
        let p = chunk_nbt(cx, cz, secs, hm, None);
        self.cache.insert(ck, p.clone());
        p
    }

    fn end_city(&mut self, var: u64, cx: i32, cz: i32) -> Vec<u8> {
        let city_top = 84 + var as i32 * 4;
        let mut secs: Vec<Value> = Vec::with_capacity(16);
        secs.push(sec_empty(0));
        secs.push(sec_empty(1));
        secs.push(end_island_core(2, var, 8, 15));
        secs.push(end_city_base_section(3, var, city_top));
        secs.push(end_city_tower_section(4, city_top));
        secs.push(end_city_tower_section(5, city_top));
        for y in 6..=15 {
            secs.push(sec_empty(y));
        }
        let over: Vec<(usize, u16)> = (0..40)
            .map(|i| {
                (
                    (h2(cx, cz, 0x2E0 + i) % 256) as usize,
                    (city_top + 1) as u16,
                )
            })
            .collect();
        let hm = heightmaps(vec![
            ("MOTION_BLOCKING", hm_over(57, &over)),
            ("MOTION_BLOCKING_NO_LEAVES", hm_over(57, &over)),
            ("OCEAN_FLOOR", hm_over(56, &over)),
            ("WORLD_SURFACE", hm_over(57, &over)),
        ]);
        let mut start: Map<String, Value> = Map::new();
        start.insert("id".to_string(), Value::String("minecraft:end_city".to_string()));
        start.insert("chunkX".to_string(), Value::Int(cx));
        start.insert("chunkZ".to_string(), Value::Int(cz));
        let mut starts: Map<String, Value> = Map::new();
        starts.insert("minecraft:end_city".to_string(), Value::Compound(start));
        chunk_nbt(cx, cz, secs, hm, Some(Value::Compound(starts)))
    }

    pub fn payload(&mut self, cx: i32, cz: i32) -> (Vec<u8>, EndClass) {
        let (cls, var) = self.classify(cx, cz);
        let p = match cls {
            EndClass::Void => self.void_chunk(cx, cz),
            EndClass::MainIsland => self.main_island(cx, cz),
            EndClass::OuterIsland => self.outer_island(var, cx, cz),
            EndClass::Pillar => self.pillar(var, cx, cz),
            EndClass::EndCity => self.end_city(var, cx, cz),
        };
        (p, cls)
    }
}
