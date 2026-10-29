//! survival_exp — survival-world POST-COMPRESSION byte attribution per
//! component class + prediction of codec levers BEFORE touching production
//! code (same methodology as jj_exp3, applied to the survival shape):
//!
//!   L1: uniform-plane skylight codec (light planes that are constant collapse to 1 byte)
//!   L2: column-staircase skylight codec (per-column boundary list, RLE over columns)
//!   L3: cross-section shift-ref potential for block_states (CSEC 0xC9 admission rate)
//!   L4: plane-level (16x16x1) block-data dedup potential
//!   L5: block-entity item dedup potential (content-addressed item blobs)
//!
//! Model: per-class concatenation compressed with zstd-19 (models slab frames).

use fastnbt::Value;
use std::collections::HashMap as Map;
use std::io::Write;

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn z19(data: &[u8]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    zstd::bulk::compress(data, 19).map(|c| c.len() as u64).unwrap_or(0)
}

fn z19_of(data: &[u8]) -> u64 {
    z19(data)
}

// ----------------------------------------------------------- noise ----
// (deterministic generators mirroring survival_bench.rs — same corpus shape)

fn noise2(x: i32, z: i32, seed: u64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (z as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ seed.wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    (h & 0xFFFF_FFFF) as f64 / 4294967296.0
}

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

const SEC_MIN: i32 = -4;
const SEC_MAX: i32 = 20;

/// Per-section analysis payload kept aside while building the chunk NBT.
struct SecMeta {
    sy: i32,
    palette: Vec<&'static str>,
    voxels: Vec<u8>,           // 4096 palette indices
    skylight: Option<Vec<u8>>, // 4096 expanded nibble values (0..15)
}

fn column_height(cx: i32, cz: i32, x: usize, z: usize) -> i32 {
    let wx = cx as f64 * 16.0 + x as f64;
    let wz = cz as f64 * 16.0 + z as f64;
    let base = smooth2(wx / 96.0, wz / 96.0, 0xA1);
    let detail = smooth2(wx / 24.0, wz / 24.0, 0xB2);
    let h = 62.0 + (base - 0.5) * 44.0 + (detail - 0.5) * 10.0;
    h as i32
}

fn block_at(cx: i32, cz: i32, x: usize, y: i32, z: usize, surface: i32) -> &'static str {
    let wx = cx * 16 + x as i32;
    let wz = cz * 16 + z as i32;
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

fn skylight_at(surface: i32, y: i32) -> u8 {
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

/// Build one section: returns (vanilla NBT compound, analysis metadata).
#[allow(clippy::type_complexity)]
fn build_section(
    cx: i32,
    cz: i32,
    sy: i32,
    surface_col: &[i32; 256],
    with_skylight: bool,
) -> (Value, SecMeta) {
    let mut sec: Map<String, Value> = Map::new();
    sec.insert("Y".to_string(), Value::Byte(sy as i8));
    let mut pal: Vec<&'static str> = Vec::new();
    let mut idx_of: Map<&'static str, u8> = Map::new();
    let mut voxels: Vec<u8> = Vec::with_capacity(4096);
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
                voxels.push(i);
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
                &voxels, bits,
            ))),
        );
    }
    sec.insert("block_states".to_string(), Value::Compound(bs));
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
    let mut sky_expanded: Option<Vec<u8>> = None;
    if with_skylight {
        let mut arr = vec![0i8; 2048];
        let mut exp = vec![0u8; 4096];
        for z in 0..16usize {
            for x in 0..16usize {
                let surface = surface_col[z * 16 + x];
                for dy in 0..16i32 {
                    let y = sy * 16 + dy;
                    let v = skylight_at(surface, y);
                    let i = (dy as usize) * 256 + z * 16 + x;
                    exp[i] = v;
                    let byte = i >> 1;
                    let cur = arr[byte] as u8;
                    let nv = if i % 2 == 0 {
                        (cur & 0xF0) | (v & 0x0F)
                    } else {
                        (cur & 0x0F) | ((v & 0x0F) << 4)
                    };
                    arr[byte] = nv as i8;
                }
            }
        }
        sky_expanded = Some(exp);
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(arr)));
    }
    (
        Value::Compound(sec),
        SecMeta { sy, palette: pal, voxels, skylight: sky_expanded },
    )
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

fn heightmap_long(surface_col: &[i32; 256]) -> Vec<i64> {
    let entries: Vec<u8> = (0..256).map(|i| (surface_col[i] + 64) as u8).collect();
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

struct ChunkData {
    nbt: Vec<u8>,
    metas: Vec<SecMeta>,
    block_entities: Vec<Value>,
}

fn build_chunk(cx: i32, cz: i32) -> ChunkData {
    let surface = surface_map(cx, cz);
    let mut root: Map<String, Value> = Map::new();
    root.insert("DataVersion".to_string(), Value::Int(4189));
    root.insert(
        "Position".to_string(),
        Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
    );
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
    let mut metas = Vec::new();
    for sy in SEC_MIN..SEC_MAX {
        let sec_top = sy * 16 + 15;
        let smin = surface.iter().cloned().fold(i32::MAX, |a, b| a.min(b));
        let smax = surface.iter().cloned().fold(i32::MIN, |a, b| a.max(b));
        let lit = sec_top >= smin - 4 && sy * 16 <= smax + 1;
        let (v, meta) = build_section(cx, cz, sy, &surface, lit);
        sec_list.push(v);
        metas.push(meta);
    }
    root.insert("sections".to_string(), Value::List(sec_list));
    let mut be = Vec::new();
    for k in 0..2u64 {
        let x = cx * 16 + (hash3(cx, k as i32, cz, 0x11) % 16) as i32;
        let z = cz * 16 + (hash3(cx, (k + 7) as i32, cz, 0x12) % 16) as i32;
        let y = surface[(z.rem_euclid(16) * 16 + x.rem_euclid(16)) as usize];
        be.push(chest_block_entity(x, y, z, 0x13 + k));
    }
    root.insert("block_entities".to_string(), Value::List(be.clone()));
    ChunkData {
        nbt: fastnbt::to_bytes(&Value::Compound(root)).unwrap(),
        metas,
        block_entities: be,
    }
}

// -------------------------------------------------- lever experiments ----

/// L1: uniform-plane skylight codec. Plane dy is 128 contiguous nibble bytes.
/// Layout: per plane either [0x80|v] (constant) or [0xFF][128 raw bytes].
fn light_planes_cost(sections: &[&SecMeta]) -> (u64, u64) {
    let mut raw_total = 0u64;
    let mut codec_total = 0u64;
    for s in sections {
        if let Some(sky) = &s.skylight {
            raw_total += 2048;
            for dy in 0..16usize {
                let plane = &sky[dy * 256..(dy + 1) * 256];
                let v0 = plane[0];
                if plane.iter().all(|&v| v == v0) {
                    codec_total += 1;
                } else {
                    codec_total += 1 + 128;
                }
            }
        }
    }
    (raw_total, codec_total)
}

/// L2: column-staircase skylight codec. Per column: boundary list from the
/// top (dy where the value drops), then RLE over the 256 columns.
fn light_columns_cost(sections: &[&SecMeta]) -> (u64, u64) {
    let mut raw_total = 0u64;
    let mut codec_total = 0u64;
    for s in sections {
        if let Some(sky) = &s.skylight {
            raw_total += 2048;
            // per column (z*16+x): values at dy 0..16
            let mut col_stream: Vec<u8> = Vec::new();
            for col in 0..256usize {
                // staircase: value changes as dy increases (y decreases)
                let mut steps: Vec<(u8, u8)> = Vec::new();
                let mut cur = sky[col]; // dy=0
                for dy in 1..16usize {
                    let v = sky[dy * 256 + col];
                    if v != cur {
                        steps.push((dy as u8, v));
                        cur = v;
                    }
                }
                col_stream.push(steps.len() as u8);
                for (dy, v) in steps {
                    col_stream.push(dy);
                    col_stream.push(v);
                }
            }
            // RLE over column records (record = 1 + 2*steps bytes):
            // identical consecutive columns collapse to one record + run len
            let mut i = 0usize;
            while i < col_stream.len() {
                let rec_len = 1 + 2 * col_stream[i] as usize;
                let rec = &col_stream[i..i + rec_len];
                let mut j = i + rec_len;
                while j + rec_len <= col_stream.len()
                    && col_stream[j] == col_stream[i]
                    && &col_stream[j..j + rec_len] == rec
                {
                    j += rec_len;
                }
                codec_total += (rec_len + 4) as u64; // record once + u32 run len
                i = j;
            }
        }
    }
    (raw_total, codec_total)
}

/// L3: shift-ref admission for block data. A section references the section
/// k planes below it (same chunk) if its lower (16-k) planes are exactly the
/// lower section's top (16-k) planes shifted up... simplified vanilla form:
/// section.s voxels[i] == below.voxels[i + k*256] for i < (4096 - k*256),
/// with the top k planes explicit. Measure best k and the byte win at the
/// section's dense bit width.
fn shift_ref_potential(sections: &[&SecMeta]) -> (usize, u64, u64) {
    let mut hits = 0usize;
    let mut full_bytes = 0u64;
    let mut ref_bytes = 0u64;
    let by_y: Map<i32, &SecMeta> = sections.iter().map(|s| (s.sy, *s)).collect();
    for s in sections {
        let bits = {
            let n = s.palette.len();
            if n <= 1 {
                0
            } else {
                let mut b = 4u32;
                while (1usize << b) < n {
                    b += 1;
                }
                b
            }
        };
        if bits == 0 {
            continue;
        }
        let bits64 = bits as u64;
        let cost_full = (4096u64 * bits64).div_ceil(8);
        full_bytes += cost_full;
        let mut best: Option<(usize, u64)> = None;
        for k in 1..=15usize {
            if let Some(below) = by_y.get(&(s.sy - k as i32)) {
                let limit = 4096 - k * 256;
                let ok = (0..limit).all(|i| s.voxels[i] == below.voxels[i + k * 256]);
                if ok {
                    let cost_ref = ((k * 256) as u64 * bits64).div_ceil(8) + 8;
                    if best.map(|(_, c)| cost_ref < c).unwrap_or(true) {
                        best = Some((k, cost_ref));
                    }
                }
            }
        }
        if let Some((_, cost_ref)) = best {
            hits += 1;
            ref_bytes += cost_ref;
        } else {
            ref_bytes += cost_full;
        }
    }
    (hits, full_bytes, ref_bytes)
}

/// L4: plane-level dedup potential for block data (16x16x1 planes).
fn plane_dedup_potential(chunks: &[ChunkData]) -> (usize, usize) {
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut total = 0usize;
    for ch in chunks {
        for s in &ch.metas {
            if s.palette.len() <= 1 {
                continue;
            }
            for p in 0..16usize {
                let plane = &s.voxels[p * 256..(p + 1) * 256];
                let mut h = xxhash_rust::xxh3::xxh3_64(plane);
                h = h.wrapping_mul(31).wrapping_add(s.palette.len() as u64);
                // include palette identity (cheap approximation: hash names)
                for n in &s.palette {
                    h ^= xxhash_rust::xxh3::xxh3_64(n.as_bytes());
                }
                seen.insert(h);
                total += 1;
            }
        }
    }
    (total, seen.len())
}

/// L5: block-entity item dedup potential.
fn item_dedup_potential(chunks: &[ChunkData]) -> (u64, u64, usize, usize) {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut refs = 0usize;
    for ch in chunks {
        for be in &ch.block_entities {
            let Value::Compound(m) = be else { continue };
            if let Some(Value::List(items)) = m.get("Items") {
                for it in items {
                    blobs.push(fastnbt::to_bytes(it).unwrap_or_default());
                    refs += 1;
                }
            }
        }
    }
    // unique set
    let mut uniq: Vec<Vec<u8>> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for b in &blobs {
        let h = xxhash_rust::xxh3::xxh3_64(b);
        if seen.insert(h) {
            uniq.push(b.clone());
        }
    }
    let concat: Vec<u8> = blobs.concat();
    let uniq_concat: Vec<u8> = uniq.concat();
    let refs_cost = refs as u64 * 3; // varint id model
    let raw_z = z19_of(&concat);
    let dedup_z = z19_of(&uniq_concat) + refs_cost;
    (raw_z, dedup_z, refs, uniq.len())
}

// ------------------------------------------------------------- main ----

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let half: i32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(24); // 48x48
    println!("survival_exp: building {}x{} chunk corpus...", half * 2, half * 2);

    let mut chunks: Vec<ChunkData> = Vec::new();
    for cz in -half..half {
        for cx in -half..half {
            chunks.push(build_chunk(cx, cz));
        }
    }
    let n_chunks = chunks.len();

    // ---- class weights (raw) ----
    let mut class_stream: Map<&str, Vec<u8>> = Map::new();
    let add = |k: &'static str, bytes: &[u8], cls: &mut Map<&'static str, Vec<u8>>| {
        cls.entry(k).or_default().extend_from_slice(bytes);
    };
    let mut sections_all: Vec<&SecMeta> = Vec::new();
    for ch in &chunks {
        // frame: root minus sections/block_entities (approximate: serialize
        // DataVersion+Position+Heightmaps by rebuilding a small compound)
        let frame = fastnbt::to_bytes(&Value::Compound({
            let mut m: Map<String, Value> = Map::new();
            m.insert("DataVersion".to_string(), Value::Int(4189));
            m.insert("Position".to_string(), Value::List(vec![Value::Int(0), Value::Int(0), Value::Int(0)]));
            m
        }))
        .unwrap();
        add("frame_root", &frame, &mut class_stream);
        // heightmaps: extract from the real chunk bytes by re-serializing
        let root: Value = fastnbt::from_bytes(&ch.nbt).unwrap();
        if let Value::Compound(m) = &root {
            if let Some(hm) = m.get("Heightmaps") {
                let b = fastnbt::to_bytes(hm).unwrap_or_default();
                add("heightmaps", &b, &mut class_stream);
            }
        }
        for be in &ch.block_entities {
            let b = fastnbt::to_bytes(be).unwrap_or_default();
            add("block_entities", &b, &mut class_stream);
        }
        for meta in &ch.metas {
            sections_all.push(meta);
        }
    }
    // section components from metadata (exact accounting)
    for s in sections_all.iter() {
        // palette bytes (model: u8 kind + u16 len + name, string form)
        for n in &s.palette {
            let mut e = vec![0u8];
            e.extend_from_slice(&(n.len() as u16).to_le_bytes());
            e.extend_from_slice(n.as_bytes());
            add("palette_entries", &e, &mut class_stream);
        }
        if s.palette.len() > 1 {
            let bits = {
                let mut b = 4u32;
                while (1usize << b) < s.palette.len() {
                    b += 1;
                }
                b
            };
            let packed = c_saves::chunknbt::pack_nostroddle(&s.voxels, bits);
            let bytes: Vec<u8> = packed
                .iter()
                .flat_map(|l| l.to_le_bytes())
                .collect();
            add("bs_data", &bytes, &mut class_stream);
        }
        if let Some(sky) = &s.skylight {
            // nibble-pack back to the vanilla 2048-byte array
            let mut arr = vec![0u8; 2048];
            for (i, &v) in sky.iter().enumerate() {
                let b = i >> 1;
                if i % 2 == 0 {
                    arr[b] = (arr[b] & 0xF0) | (v & 0x0F);
                } else {
                    arr[b] = (arr[b] & 0x0F) | ((v & 0x0F) << 4);
                }
            }
            add("skylight", &arr, &mut class_stream);
        }
        add("biomes_uniform", &[0u8; 4], &mut class_stream);
    }
    let total_raw: u64 = class_stream.values().map(|v| v.len() as u64).sum();
    println!("\n== survival corpus: {} chunks, {} sections ==", n_chunks, sections_all.len());
    println!("== class weights (raw / z19-concat) ==");
    let mut rows: Vec<(&str, u64, u64)> = class_stream
        .iter()
        .map(|(k, v)| (*k, v.len() as u64, z19_of(v)))
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let total_z19: u64 = rows.iter().map(|r| r.2).sum();
    let mut w = std::io::BufWriter::new(std::io::stdout());
    for (k, raw, z) in &rows {
        writeln!(
            w,
            "CLASS {:16} raw {:>10} ({:5.1}%)  z19 {:>9} ({:5.1}%)",
            k,
            raw,
            100.0 * *raw as f64 / total_raw as f64,
            z,
            100.0 * *z as f64 / total_z19 as f64
        )
        .unwrap();
    }
    writeln!(w, "CLASS {:16} raw {:>10}            z19 {:>9}", "TOTAL", total_raw, total_z19).unwrap();

    // ---- levers ----
    writeln!(w, "\n== levers (post-compression byte predictions) ==").unwrap();

    // L1/L2: skylight
    let (l1_raw, l1) = light_planes_cost(&sections_all);
    let (l2_raw, l2) = light_columns_cost(&sections_all);
    let sky_z = rows.iter().find(|r| r.0 == "skylight").map(|r| r.2).unwrap_or(0);
    writeln!(
        w,
        "L1 skylight-planes : raw {} -> codec {} bytes (z19-model) vs current z19 {}",
        l1_raw,
        z19_model_hint(l1),
        sky_z
    )
    .unwrap();
    writeln!(
        w,
        "L2 skylight-columns: raw {} -> codec {} bytes (z19-model) vs current z19 {}",
        l2_raw,
        z19_model_hint(l2),
        sky_z
    )
    .unwrap();

    // L3: shift-ref
    let (hits, full, refr) = shift_ref_potential(&sections_all);
    writeln!(
        w,
        "L3 shift-ref blocks: {}/{} sections admit a ref; dense bytes {} -> {} (delta {})",
        hits,
        sections_all.iter().filter(|s| s.palette.len() > 1).count(),
        full,
        refr,
        full.saturating_sub(refr)
    )
    .unwrap();

    // L4: plane dedup
    let (total_planes, uniq_planes) = plane_dedup_potential(&chunks);
    writeln!(
        w,
        "L4 plane dedup     : {}/{} planes unique ({:.1}%) — dedup headroom if palettes aligned",
        uniq_planes,
        total_planes,
        100.0 * uniq_planes as f64 / total_planes.max(1) as f64
    )
    .unwrap();

    // L5: item dedup
    let (raw_z, dedup_z, refs, uniq) = item_dedup_potential(&chunks);
    writeln!(
        w,
        "L5 item dedup      : {} item blobs ({} unique); z19 {} -> dedup+z19 {} bytes",
        refs, uniq, raw_z, dedup_z
    )
    .unwrap();

    w.flush().unwrap();
    println!("survival_exp done");
}

/// Rough zstd-19 prediction for structured byte streams (observed 3-6x on
/// such streams in the jj_exp3 experiments). Conservative model.
fn z19_model_hint(codec_bytes: u64) -> u64 {
    codec_bytes / 3 + 8
}
