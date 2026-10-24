//! Shared Tectonic-like world generator for size benchmarks and profilers.
#![allow(dead_code)]

use fastnbt::Value;
use std::collections::HashMap as Map;

// ---------------------------------------------------------------- noise ----

pub fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn lattice(ix: i64, iz: i64, seed: u64) -> f64 {
    let h = splitmix(
        (ix as u64).wrapping_mul(0x2545F4914F6CDD1D)
            ^ (iz as u64).wrapping_mul(0x9E3779B97F4A7C15)
            ^ seed,
    );
    (h & 0xFFFFF) as f64 / 0x100000 as f64
}

pub fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

pub fn value_noise(x: f64, z: f64, seed: u64) -> f64 {
    let ix = x.floor() as i64;
    let iz = z.floor() as i64;
    let tx = smooth(x - x.floor());
    let tz = smooth(z - z.floor());
    let a = lattice(ix, iz, seed);
    let b = lattice(ix + 1, iz, seed);
    let c = lattice(ix, iz + 1, seed);
    let d = lattice(ix + 1, iz + 1, seed);
    a + (b - a) * tx + (c - a) * tz + (a - b - c + d) * tx * tz
}

pub fn fbm(x: f64, z: f64, seed: u64) -> f64 {
    0.55 * value_noise(x / 112.0, z / 112.0, seed)
        + 0.30 * value_noise(x / 37.0, z / 37.0, seed ^ 0x5351)
        + 0.15 * value_noise(x / 11.0, z / 11.0, seed ^ 0xA9E2)
}

// ------------------------------------------------------------- terrain -----

pub const SEA: i32 = 63;
pub const MIN_SEC: i32 = -4; // Y=-4..19 => blocks -64..319
pub const MAX_SEC: i32 = 19;
pub const NSEC: usize = (MAX_SEC - MIN_SEC + 1) as usize;

pub const T_ORE: u64 = 0x0FE1;
pub const T_ORE_KIND: u64 = 0x0FE2;
pub const T_TUFF: u64 = 0x7AB2;

pub struct World {
    pub seed: u64,
}

impl World {
    pub fn new(seed: u64) -> Self {
        World { seed }
    }

    /// Terrain surface height per column.
    pub fn height(&self, cx: i32, cz: i32, lx: usize, lz: usize, round: u64) -> i32 {
        let wx = (cx * 16 + lx as i32) as f64;
        let wz = (cz * 16 + lz as i32) as f64;
        let s = self.seed ^ (round as u64).wrapping_mul(0x1234_5678_9AB_CDEF);
        let cont = fbm(wx / 3.0, wz / 3.0, s ^ 0xC0A7); // continentalness ~512b scale
        let rough = fbm(wx, wz, s);
        let h = if cont < 0.36 {
            28.0 + rough * 10.0 // deep ocean
        } else if cont < 0.42 {
            58.0 + rough * 8.0 // coast band
        } else {
            62.0 + rough * 46.0 + 8.0 * fbm(wx * 2.7, wz * 2.7, s ^ 0x77E5)
        };
        h.round() as i32
    }

    /// Coarse 4x4x4 cell hash (ore veins / patches cluster like real gen).
    pub fn cell(&self, cx: i32, cz: i32, lx: usize, y: i32, lz: usize, tag: u64) -> u64 {
        splitmix(
            ((cx * 16 + lx as i32) as u64 / 4)
                ^ ((y as u64 / 4).wrapping_mul(0x9E3779B1))
                ^ (((cz * 16 + lz as i32) as u64 / 4).wrapping_mul(0x85EBCA6B))
                ^ tag,
        ) & 0x3F
    }

    /// Palette id per position.
    /// 0 air, 1 stone, 2 deepslate, 3 dirt, 4 grass_block, 5 water, 6 sand,
    /// 7 gravel, 8 coal_ore, 9 iron_ore, 10 copper_ore, 11 tuff, 12 bedrock,
    /// 13 clay
    pub fn block_at(&self, cx: i32, cz: i32, h: i32, y: i32, lx: usize, lz: usize) -> u8 {
        if y <= -60 {
            return 12; // bedrock floor
        }
        if y < 0 {
            if self.cell(0, 0, lx, y, lz, T_ORE) < 3 {
                return 8 + (self.cell(0, 0, lx, y, lz, T_ORE_KIND) % 3) as u8;
            }
            if self.cell(0, 0, lx, y, lz, T_TUFF) > 26 {
                return 11;
            }
            return 2;
        }
        if y < h - 4 {
            if self.cell(0, 0, lx, y, lz, T_ORE) < 3 {
                return 8 + (self.cell(0, 0, lx, y, lz, T_ORE_KIND) % 3) as u8;
            }
            return 1; // stone
        }
        if y < h {
            return if h <= SEA + 2 { 6 } else { 3 }; // beach sand vs dirt
        }
        if y == h {
            if h < SEA {
                return 13; // ocean floor (caller expands gravel/clay)
            }
            if h <= SEA + 1 {
                return 6; // sand beach
            }
            return 4; // grass
        }
        if y <= SEA {
            return 5; // water column
        }
        0 // air
    }
}

// ------------------------------------------------------------- packing -----

pub fn bits_for(n: usize) -> u32 {
    let mut b = 1u32;
    while (1usize << b) < n {
        b += 1;
    }
    b
}

/// 1.16+ no-straddle packing: entries never cross long boundaries.
pub fn pack(entries: &[u8], bits: u32) -> Vec<i64> {
    let per = (64 / bits) as usize;
    entries
        .chunks(per)
        .map(|c| {
            let mut v: u64 = 0;
            for (i, e) in c.iter().enumerate() {
                v |= (*e as u64) << (i * bits as usize);
            }
            v as i64
        })
        .collect()
}

// ------------------------------------------------------------- payload -----

pub const K_AIR: u8 = 0;
pub const K_WATER: u8 = 1;
pub const K_PACKED: u8 = 2;

#[allow(clippy::too_many_arguments)]
pub fn build_section(
    w: &World,
    cx: i32,
    cz: i32,
    sy: i32,
    heights: &[i32; 256],
    round: u64,
) -> (Map<String, Value>, u8) {
    let base = sy * 16;
    let y_top = base + 15;

    let max_h = *heights.iter().max().unwrap();
    let min_h = *heights.iter().min().unwrap();
    let kind = if min_h > y_top && base > SEA {
        K_AIR
    } else if max_h < base && base >= 0 && y_top <= SEA {
        K_WATER
    } else {
        K_PACKED
    };

    let mut sec: Map<String, Value> = Map::new();
    sec.insert("Y".to_string(), Value::Byte(sy as i8));

    let biome = if max_h < SEA + 2 {
        "minecraft:ocean"
    } else if max_h > 96 {
        "minecraft:windswept_hills"
    } else if base < SEA {
        "minecraft:stony_shore"
    } else {
        "minecraft:plains"
    };
    let mut biomes: Map<String, Value> = Map::new();
    biomes.insert("palette".to_string(), Value::List(vec![Value::String(biome.into())]));
    sec.insert("biomes".to_string(), Value::Compound(biomes));

    match kind {
        K_AIR => {
            let mut bs: Map<String, Value> = Map::new();
            bs.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:air".into())]));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec.insert(
                "SkyLight".to_string(),
                Value::ByteArray(fastnbt::ByteArray::new(vec![-1i8; 2048])),
            );
        }
        K_WATER => {
            let mut bs: Map<String, Value> = Map::new();
            bs.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:water".into())]));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec.insert(
                "SkyLight".to_string(),
                Value::ByteArray(fastnbt::ByteArray::new(vec![0i8; 2048])),
            );
        }
        _ => {
            const NAMES: [&str; 14] = [
                "minecraft:air",
                "minecraft:stone",
                "minecraft:deepslate",
                "minecraft:dirt",
                "minecraft:grass_block",
                "minecraft:water",
                "minecraft:sand",
                "minecraft:gravel",
                "minecraft:coal_ore",
                "minecraft:iron_ore",
                "minecraft:copper_ore",
                "minecraft:tuff",
                "minecraft:bedrock",
                "minecraft:clay",
            ];
            let mut slot_of: Map<&str, u8> = Map::new();
            let mut entries = Vec::with_capacity(4096);
            for lz in 0..16 {
                for lx in 0..16 {
                    let h = heights[lz * 16 + lx];
                    for dy in 0..16 {
                        let y = base + dy;
                        let mut id = w.block_at(cx, cz, h, y, lx, lz);
                        if id == 13 {
                            id = if splitmix(
                                ((cx * 16 + lx as i32) as u64)
                                    ^ (((cz * 16 + lz as i32) as u64).wrapping_mul(3))
                                    ^ (y as u64 / 2),
                            ) & 7
                                < 2
                            {
                                13
                            } else {
                                7
                            };
                        }
                        let name = NAMES[id as usize];
                        let next = slot_of.len() as u8;
                        let slot = *slot_of.entry(name).or_insert(next);
                        entries.push(slot);
                    }
                }
            }
            let mut by_slot = vec![""; slot_of.len()];
            for (k, s) in &slot_of {
                by_slot[*s as usize] = k;
            }
            let palette: Vec<Value> = by_slot.iter().map(|k| Value::String(k.to_string())).collect();
            let bits = bits_for(palette.len().max(1));
            let longs = pack(&entries, bits);
            let mut bs: Map<String, Value> = Map::new();
            bs.insert("palette".to_string(), Value::List(palette));
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(longs)));
            sec.insert("block_states".to_string(), Value::Compound(bs));

            // skylight nibble array: attenuated through water/solid
            if base >= SEA - 16 && base <= SEA + 48 {
                let mut nib = vec![0u8; 2048];
                for lz in 0..16 {
                    for lx in 0..16 {
                        let h = heights[lz * 16 + lx];
                        let floor = h.max(SEA);
                        for dy in 0..16 {
                            let y = base + dy;
                            let v = if y > floor {
                                15u8
                            } else if y <= h && y > floor - 16 {
                                0
                            } else {
                                0
                            };
                            let grad = if y > floor {
                                15u8
                            } else if y > h {
                                (15 - (floor - y)).max(0) as u8
                            } else {
                                0
                            };
                            let idx: usize = (dy as usize) * 256 + lz * 16 + lx;
                            if idx % 2 == 0 {
                                nib[idx / 2] |= grad & 0x0f;
                            } else {
                                nib[idx / 2] |= (grad & 0x0f) << 4;
                            }
                            let _ = v;
                        }
                    }
                }
                sec.insert(
                    "SkyLight".to_string(),
                    Value::ByteArray(fastnbt::ByteArray::new(nib.iter().map(|b| *b as i8).collect())),
                );
            }
        }
    }
    (sec, kind)
}

/// Pack 9-bit heightmap entries (7 per long, no-straddle) -> 37 longs.
pub fn pack_hmap(entries: &[u32]) -> Vec<i64> {
    let bits = 9u32;
    let per = (64 / bits) as usize;
    entries
        .chunks(per)
        .map(|c| {
            let mut v: u64 = 0;
            for (i, e) in c.iter().enumerate() {
                v |= (*e as u64) << (i * bits as usize);
            }
            v as i64
        })
        .collect()
}

pub fn chunk_payload(w: &World, cx: i32, cz: i32, round: u64) -> Vec<u8> {
    let mut heights = [0i32; 256];
    for lz in 0..16 {
        for lx in 0..16 {
            heights[lz * 16 + lx] = w.height(cx, cz, lx, lz, round);
        }
    }

    let mut sections = Vec::with_capacity(NSEC);
    for i in 0..NSEC {
        let sy = MIN_SEC + i as i32;
        let (sec, _k) = build_section(w, cx, cz, sy, &heights, round);
        sections.push(Value::Compound(sec));
    }

    // heightmaps: entry = highest blocking y + 1
    let mut hm_mb = [0u32; 256];
    let mut hm_ws = [0u32; 256];
    for i in 0..256 {
        let h = heights[i];
        let top = h.max(SEA);
        hm_mb[i] = (top + 1).max(1) as u32;
        hm_ws[i] = (h + 1).max(1) as u32;
    }
    let mut hms: Map<String, Value> = Map::new();
    hms.insert(
        "MOTION_BLOCKING".to_string(),
        Value::LongArray(fastnbt::LongArray::new(pack_hmap(&hm_mb))),
    );
    hms.insert(
        "WORLD_SURFACE".to_string(),
        Value::LongArray(fastnbt::LongArray::new(pack_hmap(&hm_ws))),
    );
    hms.insert(
        "OCEAN_FLOOR".to_string(),
        Value::LongArray(fastnbt::LongArray::new(pack_hmap(&hm_ws))),
    );
    hms.insert(
        "MOTION_BLOCKING_NO_LEAVES".to_string(),
        Value::LongArray(fastnbt::LongArray::new(pack_hmap(&hm_mb))),
    );

    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert("yPos".to_string(), Value::Int(MIN_SEC));
    root.insert("Status".to_string(), Value::String("minecraft:full".into()));
    root.insert("lastUpdate".to_string(), Value::Long(0));
    root.insert(
        "InhabitedTime".to_string(),
        Value::Long(((cx as i64 * 31 + cz as i64 * 17) % 937) as i64),
    );
    root.insert("sections".to_string(), Value::List(sections));
    root.insert("Heightmaps".to_string(), Value::Compound(hms));
    root.insert(
        "PostProcessing".to_string(),
        Value::List((0..NSEC).map(|_| Value::List(Vec::new())).collect()),
    );

    // block entities: player edits add chests; land chunks sprinkle some
    let mut be: Vec<Value> = Vec::new();
    if round > 0 && (cx ^ cz) % 3 == 0 {
        let mut chest: Map<String, Value> = Map::new();
        chest.insert("id".to_string(), Value::String("minecraft:chest".into()));
        chest.insert("x".to_string(), Value::Int(cx * 16 + 8));
        chest.insert("y".to_string(), Value::Int(heights[136] + 1));
        chest.insert("z".to_string(), Value::Int(cz * 16 + 8));
        chest.insert(
            "LootTable".to_string(),
            Value::String(format!("minecraft:chests/simple_treasure_r{round}")),
        );
        be.push(Value::Compound(chest));
    } else if (cx.wrapping_mul(7) ^ cz.wrapping_mul(13)) % 23 == 0 {
        let mut chest: Map<String, Value> = Map::new();
        chest.insert("id".to_string(), Value::String("minecraft:chest".into()));
        chest.insert("x".to_string(), Value::Int(cx * 16 + 3));
        chest.insert("y".to_string(), Value::Int(heights[52] + 1));
        chest.insert("z".to_string(), Value::Int(cz * 16 + 3));
        be.push(Value::Compound(chest));
    }
    root.insert("block_entities".to_string(), Value::List(be));

    if (cx ^ cz) % 5 == 0 {
        let mut bl: Map<String, Value> = Map::new();
        bl.insert("min_section_height".to_string(), Value::Int(MIN_SEC));
        bl.insert("sections_count".to_string(), Value::Int(NSEC as i32));
        bl.insert("old_noise".to_string(), Value::Byte(0));
        bl.insert(
            "section_pos_heights".to_string(),
            Value::IntArray(fastnbt::IntArray::new(
                (0..NSEC)
                    .map(|i| (MIN_SEC + i as i32) * 16 + heights[(i * 5) % 256] / 2)
                    .collect(),
            )),
        );
        root.insert("blending_data".to_string(), Value::Compound(bl));
    }

    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

/// Entity payload (whole-mode store).
pub fn entity_payload(cx: i32, cz: i32, round: u64) -> Vec<u8> {
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
    let mut ents = Vec::new();
    let n = (splitmix((cx as u64) ^ ((cz as u64) << 32) ^ round) % 100) as i32;
    if n < 4 {
        for i in 0..n {
            let mut e: Map<String, Value> = Map::new();
            e.insert(
                "id".to_string(),
                Value::String(if i % 2 == 0 { "minecraft:sheep".into() } else { "minecraft:zombie".into() }),
            );
            e.insert(
                "Pos".to_string(),
                Value::List(vec![
                    Value::Double(cx as f64 * 16.0 + i as f64),
                    Value::Double(64.0),
                    Value::Double(cz as f64 * 16.0 + i as f64),
                ]),
            );
            e.insert(
                "UUID".to_string(),
                Value::IntArray(fastnbt::IntArray::new(vec![
                    (cx * 31 + i) as i32,
                    (cz * 17 + i) as i32,
                    (0xdead_be7u32 ^ (i as u32)) as i32,
                    42 + i,
                ])),
            );
            e.insert("Health".to_string(), Value::Float(4.0));
            e.insert("Motion".to_string(), Value::List(vec![Value::Double(0.0); 3]));
            e.insert("OnGround".to_string(), Value::Byte(1));
            ents.push(Value::Compound(e));
        }
    }
    root.insert("Entities".to_string(), Value::List(ents));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}


pub fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

pub fn semantic_eq(a: &[u8], b: &[u8]) -> bool {
    let va: Value = fastnbt::from_bytes(a).unwrap();
    let vb: Value = fastnbt::from_bytes(b).unwrap();
    va == vb
}
