//! profile_size: WHAT weighs the most in the container, byte-exact.
//!
//! 1. writes the bench world through real Stores (dict training + compaction),
//! 2. dumps exact container accounting (framing / headers / comp / slabs /
//!    recipes / dicts),
//! 3. classifies every payload component (sections by kind, tail, entities)
//!    and measures raw + compressed weight of each,
//! 4. runs offline encoding experiments (levels, dicts, custom section codec,
//!    chunk-grouping, heightmap repack) to rank optimization candidates.
//!
//! Usage: cargo run --release --example profile_size [grid] [rounds]

#[path = "wgen/mod.rs"]
mod wgen;

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap as Map;
use std::io::Write;
use std::time::Instant;
use wgen::*;

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn tb(v: &Value) -> Vec<u8> {
    fastnbt::to_bytes(v).unwrap()
}

// --------------------------------------------------------- custom codec ----

const CSEC_MAGIC: u8 = 0xC5; // custom section marker

fn bits_for(n: usize) -> u32 {
    let mut b = 1u32;
    while (1usize << b) < n {
        b += 1;
    }
    b
}

/// Dense bit writer (no long alignment).
struct BitW {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitW {
    fn new() -> Self {
        BitW { out: Vec::new(), acc: 0, n: 0 }
    }
    fn put(&mut self, v: u64, bits: u32) {
        self.acc |= v << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc & 0xff) as u8);
        }
        self.out
    }
}

struct BitR<'a> {
    in_: &'a [u8],
    pos: usize, // bit
}

impl<'a> BitR<'a> {
    fn get(&mut self, bits: u32) -> u64 {
        let mut v = 0u64;
        for i in 0..bits {
            let byte = self.in_[self.pos >> 3];
            let bit = (byte >> (self.pos & 7)) & 1;
            v |= (bit as u64) << i;
            self.pos += 1;
        }
        let _ = i_unused();
        v
    }
}

fn i_unused() -> usize {
    0
}

/// Encode `entries` (values < 2^bits) densely.
fn dense_pack(entries: &[u8], bits: u32) -> Vec<u8> {
    let mut w = BitW::new();
    for e in entries {
        w.put(*e as u64, bits);
    }
    w.finish()
}

/// Unpack `n` entries of `bits` bits from a dense stream.
fn dense_unpack(b: &[u8], n: usize, bits: u32) -> Vec<u8> {
    let mut r = BitR { in_: b, pos: 0 };
    (0..n).map(|_| r.get(bits) as u8).collect()
}

/// 1.16+ no-straddle packing (vanilla layout) — used to restore exact data.
fn pack_nostroddle(entries: &[u8], bits: u32) -> Vec<i64> {
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

/// Vanilla no-straddle unpack: `n` entries of `bits` bits.
fn unpack_nostroddle(longs: &[i64], n: usize, bits: u32) -> Vec<u8> {
    let per = (64 / bits) as usize;
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let mut out = Vec::with_capacity(n);
    for l in longs {
        let lu = *l as u64; // bit pattern, sign bit is data
        for i in 0..per {
            if out.len() == n {
                return out;
            }
            out.push(((lu >> (i * bits as usize)) & mask) as u8);
        }
    }
    out
}

/// Custom section encoding. Returns None when the section has keys outside
/// the understood set (caller falls back to plain NBT bytes).
/// Layout:
///   [0xC5][flags u8][Y i8]
///   flags bit0: block_states present
///         bit1: block_states has data (palette > 1)
///         bit2: biomes present
///         bit3: biomes has data
///         bit4: SkyLight present, uniform-all-1
///         bit5: SkyLight present, uniform-all-0
///         bit6: SkyLight present, non-uniform
///         bit7: other keys present (raw NBT fragment appended)
///   block_states: [u16 pal_n] strings; data dense-packed at bits_for(pal_n)
///   biomes:       same, 64 entries
///   skylight:     0 or 2048 bytes
fn section_encode(sec: &Value) -> Option<Vec<u8>> {
    let Value::Compound(m) = sec else { return None };
    const KNOWN: [&str; 5] = ["Y", "block_states", "biomes", "SkyLight", "BlockLight"];
    let mut other: Vec<&String> = m.keys().filter(|k| !KNOWN.contains(&k.as_str())).collect();
    // unknown keys => bail (keep it lossless-simple for the experiment)
    if !other.is_empty() {
        return None;
    }
    other.clear();
    let y = match m.get("Y") {
        Some(Value::Byte(b)) => *b,
        _ => return None,
    };
    let mut out = vec![CSEC_MAGIC, 0u8, y as u8];
    let mut flags = 0u8;

    let enc_pal_data = |out: &mut Vec<u8>, bs: &Value, entries: usize| -> Option<bool> {
        let Value::Compound(bm) = bs else { return None };
        let Value::List(pal) = bm.get("palette")? else { return None };
        let n = pal.len();
        if n > 65535 {
            return None;
        }
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for p in pal {
            let Value::String(s) = p else { return None };
            out.extend_from_slice(&(s.len() as u16).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let data = match bm.get("data") {
            Some(Value::LongArray(la)) => la.as_ref(),
            Some(_) => return None,
            None => {
                if n > 1 {
                    return None; // invalid vanilla shape -> bail
                }
                return Some(false);
            }
        };
        let bits = bits_for(n);
        let per = (64 / bits) as usize;
        if data.len() != (entries + per - 1) / per {
            return None; // unexpected bit width -> bail
        }
        let entries_v = unpack_nostroddle(data, entries, bits);
        out.extend_from_slice(&dense_pack(&entries_v, bits));
        Some(true)
    };

    if let Some(bs) = m.get("block_states") {
        let has_data = enc_pal_data(&mut out, bs, 4096)?;
        flags |= 1;
        if has_data {
            flags |= 2;
        }
    }
    if let Some(bs) = m.get("biomes") {
        let has_data = enc_pal_data(&mut out, bs, 64)?;
        flags |= 4;
        if has_data {
            flags |= 8;
        }
    }
    if let Some(Value::ByteArray(ba)) = m.get("SkyLight") {
        let b: Vec<u8> = ba.as_ref().iter().map(|x| *x as u8).collect();
        if b.iter().all(|x| *x == 0xff) {
            flags |= 16;
        } else if b.iter().all(|x| *x == 0) {
            flags |= 32;
        } else {
            flags |= 64;
            out.extend_from_slice(&b);
        }
    }
    if let Some(Value::ByteArray(ba)) = m.get("BlockLight") {
        // rare in overworld gen; non-uniform => bail to keep experiment simple
        let b: Vec<u8> = ba.as_ref().iter().map(|x| *x as u8).collect();
        if !(b.iter().all(|x| *x == 0)) {
            return None;
        }
    }
    out[1] = flags;
    Some(out)
}

fn section_decode(b: &[u8]) -> Option<Value> {
    if b.first() != Some(&CSEC_MAGIC) {
        return None;
    }
    let flags = b[1];
    let mut p = 2usize;
    let y = b[p] as i8;
    p += 1;
    let mut sec: Map<String, Value> = Map::new();
    sec.insert("Y".to_string(), Value::Byte(y));

    let dec_pal_data = |b: &[u8], p: &mut usize, entries: usize, has_data: bool| -> Option<Value> {
        let n = u16::from_le_bytes(b[*p..*p + 2].try_into().ok()?) as usize;
        *p += 2;
        let mut pal = Vec::with_capacity(n);
        for _ in 0..n {
            let l = u16::from_le_bytes(b[*p..*p + 2].try_into().ok()?) as usize;
            *p += 2;
            let s = std::str::from_utf8(b.get(*p..*p + l)?).ok()?.to_string();
            *p += l;
            pal.push(Value::String(s));
        }
        let mut bm: Map<String, Value> = Map::new();
        bm.insert("palette".to_string(), Value::List(pal));
        if has_data {
            let bits = bits_for(n);
            let bytes = (entries * bits as usize + 7) / 8;
            let dense = b.get(*p..*p + bytes)?;
            *p += bytes;
            let entries_v = dense_unpack(dense, entries, bits);
            let longs = pack_nostroddle(&entries_v, bits);
            bm.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(longs)));
        }
        Some(Value::Compound(bm))
    };

    if flags & 1 != 0 {
        let bs = dec_pal_data(b, &mut p, 4096, flags & 2 != 0)?;
        sec.insert("block_states".to_string(), bs);
    }
    if flags & 4 != 0 {
        let bs = dec_pal_data(b, &mut p, 64, flags & 8 != 0)?;
        sec.insert("biomes".to_string(), bs);
    }
    if flags & 112 != 0 {
        let v = if flags & 16 != 0 {
            vec![-1i8; 2048]
        } else if flags & 32 != 0 {
            vec![0i8; 2048]
        } else {
            let arr = b.get(p..p + 2048)?;
            p += 2048;
            arr.iter().map(|x| *x as i8).collect()
        };
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(v)));
    }
    Some(Value::Compound(sec))
}

/// Repack a heightmap LongArray (37 longs, 9-bit entries) to dense bytes.
/// Returns None when shape does not match.
fn hmap_repack(la: &[i64]) -> Option<Vec<u8>> {
    if la.len() != 37 {
        return None;
    }
    let entries = unpack_nostroddle(la, 256, 9);
    if entries.iter().any(|e| *e as u32 >= 512) {
        return None;
    }
    Some(dense_pack(&entries, 9))
}

fn hmap_restore(dense: &[u8]) -> Vec<i64> {
    let entries = dense_unpack(dense, 256, 9);
    pack_nostroddle(&entries, 9)
}

// ------------------------------------------------------------- helpers ----

struct Codec {
    dict: Option<Vec<u8>>,
    level: i32,
}

impl Codec {
    fn comp(&self, data: &[u8]) -> Vec<u8> {
        match &self.dict {
            Some(d) => {
                let mut c = zstd::bulk::Compressor::new(self.level).unwrap();
                c.set_dictionary(self.level, d).unwrap();
                c.compress(data).unwrap()
            }
            None => zstd::bulk::compress(data, self.level).unwrap(),
        }
    }
}

fn train_dict(samples: &[Vec<u8>], max: usize) -> Option<Vec<u8>> {
    let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
    zstd::dict::from_samples(&refs, max).ok()
}

// ----------------------------------------------------------------- main ----

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let grid = args.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(96);
    let rounds = args.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(4);
    let n = (grid * grid) as usize;
    let w = World::new(0xC0FFEE);
    let dir = std::env::temp_dir().join(format!(
        "csaves-prof-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let half = (grid as i32) / 2;
    let coords: Vec<(i32, i32)> =
        (-half..half).flat_map(|cx| (-half..half).map(move |cz| (cx, cz))).collect();

    println!("=== c-saves size profile: {n} chunks, {rounds} edit rounds ===");

    let cfg = Cfg { compact_min: 256 * 1024, compact_ratio: 0.10, dict_train: 400, ..Cfg::default() };
    let region_store = Store::open_cfg(&dir.join("region"), true, cfg.clone()).unwrap();
    let entity_store = Store::open_cfg(&dir.join("entities"), true, cfg.clone()).unwrap();

    for &(cx, cz) in &coords {
        region_store.write_chunk(key(cx, cz), &chunk_payload(&w, cx, cz, 0), false).unwrap();
        entity_store.write_chunk(key(cx, cz), &entity_payload(cx, cz, 0), true).unwrap();
    }
    region_store.sync();
    entity_store.sync();
    region_store.train_dict_if_needed();
    entity_store.train_dict_if_needed();

    // edit rounds (same schedule as bench)
    let per_round = n / 8;
    for round in 1..=rounds as u64 {
        for i in 0..per_round {
            let ci = (splitmix((round as u64) << 40 ^ i as u64) as usize) % n;
            let (cx, cz) = coords[ci];
            region_store.write_chunk(key(cx, cz), &chunk_payload(&w, cx, cz, round), false).unwrap();
            if ci % 4 == 0 {
                entity_store.write_chunk(key(cx, cz), &entity_payload(cx, cz, round), true).unwrap();
            }
        }
    }
    region_store.sync();
    entity_store.sync();
    region_store.train_dict_if_needed();
    entity_store.train_dict_if_needed();
    region_store.seal();
    entity_store.seal();
    while region_store.compact_step() || entity_store.compact_step() {}

    println!("\n--- container accounting (exact) ---");
    println!("region:   {}", region_store.profile_json());
    println!("region hist: {}", region_store.blob_histogram_json());
    println!("entities: {}", entity_store.profile_json());
    println!("entity hist: {}", entity_store.blob_histogram_json());

    // ---- collect final payloads for component analysis ----
    let mut edited: Map<u64, u64> = Map::new();
    for round in 1..=rounds as u64 {
        for i in 0..per_round {
            let ci = (splitmix((round as u64) << 40 ^ i as u64) as usize) % n;
            let (cx, cz) = coords[ci];
            edited.insert(key(cx, cz), round);
        }
    }

    let t0 = Instant::now();
    let mut sec_raw = 0u64;
    let mut sec_air_n = 0u64;
    let mut sec_air_raw = 0u64;
    let mut sec_water_n = 0u64;
    let mut sec_water_raw = 0u64;
    let mut sec_packed_n = 0u64;
    let mut sec_packed_raw = 0u64;
    let mut tail_raw = 0u64;
    let mut hmap_raw = 0u64;
    let mut ent_raw = 0u64;
    let mut ent_empty_n = 0u64;
    let mut ent_full_n = 0u64;

    let mut section_samples: Vec<Vec<u8>> = Vec::new();
    let mut sections_per_chunk: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut tails: Vec<Vec<u8>> = Vec::new();
    let mut ents: Vec<Vec<u8>> = Vec::new();
    let mut custom_sections_per_chunk: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut custom_ok = 0usize;
    let mut custom_bail = 0usize;

    for &(cx, cz) in &coords {
        let round = *edited.get(&key(cx, cz)).unwrap_or(&0);
        let p = region_store.read_chunk(key(cx, cz)).expect("region chunk");
        let e = entity_store.read_chunk(key(cx, cz)).expect("entity chunk");
        ent_raw += e.len() as u64;
        if e.len() < 96 {
            ent_empty_n += 1;
        } else {
            ent_full_n += 1;
        }
        ents.push(e);
        let root: Value = fastnbt::from_bytes(&p).unwrap();
        let Value::Compound(m) = &root else { panic!("compound") };
        let Value::List(secs) = m.get("sections").unwrap() else { panic!("sections") };
        let mut my_secs = Vec::new();
        let mut my_custom = Vec::new();
        for s in secs {
            let b = tb(s);
            sec_raw += b.len() as u64;
            // classify
            let Value::Compound(sm) = s else { continue };
            let uniform = match sm.get("block_states") {
                Some(Value::Compound(bs)) => matches!(bs.get("palette"), Some(Value::List(l)) if l.len() == 1),
                _ => true,
            };
            let waterish = match sm.get("block_states") {
                Some(Value::Compound(bs)) => matches!(
                    bs.get("palette"),
                    Some(Value::List(l)) if l.len() == 1 && matches!(&l[0], Value::String(s) if s.contains("water"))
                ),
                _ => false,
            };
            let airish = match sm.get("block_states") {
                Some(Value::Compound(bs)) => matches!(
                    bs.get("palette"),
                    Some(Value::List(l)) if l.len() == 1 && matches!(&l[0], Value::String(s) if s.contains("air"))
                ),
                _ => false,
            };
            if uniform && airish {
                sec_air_n += 1;
                sec_air_raw += b.len() as u64;
            } else if uniform && waterish {
                sec_water_n += 1;
                sec_water_raw += b.len() as u64;
            } else {
                sec_packed_n += 1;
                sec_packed_raw += b.len() as u64;
            }
            if section_samples.len() < 1200 && !uniform {
                section_samples.push(b.clone());
            }
            my_secs.push(b.clone());
            if let Some(c) = section_encode(s) {
                if custom_ok < 300 {
                    let back = section_decode(&c).expect("custom decode");
                    if &back != s {
                        nbt_diff("sec", s, &back);
                        panic!("custom section codec round-trip mismatch");
                    }
                }
                custom_ok += 1;
                my_custom.push(c);
            } else {
                custom_bail += 1;
                my_custom.push(b);
            }
        }
        sections_per_chunk.push(my_secs);
        custom_sections_per_chunk.push(my_custom);
        let mut tm = m.clone();
        tm.remove("sections");
        // heightmap weight
        if let Some(Value::Compound(hm)) = tm.get("Heightmaps") {
            for (_k, v) in hm.iter() {
                if let Value::LongArray(la) = v {
                    hmap_raw += (la.len() * 8) as u64;
                }
            }
        }
        let tb = tb(&Value::Compound(tm));
        tail_raw += tb.len() as u64;
        tails.push(tb);
    }
    let gen_secs = t0.elapsed().as_secs_f64();

    println!("\n--- component weights (raw, logical) ---");
    println!(
        "sections total   {:>9.2} MiB  ({} secs: air {} x {:.0}B, water {} x {:.0}B, packed {} x {:.0}B)",
        mib(sec_raw),
        sec_air_n + sec_water_n + sec_packed_n,
        sec_air_n,
        sec_air_raw / sec_air_n.max(1),
        sec_water_n,
        sec_water_raw / sec_water_n.max(1),
        sec_packed_n,
        sec_packed_raw / sec_packed_n.max(1)
    );
    println!("  of which heightmaps raw {:.2} MiB", mib(hmap_raw));
    println!("tail (no sections) {:>7.2} MiB  ({} x {:.0}B)", mib(tail_raw), tails.len(), tail_raw / tails.len() as u64);
    println!(
        "entities           {:>7.2} MiB  ({} chunks: {} empty, {} full)",
        mib(ent_raw),
        ents.len(),
        ent_empty_n,
        ent_full_n
    );
    println!("custom codec: ok={custom_ok} bail={custom_bail} (gen+parse {gen_secs:.1}s)");

    // tails with heightmaps repacked to dense bytes (needed by S2 too)
    let mut repacked = 0usize;
    let mut tails2: Vec<Vec<u8>> = Vec::with_capacity(tails.len());
    for tbi in &tails {
        let v: Value = fastnbt::from_bytes(tbi).unwrap();
        let mut m = match v { Value::Compound(m) => m, _ => unreachable!() };
        let mut changed = false;
        if let Some(Value::Compound(hm)) = m.get_mut("Heightmaps") {
            for (_k, val) in hm.iter_mut() {
                if let Value::LongArray(la) = val {
                    if let Some(dense) = hmap_repack(la.as_ref()) {
                        *val = Value::ByteArray(fastnbt::ByteArray::new(
                            dense.iter().map(|x| *x as i8).collect(),
                        ));
                        changed = true;
                    }
                }
            }
        }
        if changed {
            repacked += 1;
        }
        tails2.push(tb(&Value::Compound(m)));
    }
    println!("hmap repack: {repacked}/{} tails", tails2.len());
    // ---- offline experiments ----
    println!("\n--- offline encoding experiments (post-compaction content) ---");
    let dict112 = train_dict(&section_samples, 112 * 1024);
    let dict512 = train_dict(&section_samples, 512 * 1024);
    let c = |dict: &Option<Vec<u8>>, level: i32| Codec { dict: dict.clone(), level };

    // V0: control — per-section NBT, dict112 L19 (mirrors current cold tier)
    let t = Instant::now();
    let codec = c(&dict112, 19);
    let mut v0 = 0u64;
    for secs in &sections_per_chunk {
        for s in secs {
            v0 += codec.comp(s).len() as u64;
        }
    }
    println!("V0 per-section NBT + dict112 L19          {:>7.2} MiB  ({:.1}s)", mib(v0), t.elapsed().as_secs_f64());

    // V1: L22
    let t = Instant::now();
    let codec = c(&dict112, 22);
    let mut v1 = 0u64;
    for secs in &sections_per_chunk {
        for s in secs {
            v1 += codec.comp(s).len() as u64;
        }
    }
    println!("V1 per-section NBT + dict112 L22          {:>7.2} MiB  ({:.1}s)", mib(v1), t.elapsed().as_secs_f64());

    // V2: custom section codec, per-section, dict112 L19
    let t = Instant::now();
    let codec = c(&dict112, 19);
    let mut v2 = 0u64;
    for secs in &custom_sections_per_chunk {
        for s in secs {
            v2 += codec.comp(s).len() as u64;
        }
    }
    println!("V2 per-section CUSTOM + dict112 L19       {:>7.2} MiB  ({:.1}s)", mib(v2), t.elapsed().as_secs_f64());

    // V3: custom sections grouped per chunk (one zstd call per chunk)
    let t = Instant::now();
    let codec = c(&dict112, 19);
    let mut v3 = 0u64;
    for secs in &custom_sections_per_chunk {
        let mut buf = Vec::new();
        for s in secs {
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s);
        }
        v3 += codec.comp(&buf).len() as u64;
    }
    println!("V3 chunk-group CUSTOM + dict112 L19       {:>7.2} MiB  ({:.1}s)", mib(v3), t.elapsed().as_secs_f64());

    // V4: V3 with dict512
    let t = Instant::now();
    let codec = c(&dict512, 19);
    let mut v4 = 0u64;
    for secs in &custom_sections_per_chunk {
        let mut buf = Vec::new();
        for s in secs {
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s);
        }
        v4 += codec.comp(&buf).len() as u64;
    }
    println!("V4 chunk-group CUSTOM + dict512 L19       {:>7.2} MiB  (+dict {:.0}KB) ({:.1}s)", mib(v4), dict512.as_ref().map(|d| d.len()).unwrap_or(0) as f64 / 1024.0, t.elapsed().as_secs_f64());

    // V5: V0 with dict512 (isolate dict effect on NBT path)
    let t = Instant::now();
    let codec = c(&dict512, 19);
    let mut v5 = 0u64;
    for secs in &sections_per_chunk {
        for s in secs {
            v5 += codec.comp(s).len() as u64;
        }
    }
    println!("V5 per-section NBT + dict512 L19          {:>7.2} MiB  ({:.1}s)", mib(v5), t.elapsed().as_secs_f64());

    // S1/S2: honest slab-stream emulation (what the container really does).
    // Blobs in write order = per chunk: unique sections (content dedup) then
    // tail (>=512B -> blob). Slab stream = 24B entry-table row per member +
    // raw bytes, grouped at ~192KB raw, compressed as one call.
    let blob_of = |b: &[u8]| xxh(b);
    let build_blobs = |custom: bool| -> Vec<Vec<u8>> {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut blobs = Vec::new();
        for ci in 0..sections_per_chunk.len() {
            let secs = if custom { &custom_sections_per_chunk[ci] } else { &sections_per_chunk[ci] };
            for s in secs {
                let h = blob_of(s);
                if seen.insert(h) {
                    blobs.push(s.clone());
                }
            }
            let tail = if custom { &tails2[ci] } else { &tails[ci] };
            if tail.len() >= 512 {
                let h = blob_of(tail);
                if seen.insert(h) {
                    blobs.push(tail.clone());
                }
            }
        }
        blobs
    };
    let slab_stream_size = |blobs: &[Vec<u8>]| -> u64 {
        let mut total = 0u64;
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_raw = 0usize;
        for b in blobs {
            if !cur.is_empty() && cur_raw + b.len() > 192 * 1024 {
                total += codec_comp(&cur, &dict112, 19).len() as u64;
                cur.clear();
                cur_raw = 0;
            }
            cur.extend_from_slice(&(0u32).to_le_bytes()); // id
            cur.extend_from_slice(&(b.len() as u32).to_le_bytes()); // raw len
            cur.extend_from_slice(&[0u8; 16]); // hash
            cur.extend_from_slice(b);
            cur_raw += 24 + b.len();
        }
        if !cur.is_empty() {
            total += codec_comp(&cur, &dict112, 19).len() as u64;
        }
        total
    };
    let t = Instant::now();
    let s1 = slab_stream_size(&build_blobs(false));
    println!("S1 slab-stream NBT (== current container) {:>7.2} MiB  ({:.1}s)", mib(s1), t.elapsed().as_secs_f64());
    let t = Instant::now();
    let s2 = slab_stream_size(&build_blobs(true));
    println!("S2 slab-stream CUSTOM sections+tails      {:>7.2} MiB  ({:.1}s)", mib(s2), t.elapsed().as_secs_f64());

    // S3: bigger slab groups (1MB) — longer match window across chunks
    let slab_stream_size_n = |blobs: &[Vec<u8>], group: usize| -> u64 {
        let mut total = 0u64;
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_raw = 0usize;
        for b in blobs {
            if !cur.is_empty() && cur_raw + b.len() > group {
                total += codec_comp(&cur, &dict112, 19).len() as u64;
                cur.clear();
                cur_raw = 0;
            }
            cur.extend_from_slice(&(0u32).to_le_bytes());
            cur.extend_from_slice(&(b.len() as u32).to_le_bytes());
            cur.extend_from_slice(&[0u8; 16]);
            cur.extend_from_slice(b);
            cur_raw += 24 + b.len();
        }
        if !cur.is_empty() {
            total += codec_comp(&cur, &dict112, 19).len() as u64;
        }
        total
    };
    let s3 = slab_stream_size_n(&build_blobs(true), 1024 * 1024);
    println!("S3 slab-stream CUSTOM 1MB groups          {:>7.2} MiB  ({:.1}s)", mib(s3), t.elapsed().as_secs_f64());
    let s3b = slab_stream_size_n(&build_blobs(true), 16 * 1024 * 1024);
    println!("S3b slab-stream CUSTOM 16MB groups        {:>7.2} MiB  ({:.1}s)", mib(s3b), t.elapsed().as_secs_f64());

    // tail experiments: control vs hmap-repacked
    let ent_samples: Vec<Vec<u8>> = ents.iter().take(600).cloned().collect();
    let edict = train_dict(&ent_samples, 112 * 1024);
    let t = Instant::now();
    let codec = c(&dict112, 19);
    let mut t_ctrl = 0u64;
    for tbi in &tails {
        t_ctrl += codec.comp(tbi).len() as u64;
    }
    println!("T0 tail NBT per-chunk + dict112 L19       {:>7.2} MiB  ({:.1}s)", mib(t_ctrl), t.elapsed().as_secs_f64());

    let mut t_rep = 0u64;
    for tbi in &tails2 {
        t_rep += codec.comp(tbi).len() as u64;
    }
    println!("T1 tail hmap-repacked + dict112 L19       {:>7.2} MiB  (repacked {repacked}/{}) ({:.1}s)", mib(t_rep), tails2.len(), t.elapsed().as_secs_f64());

    // T2: tails with delta-varint heightmaps
    fn zvar(x: i64, out: &mut Vec<u8>) {
        let z = ((x << 1) ^ (x >> 63)) as u64;
        let mut z = z;
        loop {
            let b = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 { out.push(b); break; }
            out.push(b | 0x80);
        }
    }
    let mut tails3: Vec<Vec<u8>> = Vec::with_capacity(tails.len());
    for tbi in &tails {
        let v: Value = fastnbt::from_bytes(tbi).unwrap();
        let mut m = match v { Value::Compound(m) => m, _ => unreachable!() };
        if let Some(Value::Compound(hm)) = m.get_mut("Heightmaps") {
            for (_k, val) in hm.iter_mut() {
                if let Value::LongArray(la) = val {
                    if la.len() == 37 {
                        let entries = unpack_nostroddle(la.as_ref(), 256, 9);
                        if !entries.iter().any(|e| *e as u32 >= 512) {
                            let mut out = vec![0xC6u8, 2u8];
                            let mut prev: i64 = 0;
                            for e in &entries {
                                zvar(*e as i64 - prev, &mut out);
                                prev = *e as i64;
                            }
                            *val = Value::ByteArray(fastnbt::ByteArray::new(
                                out.iter().map(|x| *x as i8).collect(),
                            ));
                        }
                    }
                }
            }
        }
        tails3.push(tb(&Value::Compound(m)));
    }
    let mut t_dv = 0u64;
    for tbi in &tails3 {
        t_dv += codec.comp(tbi).len() as u64;
    }
    println!("T2 tail hmap delta-varint + dict112 L19   {:>7.2} MiB  ({:.1}s)", mib(t_dv), t.elapsed().as_secs_f64());

    // entities: whole vs field-split
    let t = Instant::now();
    let ecodec = c(&edict, 19);
    let mut e_whole = 0u64;
    for e in &ents {
        e_whole += ecodec.comp(e).len() as u64;
    }
    println!("E0 entity whole + dict112 L19 (data only) {:>7.2} MiB  ({:.1}s)", mib(e_whole), t.elapsed().as_secs_f64());

    // field split: DataVersion shared, Position inline raw, Entities deduped
    let t = Instant::now();
    let ecodec = c(&edict, 19);
    let mut e_whole = 0u64;
    for e in &ents {
        e_whole += ecodec.comp(e).len() as u64;
    }
    println!("E0 entity whole + dict112 L19 (data only) {:>7.2} MiB  ({:.1}s)", mib(e_whole), t.elapsed().as_secs_f64());

    let mut seen: std::collections::HashMap<u64, u64> = std::collections::HashMap::new(); // hash -> comp len (first time)
    let mut e_split_blob = 0u64;
    let mut pos_inline = 0u64;
    let mut key_ovh = 0u64;
    for e in &ents {
        let v: Value = fastnbt::from_bytes(e).unwrap();
        let Value::Compound(m) = v else { unreachable!() };
        for (k, val) in &m {
            key_ovh += k.len() as u64 + 2; // key name + kind byte in recipe
            if k == "Position" {
                pos_inline += val_bytes(val) + 2;
                continue;
            }
            let vb = tb(&Value::Compound(
                [(k.clone(), val.clone())].into_iter().collect(),
            ));
            let h = xxh(vb.as_slice());
            let comp_len = *seen.entry(h).or_insert_with(|| ecodec.comp(&vb).len() as u64);
            e_split_blob += comp_len; // dedup: only first occurrence pays
        }
    }
    key_ovh += ents.len() as u64 * 8; // blob-id varints per part
    println!(
        "E1 entity field-split estimate            {:>7.2} MiB  (blob-data {:.2} + pos-inline {:.2} + recipe-ovh {:.2}) ({:.1}s)",
        mib(e_split_blob + pos_inline + key_ovh),
        mib(e_split_blob),
        mib(pos_inline),
        mib(key_ovh),
        t.elapsed().as_secs_f64()
    );

    // projection
    println!("\n--- projection ---");
    let anvil_note = "(add anvil baseline from bench_size for the ratio)";
    let best_secs = v4 + dict512.as_ref().map(|d| d.len()).unwrap_or(0) as u64;
    let best_tail = t_rep;
    let best_ent = e_split_blob + pos_inline + key_ovh;
    println!(
        "region sections {:.2} + tails {:.2} + entities {:.2} + recipes/frames ~{:.2} + dicts {:.2} => ~{:.2} MiB total {}",
        mib(best_secs),
        mib(best_tail),
        mib(best_ent),
        mib(9216 * 90), // rough recipe+framing estimate at ~90B/chunk
        mib(dict512.as_ref().map(|d| d.len()).unwrap_or(0) as u64 + edict.as_ref().map(|d| d.len()).unwrap_or(0) as u64),
        mib(best_secs + best_tail + best_ent + 9216 * 90),
        anvil_note
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// tiny helper for the E1 experiment
fn xxh(b: &[u8]) -> u64 {
    use xxhash_rust::xxh3::xxh3_64;
    xxh3_64(b)
}

fn nbt_diff(path: &str, a: &Value, b: &Value) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Compound(ma), Value::Compound(mb)) => {
            for (k, va) in ma {
                match mb.get(k) {
                    Some(vb) => nbt_diff(&format!("{path}.{k}"), va, vb),
                    None => println!("DIFF {path}.{k}: missing in decode"),
                }
            }
            for k in mb.keys() {
                if !ma.contains_key(k) {
                    println!("DIFF {path}.{k}: extra in decode");
                }
            }
        }
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                println!("DIFF {path}: list len {} vs {}", la.len(), lb.len());
                return;
            }
            for (i, (va, vb)) in la.iter().zip(lb.iter()).enumerate() {
                nbt_diff(&format!("{path}[{i}]"), va, vb);
            }
        }
        (Value::LongArray(x), Value::LongArray(y)) => {
            println!(
                "DIFF {path}: longarray len {} vs {}, first-diff at {:?}",
                x.len(),
                y.len(),
                x.iter().zip(y.iter()).position(|(p, q)| p != q)
            );
            if let Some(pos) = x.iter().zip(y.iter()).position(|(p, q)| p != q) {
                println!("  orig[{}]={} dec[{}]={}", pos, x[pos], pos, y[pos]);
            }
        }
        _ => println!("DIFF {path}: {a:?} vs {b:?}"),
    }
}

fn val_bytes(v: &Value) -> u64 {
    match v {
        Value::Byte(_) => 1,
        Value::Short(_) => 2,
        Value::Int(_) => 4,
        Value::Long(_) => 8,
        Value::Float(_) => 4,
        Value::Double(_) => 8,
        Value::ByteArray(b) => 4 + b.len() as u64,
        Value::String(s) => 2 + s.len() as u64,
        Value::List(l) => 5 + l.iter().map(val_bytes).sum::<u64>(),
        Value::Compound(m) => 1 + m.iter().map(|(k, v)| 1 + k.len() as u64 + val_bytes(v)).sum::<u64>(),
        Value::IntArray(a) => 4 + a.len() as u64 * 4,
        Value::LongArray(a) => 4 + a.len() as u64 * 8,
    }
}

fn codec_comp(data: &[u8], dict: &Option<Vec<u8>>, level: i32) -> Vec<u8> {
    match dict {
        Some(d) => {
            let mut c = zstd::bulk::Compressor::new(level).unwrap();
            c.set_dictionary(level, d).unwrap();
            c.compress(data).unwrap()
        }
        None => zstd::bulk::compress(data, level).unwrap(),
    }
}
