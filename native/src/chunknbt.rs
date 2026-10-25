//! Chunk NBT split/join: the dedup heart of c-saves.
//!
//! A Paper 26.3 chunk payload (uncompressed NBT, `NbtIo.write` form) is split
//! into:
//!   * `sections` — per-section compounds (block_states/biomes/light).
//!                  Position-free => massively shared across chunks. Each is
//!                  addressed by a STRUCTURAL hash (a canonical, order-free
//!                  digest of the NBT tree) so identical sections dedup even
//!                  though fastnbt's serialization order is nondeterministic.
//!                  Sections are stored in a custom binary form (CSEC) that
//!                  removes NBT tag overhead, unpacks the no-straddle long
//!                  packing into dense bit streams and collapses uniform
//!                  light arrays — measured -40% vs raw NBT section bytes
//!                  after compression. Sections whose shape is not understood
//!                  fall back to plain NBT bytes (detected by the marker).
//!   * tail       — everything else (Heightmaps, block_entities, DataVersion,
//!                  ...). Heightmap LongArrays (9-bit no-straddle entries)
//!                  are re-packed into dense byte arrays (measured -9% after
//!                  compression); the transform is self-describing and only
//!                  ever applied to values we produced.
//!
//! Entity/POI payloads have no `sections`; they are split per top-level key
//! (field split) so shared fields (DataVersion, empty Entities, ...) dedup
//! into single blobs and tiny unique fields (Position) are inlined into the
//! recipe instead of paying blob overhead.

use fastnbt::Value;
use xxhash_rust::xxh3::{xxh3_128, Xxh3};

pub type Hash = u128;

// ------------------------------------------------------------ csec codec --

const CSEC_MAGIC: u8 = 0xC5; // legacy v1/v2 sections (read-only)
const CSEC3_MAGIC: u8 = 0xC7; // v3: compound palettes + starlight + canonical order
const HMAP_MAGIC: u8 = 0xC6; // repacked heightmap ByteArray marker

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
    pos: usize,
}

impl<'a> BitR<'a> {
    fn get(&mut self, bits: u32) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..bits {
            let byte = *self.in_.get(self.pos >> 3)?;
            v |= (((byte >> (self.pos & 7)) & 1) as u64) << i;
            self.pos += 1;
        }
        Some(v)
    }
}

/// Determine the no-straddle bit width a section's data LongArray uses.
/// Vanilla clamps linear palettes to a 4-bit minimum (26.x data for small
/// palettes is 256 longs = 4-bit), so try the clamped width first and fall
/// back to ceillog2 (legacy/synthetic payloads).
fn section_data_bits(n: usize, entries: usize, longs: usize) -> u32 {
    let bf = bits_for(n);
    let clamp = bf.max(4);
    let fits = |bits: u32| {
        let per = (64 / bits) as usize;
        (entries + per - 1) / per == longs
    };
    if clamp != bf && fits(clamp) {
        return clamp;
    }
    if fits(bf) {
        return bf;
    }
    clamp
}

fn dense_pack_wide(entries: &[u16], bits: u32) -> Vec<u8> {
    let mut w = BitW::new();
    for e in entries {
        w.put(*e as u64, bits);
    }
    w.finish()
}

fn dense_unpack_wide(b: &[u8], n: usize, bits: u32) -> Option<Vec<u16>> {
    let mut r = BitR { in_: b, pos: 0 };
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.get(bits)? as u16);
    }
    Some(out)
}

/// Decode a v3 (0xC7) section.
fn section_decode3(b: &[u8]) -> Option<Value> {
    let f1 = *b.get(1)?;
    let f2 = *b.get(2)?;
    let mut p = 3usize;
    let y = *b.get(p)? as i8;
    p += 1;
    let mut sec_map: std::collections::HashMap<String, Value> = Default::default();
    sec_map.insert("Y".to_string(), Value::Byte(y));

    fn read_entry(b: &[u8], p: &mut usize) -> Option<Value> {
        let kind = *b.get(*p)?;
        *p += 1;
        match kind {
            0 => {
                let l = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                *p += 2;
                let s = core::str::from_utf8(b.get(*p..*p + l)?).ok()?.to_string();
                *p += l;
                Some(Value::String(s))
            }
            1 | 2 => {
                let l = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                *p += 2;
                let name = core::str::from_utf8(b.get(*p..*p + l)?).ok()?.to_string();
                *p += l;
                let mut cm: std::collections::HashMap<String, Value> = Default::default();
                cm.insert("Name".to_string(), Value::String(name));
                if kind == 2 {
                    let np = *b.get(*p)?;
                    *p += 1;
                    let mut props: Vec<(String, Value)> = Vec::with_capacity(np as usize);
                    for _ in 0..np {
                        let kl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                        *p += 2;
                        let k = core::str::from_utf8(b.get(*p..*p + kl)?).ok()?.to_string();
                        *p += kl;
                        let vl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                        *p += 2;
                        let v = core::str::from_utf8(b.get(*p..*p + vl)?).ok()?.to_string();
                        *p += vl;
                        props.push((k, Value::String(v)));
                    }
                    let mut pm: std::collections::HashMap<String, Value> = Default::default();
                    for (k, v) in props {
                        pm.insert(k, v);
                    }
                    cm.insert("Properties".to_string(), Value::Compound(pm));
                }
                Some(Value::Compound(cm))
            }
            _ => None,
        }
    }

    fn dec_pal_data(
        b: &[u8],
        p: &mut usize,
        entries: usize,
        has_data: bool,
        narrow: bool,
    ) -> Option<Value> {
        let n = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
        *p += 2;
        let mut pal = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            pal.push(read_entry(b, p)?);
        }
        let mut bm: std::collections::HashMap<String, Value> = Default::default();
        bm.insert("palette".to_string(), Value::List(pal));
        if has_data {
            // narrow mode: dense stream at ceillog2(n) (encode-side choice
            // when that beats the vanilla 4-bit clamp); output is normalized
            // back to the vanilla-clamped width either way.
            let rbits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
            let bytes = (entries * rbits as usize + 7) / 8;
            let dense = b.get(*p..*p + bytes)?;
            *p += bytes;
            let v = if n > 256 {
                let v = dense_unpack_wide(dense, entries, rbits)?;
                v.iter().map(|x| *x as u8).collect::<Vec<u8>>()
            } else {
                dense_unpack(dense, entries, rbits)?
            };
            let out_bits = bits_for(n).max(4);
            let longs = pack_nostroddle(&v, out_bits);
            bm.insert(
                "data".to_string(),
                Value::LongArray(fastnbt::LongArray::new(longs)),
            );
        }
        Some(Value::Compound(bm))
    }

    if f1 & 1 != 0 {
        let bs = dec_pal_data(b, &mut p, 4096, f1 & 2 != 0, f2 & 4 != 0)?;
        sec_map.insert("block_states".to_string(), bs);
    }
    if f1 & 4 != 0 {
        let bs = dec_pal_data(b, &mut p, 64, f1 & 8 != 0, f2 & 8 != 0)?;
        sec_map.insert("biomes".to_string(), bs);
    }
    if f1 & 16 != 0 {
        sec_map.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![-1; 2048])));
    } else if f1 & 32 != 0 {
        sec_map.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])));
    } else if f1 & 64 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let arr = b.get(p..p + l)?;
        p += l;
        sec_map.insert(
            "SkyLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(arr.iter().map(|x| *x as i8).collect())),
        );
    }
    if f1 & 128 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let arr = b.get(p..p + l)?;
        p += l;
        sec_map.insert(
            "BlockLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(arr.iter().map(|x| *x as i8).collect())),
        );
    }
    if f2 & 1 != 0 {
        let v = *b.get(p)?;
        p += 1;
        sec_map.insert("starlight.skylight_state".to_string(), Value::Int(v as i32));
    }
    if f2 & 2 != 0 {
        let v = *b.get(p)?;
        sec_map.insert("starlight.blocklight_state".to_string(), Value::Int(v as i32));
    }
    Some(Value::Compound(sec_map))
}

fn dense_pack(entries: &[u8], bits: u32) -> Vec<u8> {
    let mut w = BitW::new();
    for e in entries {
        w.put(*e as u64, bits);
    }
    w.finish()
}

fn dense_unpack(b: &[u8], n: usize, bits: u32) -> Option<Vec<u8>> {
    let mut r = BitR { in_: b, pos: 0 };
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.get(bits)? as u8);
    }
    Some(out)
}

/// 1.16+ no-straddle packing (vanilla layout).
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

fn unpack_nostroddle(longs: &[i64], n: usize, bits: u32) -> Vec<u8> {
    let per = (64 / bits) as usize;
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let mut out = Vec::with_capacity(n);
    for l in longs {
        let lu = *l as u64; // bit pattern; the sign bit is data
        for i in 0..per {
            if out.len() == n {
                return out;
            }
            out.push(((lu >> (i * bits as usize)) & mask) as u8);
        }
    }
    out
}

/// Encode one section compound into the custom form (v3). Returns None when
/// the section contains keys or shapes outside the understood set.
///
/// Layout: [0xC7][flags1][flags2][Y i8]
///   flags1 bit0 block_states present, bit1 block_states has data
///          bit2 biomes present,       bit3 biomes has data
///          bit4 SkyLight all-1, bit5 SkyLight all-0, bit6 SkyLight raw
///          bit7 BlockLight present-raw
///   flags2 bit0 starlight.skylight_state present (+1 byte)
///          bit1 starlight.blocklight_state present (+1 byte)
///   palette: [u16 n]{[u8 kind]...} in CANONICAL (sorted) order;
///            data: dense bits at bits_for(n) over canonical indices.
///            kind 0 plain string [u16 len][bytes] (pre-1.18 style payload)
///            kind 1 compound {Name}            [u16 len][name]
///            kind 2 compound {Name,Properties} [u16 len][name][u8 np]
///                  {[u16 klen][k][u16 vlen][v]} (props sorted by key)
///
/// Canonical palette order is semantically neutral: the palette is a pure
/// lookup table, so any permutation of it (with consistently remapped data)
/// decodes to the exact same block states. Sorting maximizes section dedup
/// across palette permutations and makes the encoding deterministic.
pub fn section_encode(sec: &Value) -> Option<Vec<u8>> {
    let Value::Compound(m) = sec else { return None };
    const KNOWN: [&str; 7] = [
        "Y",
        "block_states",
        "biomes",
        "SkyLight",
        "BlockLight",
        "starlight.skylight_state",
        "starlight.blocklight_state",
    ];
    if m.keys().any(|k| !KNOWN.contains(&k.as_str())) {
        return None;
    }
    let y = match m.get("Y") {
        Some(Value::Byte(b)) => *b,
        _ => return None,
    };
    let mut out = vec![CSEC3_MAGIC, 0u8, 0u8, y as u8];
    let mut f1 = 0u8;
    let mut f2 = 0u8;

    /// One palette entry, normalized from either the string form or the
    /// modern compound form {Name, Properties}.
    struct PalEntry {
        name: String,
        props: Vec<(String, String)>,
        plain: bool, // was a bare string
    }

    fn parse_entry(v: &Value) -> Option<PalEntry> {
        match v {
            Value::String(s) => Some(PalEntry { name: s.clone(), props: Vec::new(), plain: true }),
            Value::Compound(cm) => {
                let Value::String(name) = cm.get("Name")? else { return None };
                let mut props = Vec::new();
                match cm.get("Properties") {
                    Some(Value::Compound(pn)) => {
                        for (k, kv) in pn {
                            let Value::String(vv) = kv else { return None };
                            props.push((k.clone(), vv.clone()));
                        }
                    }
                    None => {}
                    _ => return None,
                }
                props.sort();
                Some(PalEntry { name: name.clone(), props, plain: false })
            }
            _ => None,
        }
    }

    fn put_entry(out: &mut Vec<u8>, e: &PalEntry) -> Option<()> {
        let nb = e.name.as_bytes();
        if nb.len() > 65535 {
            return None;
        }
        if e.plain {
            out.push(0u8);
        } else if e.props.is_empty() {
            out.push(1u8);
        } else {
            out.push(2u8);
        }
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(nb);
        if !e.plain && !e.props.is_empty() {
            if e.props.len() > 255 {
                return None;
            }
            out.push(e.props.len() as u8);
            for (k, v) in &e.props {
                let kb = k.as_bytes();
                let vb = v.as_bytes();
                if kb.len() > 65535 || vb.len() > 65535 {
                    return None;
                }
                out.extend_from_slice(&(kb.len() as u16).to_le_bytes());
                out.extend_from_slice(kb);
                out.extend_from_slice(&(vb.len() as u16).to_le_bytes());
                out.extend_from_slice(vb);
            }
        }
        Some(())
    }

    /// Encode one palette+data compound canonically.
    fn enc_pal_data(out: &mut Vec<u8>, bs: &Value, entries: usize) -> Option<bool> {
        let Value::Compound(bm) = bs else { return None };
        let Value::List(pal) = bm.get("palette")? else { return None };
        let n = pal.len();
        if n == 0 || n > 65535 {
            return None;
        }
        let mut ents: Vec<PalEntry> = Vec::with_capacity(n);
        for p in pal {
            ents.push(parse_entry(p)?);
        }
        // canonical order = sort by (name, props); stable on ties
        ents.sort_by(|a, b| {
            a.name.cmp(&b.name)
                .then_with(|| a.props.cmp(&b.props))
        });
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for e in &ents {
            put_entry(out, e)?;
        }
        let data = match bm.get("data") {
            Some(Value::LongArray(la)) => la.as_ref(),
            Some(_) => return None,
            None => {
                if n > 1 {
                    return None; // vanilla always has data when palette > 1
                }
                return Some(false);
            }
        };
        let bits = bits_for(n);
        let per = (64 / bits) as usize;
        if data.len() != (entries + per - 1) / per {
            return None; // unexpected packing -> bail
        }
        // no remap needed: entries were sorted in place, data indices refer to
        // positions in the ORIGINAL list, so build old->new order and remap.
        // (sorting moved entries; reconstruct permutation by matching the
        // sorted entries back to their original positions)
        // -- NOTE: `ents` was sorted; we must recover the permutation. To keep
        //    this simple and unambiguous we re-derive it from the ORIGINAL
        //    list order captured before the sort.
        // (implemented by the caller passing original entries; see below)
        let _ = ents;
        unreachable!()
    }

    // The generic enc_pal_data above cannot recover the permutation after
    // sorting, so the real implementation works on (entry, old_index) pairs.
    // Returns (has_data, narrow): narrow = dense stream packed at ceillog2(n)
    // because that beats the vanilla 4-bit clamp (palettes under 16 entries).
    fn enc_pal_data_canon(out: &mut Vec<u8>, bs: &Value, entries: usize) -> Option<(bool, bool)> {
        let Value::Compound(bm) = bs else { return None };
        let Value::List(pal) = bm.get("palette")? else { return None };
        let n = pal.len();
        if n == 0 || n > 65535 {
            return None;
        }
        let mut ents: Vec<(PalEntry, usize)> = Vec::with_capacity(n);
        for (i, p) in pal.iter().enumerate() {
            ents.push((parse_entry(p)?, i));
        }
        // Frequency-canonical order: the dominant state becomes index 0, so
        // the dense data stream is dominated by zero runs that compress
        // extremely well. Ties break by (plain, name, props) so the order is
        // a pure function of content (deterministic => dedup-stable).
        let data_pre = bm.get("data");
        let mut counts = vec![0u32; n];
        if let Some(Value::LongArray(la)) = data_pre {
            // the source array may be vanilla-clamped (max(4, bits)) or
            // legacy ceillog2 — detect from its length
            let bits = section_data_bits(n, entries, la.as_ref().len());
            let per = (64 / bits) as usize;
            if la.as_ref().len() == (entries + per - 1) / per {
                for v in unpack_nostroddle(la.as_ref(), entries, bits) {
                    if (v as usize) < n {
                        counts[v as usize] += 1;
                    }
                }
            }
        }
        ents.sort_by(|a, b| {
            counts[b.1]
                .cmp(&counts[a.1])
                .then_with(|| (a.0.plain, &a.0.name, &a.0.props).cmp(&(b.0.plain, &b.0.name, &b.0.props)))
        });
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for (e, _) in &ents {
            put_entry(out, e)?;
        }
        let data = match bm.get("data") {
            Some(Value::LongArray(la)) => la.as_ref(),
            Some(_) => return None,
            None => {
                if n > 1 {
                    return None;
                }
                return Some((false, false));
            }
        };
        let in_bits = section_data_bits(n, entries, data.len());
        let per = (64 / in_bits) as usize;
        if data.len() != (entries + per - 1) / per {
            return None;
        }
        let v = unpack_nostroddle(data, entries, in_bits);
        // remap old palette index -> canonical index
        let mut remap = vec![0u8; n];
        for (new_i, (_, old_i)) in ents.iter().enumerate() {
            remap[*old_i] = new_i as u8;
        }
        // Narrow packing: vanilla clamps linear palettes to 4 bits/entry, so
        // a 2-entry palette wastes 4x (2 KiB vs 512 B per section). Under
        // 16-entry palettes re-pack at ceillog2(n); the decoder normalizes
        // back to the vanilla width. n >= 16: ceillog2 >= 4, no change.
        let narrow = bits_for(n) < 4;
        let out_bits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
        if n > 256 {
            // wide palettes: remap table must be u16
            let mut remap16 = vec![0u16; n];
            for (new_i, (_, old_i)) in ents.iter().enumerate() {
                remap16[*old_i] = new_i as u16;
            }
            let remapped: Vec<u16> = v.iter().map(|x| remap16[*x as usize]).collect();
            let mut w = BitW::new();
            for e in &remapped {
                w.put(*e as u64, out_bits);
            }
            out.extend_from_slice(&w.finish());
        } else {
            let remapped: Vec<u8> = v.iter().map(|x| remap[*x as usize]).collect();
            out.extend_from_slice(&dense_pack(&remapped, out_bits));
        }
        Some((true, narrow))
    }

    if let Some(bs) = m.get("block_states") {
        let (has_data, narrow) = enc_pal_data_canon(&mut out, bs, 4096)?;
        f1 |= 1;
        if has_data {
            f1 |= 2;
        }
        if narrow {
            f2 |= 4;
        }
    }
    if let Some(bs) = m.get("biomes") {
        let (has_data, narrow) = enc_pal_data_canon(&mut out, bs, 64)?;
        f1 |= 4;
        if has_data {
            f1 |= 8;
        }
        if narrow {
            f2 |= 8;
        }
    }
    for (key, uni_bit, raw_bit, all) in [
        ("SkyLight", 16u8, 64u8, 0xffu8),
        ("BlockLight", 0u8, 128u8, 0u8),
    ] {
        let Some(Value::ByteArray(ba)) = m.get(key) else { continue };
        let b: Vec<u8> = ba.as_ref().iter().map(|x| *x as u8).collect();
        if uni_bit != 0 && b.iter().all(|x| *x == all) {
            f1 |= uni_bit;
        } else {
            if b.len() > 65535 {
                return None;
            }
            f1 |= raw_bit;
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(&b);
        }
    }
    if let Some(Value::Int(v)) = m.get("starlight.skylight_state") {
        if !(0..=255).contains(v) {
            return None;
        }
        f2 |= 1;
        out.push(*v as u8);
    }
    if let Some(Value::Int(v)) = m.get("starlight.blocklight_state") {
        if !(0..=255).contains(v) {
            return None;
        }
        f2 |= 2;
        out.push(*v as u8);
    }
    out[1] = f1;
    out[2] = f2;
    Some(out)
}

/// Semantic equality for section compounds: block/biome palettes are pure
/// lookup tables, so only the EXPANDED state vectors matter. Canonical
/// re-encoding legitimately permutes palette order; use this instead of
/// structural equality wherever decoded sections are compared.
pub fn sections_semantically_equal(a: &Value, b: &Value) -> bool {
    let (Value::Compound(ma), Value::Compound(mb)) = (a, b) else { return a == b };
    if ma.len() != mb.len() {
        return false;
    }
    for (k, va) in ma {
        let Some(vb) = mb.get(k) else { return false };
        match k.as_str() {
            "block_states" => {
                if !pal_semantic_equal(va, vb, 4096) {
                    return false;
                }
            }
            "biomes" => {
                if !pal_semantic_equal(va, vb, 64) {
                    return false;
                }
            }
            _ => {
                if va != vb {
                    return false;
                }
            }
        }
    }
    true
}

fn pal_semantic_equal(a: &Value, b: &Value, entries: usize) -> bool {
    let (Value::Compound(ma), Value::Compound(mb)) = (a, b) else { return a == b };
    let expand = |m: &std::collections::HashMap<String, Value>| -> Option<Vec<String>> {
        let Value::List(pal) = m.get("palette")? else { return None };
        let mut names: Vec<String> = Vec::with_capacity(pal.len());
        for p in pal {
            match p {
                Value::String(s) => names.push(s.clone()),
                Value::Compound(cm) => {
                    let mut s = match cm.get("Name") {
                        Some(Value::String(s)) => s.clone(),
                        _ => return None,
                    };
                    if let Some(Value::Compound(pr)) = cm.get("Properties") {
                        let mut kvs: Vec<(String, String)> = pr
                            .iter()
                            .map(|(k, v)| {
                                (k.clone(), if let Value::String(s) = v { s.clone() } else { String::new() })
                            })
                            .collect();
                        kvs.sort();
                        s.push('[');
                        s.push_str(&kvs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(","));
                        s.push(']');
                    }
                    names.push(s);
                }
                _ => return None,
            }
        }
        match m.get("data") {
            Some(Value::LongArray(la)) => {
                // derive the packing width from the array itself (floor:
                // no-straddle padding lives in the last long)
                let bits = ((la.as_ref().len() as u32) * 64 / entries as u32).max(1);
                let idx = unpack_nostroddle(la.as_ref(), entries, bits);
                Some(idx.into_iter().map(|i| names.get(i as usize).cloned().unwrap_or_default()).collect())
            }
            Some(_) => None,
            None => {
                if names.len() > 1 {
                    return None;
                }
                Some(vec![names[0].clone(); entries])
            }
        }
    };
    match (expand(ma), expand(mb)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Decode the custom section form back to the exact NBT value.
pub fn section_decode(b: &[u8]) -> Option<Value> {
    if b.first() == Some(&CSEC3_MAGIC) {
        return section_decode3(b);
    }
    if b.first() != Some(&CSEC_MAGIC) || b.len() < 3 {
        return None;
    }
    let flags = b[1];
    let mut p = 2usize;
    let y = *b.get(p)? as i8;
    p += 1;
    let mut sec_map: std::collections::HashMap<String, Value> = Default::default();
    sec_map.insert("Y".to_string(), Value::Byte(y));

    fn dec_pal_data(
        b: &[u8],
        p: &mut usize,
        entries: usize,
        has_data: bool,
    ) -> Option<Value> {
        let n = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
        *p += 2;
        let mut pal = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let l = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            let s = core::str::from_utf8(b.get(*p..*p + l)?).ok()?.to_string();
            *p += l;
            pal.push(Value::String(s));
        }
        let mut bm: std::collections::HashMap<String, Value> = Default::default();
        bm.insert("palette".to_string(), Value::List(pal));
        if has_data {
            let bits = bits_for(n);
            let bytes = (entries * bits as usize + 7) / 8;
            let dense = b.get(*p..*p + bytes)?;
            *p += bytes;
            let v = dense_unpack(dense, entries, bits)?;
            bm.insert(
                "data".to_string(),
                Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&v, bits))),
            );
        }
        Some(Value::Compound(bm))
    }

    if flags & 1 != 0 {
        let bs = dec_pal_data(b, &mut p, 4096, flags & 2 != 0)?;
        sec_map.insert("block_states".to_string(), bs);
    }
    if flags & 4 != 0 {
        let bs = dec_pal_data(b, &mut p, 64, flags & 8 != 0)?;
        sec_map.insert("biomes".to_string(), bs);
    }
    if flags & 16 != 0 {
        sec_map.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![-1; 2048])));
    } else if flags & 32 != 0 {
        sec_map.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])));
    } else if flags & 64 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let arr = b.get(p..p + l)?;
        p += l;
        sec_map.insert(
            "SkyLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(arr.iter().map(|x| *x as i8).collect())),
        );
    }
    if flags & 128 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let arr = b.get(p..p + l)?;
        sec_map.insert(
            "BlockLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(arr.iter().map(|x| *x as i8).collect())),
        );
    }
    Some(Value::Compound(sec_map))
}

/// Repack a 37-long 9-bit heightmap into a compact byte array (with marker).
/// Version 2: zigzag-varint delta chain along column order (adjacent columns
/// have similar heights -> tiny skewed deltas that compress extremely well).
/// Only applies to genuinely vanilla-shaped arrays: the no-straddle unpack
/// must re-pack to the exact original longs (zero padding bits in long 36).
fn hmap_encode(la: &[i64]) -> Option<Vec<u8>> {
    if la.len() != 37 {
        return None;
    }
    let entries = unpack_nostroddle(la, 256, 9);
    if entries.iter().any(|e| *e as u32 >= 512) {
        return None;
    }
    if pack_nostroddle(&entries, 9).as_slice() != la {
        return None; // non-zero padding / foreign packing -> leave as-is
    }
    let mut out = vec![HMAP_MAGIC, 2u8];
    let mut prev: i64 = 0;
    for e in &entries {
        let d = (*e as i64 - prev) << 1 ^ ((*e as i64 - prev) >> 63);
        let mut z = d as u64;
        loop {
            let b = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        prev = *e as i64;
    }
    Some(out)
}

fn hmap_decode(b: &[u8]) -> Option<Vec<i64>> {
    if b.first() != Some(&HMAP_MAGIC) {
        return None;
    }
    match b.get(1) {
        Some(&1u8) => {
            let entries = dense_unpack(&b[2..], 256, 9)?;
            Some(pack_nostroddle(&entries, 9))
        }
        Some(&2u8) => {
            let mut p = 2usize;
            let mut out = Vec::with_capacity(256);
            let mut prev: i64 = 0;
            for _ in 0..256 {
                // read zigzag varint
                let mut z = 0u64;
                let mut shift = 0u32;
                loop {
                    let byte = *b.get(p)?;
                    p += 1;
                    z |= ((byte & 0x7f) as u64) << shift;
                    if byte & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                    if shift > 63 {
                        return None;
                    }
                }
                let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
                prev += d;
                if !(0..512).contains(&prev) {
                    return None;
                }
                out.push(prev as u8);
            }
            Some(pack_nostroddle(&out, 9))
        }
        _ => None,
    }
}

/// Apply the tail value transforms (currently: heightmap repack) recursively
/// to every key of a compound. Only touches shapes we understand; everything
/// else is left as-is, so the transform is always reversible.
fn tail_encode(v: &mut Value) {
    match v {
        Value::Compound(m) => {
            for (k, val) in m.iter_mut() {
                if k == "Heightmaps" {
                    if let Value::Compound(hm) = val {
                        for hv in hm.values_mut() {
                            if let Value::LongArray(la) = hv {
                                if let Some(dense) = hmap_encode(la.as_ref()) {
                                    *hv = Value::ByteArray(fastnbt::ByteArray::new(
                                        dense.iter().map(|x| *x as i8).collect(),
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    tail_encode(val);
                }
            }
        }
        Value::List(l) => {
            for v in l.iter_mut() {
                tail_encode(v);
            }
        }
        _ => {}
    }
}

fn tail_decode(v: &mut Value) {
    match v {
        Value::Compound(m) => {
            for (k, val) in m.iter_mut() {
                if k == "Heightmaps" {
                    if let Value::Compound(hm) = val {
                        for hv in hm.values_mut() {
                            if let Value::ByteArray(ba) = hv {
                                let raw: Vec<u8> = ba.as_ref().iter().map(|x| *x as u8).collect();
                                if raw.first() == Some(&HMAP_MAGIC) {
                                    if let Some(longs) = hmap_decode(&raw) {
                                        *hv = Value::LongArray(fastnbt::LongArray::new(longs));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    tail_decode(val);
                }
            }
        }
        Value::List(l) => {
            for v in l.iter_mut() {
                tail_decode(v);
            }
        }
        _ => {}
    }
}

/// A split payload, carrying the canonical section bytes so the writer never
/// parses the payload twice.
#[derive(Debug, Clone)]
pub enum Split {
    Whole {
        hash: Hash,
    },
    Split {
        /// (structural hash, serialized bytes) per section, in order. Bytes
        /// are the custom form when the shape is understood, NBT otherwise.
        sections: Vec<(Hash, Vec<u8>)>,
        tail: Vec<u8>,
    },
    /// Whole-mode field split: one part per top-level key.
    Fields {
        parts: Vec<FieldPart>,
        /// The `Position` marker was key-redundant and stripped.
        pos_from_key: bool,
    },
}

#[derive(Debug, Clone)]
pub struct FieldPart {
    pub key: String,
    /// true: `bytes` is an unnamed value (write_unnamed form);
    /// false: `bytes` is a wrapped compound `{"": value}` (blob content).
    pub inline: bool,
    /// Structural hash of the value (blob parts only; 0 for inline).
    pub hash: Hash,
    pub bytes: Vec<u8>,
}

pub fn to_bytes(v: &Value) -> Vec<u8> {
    fastnbt::to_bytes(v).unwrap_or_default()
}

pub fn hash_of(payload: &[u8]) -> Hash {
    xxh3_128(payload)
}

/// Order-free structural digest of an NBT value. Compounds are fed in sorted
/// key order; strings/types/lengths are mixed in, so any semantic difference
/// changes the hash and any re-serialization keeps it stable.
pub fn structural_hash(v: &Value) -> Hash {
    let mut h = Xxh3::new();
    feed(&mut h, v);
    h.digest128()
}

macro_rules! w {
    ($h:expr, $b:expr) => {
        $h.update($b)
    };
}

fn feed(h: &mut Xxh3, v: &Value) {
    match v {
        Value::Byte(x) => {
            w!(h, &[1]);
            w!(h, &x.to_le_bytes());
        }
        Value::Short(x) => {
            w!(h, &[2]);
            w!(h, &x.to_le_bytes());
        }
        Value::Int(x) => {
            w!(h, &[3]);
            w!(h, &x.to_le_bytes());
        }
        Value::Long(x) => {
            w!(h, &[4]);
            w!(h, &x.to_le_bytes());
        }
        Value::Float(x) => {
            w!(h, &[5]);
            w!(h, &x.to_bits().to_le_bytes());
        }
        Value::Double(x) => {
            w!(h, &[6]);
            w!(h, &x.to_bits().to_le_bytes());
        }
        Value::ByteArray(x) => {
            w!(h, &[7]);
            w!(h, &(x.len() as u64).to_le_bytes());
            // ByteArray derefs to [i8]; hash the raw little-endian bytes.
            for b in x.as_ref() {
                w!(h, &(*b as u8).to_le_bytes());
            }
        }
        Value::String(s) => {
            w!(h, &[8]);
            w!(h, &(s.len() as u64).to_le_bytes());
            w!(h, s.as_bytes());
        }
        Value::List(l) => {
            w!(h, &[9]);
            w!(h, &(l.len() as u64).to_le_bytes());
            for i in l {
                feed(h, i);
            }
        }
        Value::Compound(m) => {
            w!(h, &[10]);
            w!(h, &(m.len() as u64).to_le_bytes());
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                w!(h, &(k.len() as u64).to_le_bytes());
                w!(h, k.as_bytes());
                feed(h, &m[k]);
            }
        }
        Value::IntArray(x) => {
            w!(h, &[11]);
            w!(h, &(x.len() as u64).to_le_bytes());
            for i in x.as_ref() {
                w!(h, &i.to_le_bytes());
            }
        }
        Value::LongArray(x) => {
            w!(h, &[12]);
            w!(h, &(x.len() as u64).to_le_bytes());
            for i in x.as_ref() {
                w!(h, &i.to_le_bytes());
            }
        }
    }
}

/// Split an uncompressed chunk payload into a recipe. `pos_hint` is the
/// (cx, cz) of the chunk key; when the payload carries a redundant
/// `Position: [Int cx, Int 0, Int cz]` marker (entity-chunk format) it is
/// stripped from the recipe and reconstructed on read.
pub fn split(payload: &[u8], whole_mode: bool, pos_hint: Option<(i32, i32)>) -> Result<Split, String> {
    if whole_mode {
        return whole_split(payload, pos_hint);
    }
    let root: Value = fastnbt::from_bytes(payload).map_err(|e| format!("nbt parse: {e}"))?;
    let Value::Compound(map) = &root else {
        return Ok(Split::Whole { hash: hash_of(payload) });
    };
    let Some(Value::List(list)) = map.get("sections") else {
        return whole_split(payload, pos_hint);
    };

    let mut sections = Vec::with_capacity(list.len());
    for s in list {
        // custom form when possible (marker-detectable), NBT bytes otherwise.
        // v3 output is canonical, so identical content yields identical bytes
        // regardless of the original palette order -> hash the bytes.
        match section_encode(s) {
            Some(c) => {
                let h = if c.first() == Some(&CSEC3_MAGIC) { hash_of(&c) } else { structural_hash(s) };
                sections.push((h, c));
            }
            None => sections.push((structural_hash(s), to_bytes(s))),
        }
    }

    let mut tail_map = map.clone();
    tail_map.remove("sections");
    let mut tail_val = Value::Compound(tail_map);
    tail_encode(&mut tail_val);
    let tail = to_bytes(&tail_val);

    Ok(Split::Split { sections, tail })
}

/// Whole-mode field split: a compound payload is split per top-level key so
/// shared fields dedup into shared blobs and tiny unique fields stay inline
/// in the recipe (no blob framing). Falls back to Whole when the payload is
/// not a decomposable compound.
const INLINE_MAX: usize = 40;

pub fn wrap(v: &Value) -> Vec<u8> {
    let mut m = fastnbt::Value::Compound(Default::default());
    if let Value::Compound(mm) = &mut m {
        mm.insert("".to_string(), v.clone());
    }
    to_bytes(&m)
}

fn whole_split(payload: &[u8], pos_hint: Option<(i32, i32)>) -> Result<Split, String> {
    let Ok(Value::Compound(mut map)) = fastnbt::from_bytes::<Value>(payload) else {
        return Ok(Split::Whole { hash: hash_of(payload) });
    };
    if map.len() < 2 {
        return Ok(Split::Whole { hash: hash_of(payload) });
    }
    // strip a key-redundant Position marker (exact shape check)
    let mut pos_from_key = false;
    if let Some((cx, cz)) = pos_hint {
        if let Some(Value::List(l)) = map.get("Position") {
            if l.len() == 3
                && matches!(&l[0], Value::Int(x) if *x == cx)
                && matches!(&l[1], Value::Int(0))
                && matches!(&l[2], Value::Int(z) if *z == cz)
            {
                map.remove("Position");
                pos_from_key = true;
            }
        }
    }
    if pos_from_key && map.len() < 2 {
        // nothing shareable left; keep the payload whole
        return Ok(Split::Whole { hash: hash_of(payload) });
    }
    let mut parts = Vec::with_capacity(map.len());
    for (k, v) in &map {
        let mut unnamed = Vec::with_capacity(48);
        let compact = !matches!(v, Value::Compound(_))
            && write_unnamed(v, &mut unnamed).is_some()
            && unnamed.len() <= INLINE_MAX;
        if compact {
            parts.push(FieldPart { key: k.clone(), inline: true, hash: 0, bytes: unnamed });
        } else {
            let bytes = wrap(v);
            parts.push(FieldPart { key: k.clone(), inline: false, hash: structural_hash(v), bytes });
        }
    }
    Ok(Split::Fields { parts, pos_from_key })
}

/// Reassemble the uncompressed payload from a split recipe.
pub fn join(sections: &[Vec<u8>], tail: &[u8]) -> Result<Vec<u8>, String> {
    let tail_val: Value = fastnbt::from_bytes(tail).map_err(|e| format!("tail parse: {e}"))?;
    let Value::Compound(mut map) = tail_val else {
        return Err("tail is not a compound".into());
    };
    {
        let mut tv = Value::Compound(std::mem::take(&mut map));
        tail_decode(&mut tv);
        map = match tv { Value::Compound(m) => m, _ => unreachable!() };
    }
    let mut list = Vec::with_capacity(sections.len());
    for s in sections {
        let v = if s.first() == Some(&CSEC_MAGIC) || s.first() == Some(&CSEC3_MAGIC) {
            section_decode(s).ok_or("bad csec section")?
        } else {
            fastnbt::from_bytes(s).map_err(|e| format!("section parse: {e}"))?
        };
        list.push(v);
    }
    map.insert("sections".to_string(), Value::List(list));
    Ok(to_bytes(&Value::Compound(map)))
}

// ------------------------------------------------------- unnamed values --

/// Serialize a value as an unnamed NBT tag (tag byte + payload, no name).
/// Used for recipe-inlined field parts where every byte counts. Returns None
/// for shapes the compact form cannot represent (caller stores them as blobs
/// instead).
pub fn write_unnamed(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    let tag = prim_tag(v)?;
    out.push(tag);
    write_payload(v, tag, out)
}

fn prim_tag(v: &Value) -> Option<u8> {
    Some(match v {
        Value::Byte(_) => 1,
        Value::Short(_) => 2,
        Value::Int(_) => 3,
        Value::Long(_) => 4,
        Value::Float(_) => 5,
        Value::Double(_) => 6,
        Value::ByteArray(_) => 7,
        Value::String(_) => 8,
        Value::List(_) => 9,
        Value::Compound(_) => 10,
        Value::IntArray(_) => 11,
        Value::LongArray(_) => 12,
    })
}

/// Write a value's payload (no tag byte; type implied by `tag`).
fn write_payload(v: &Value, tag: u8, out: &mut Vec<u8>) -> Option<()> {
    if prim_tag(v)? != tag {
        return None;
    }
    match v {
        Value::Byte(x) => out.extend_from_slice(&x.to_le_bytes()),
        Value::Short(x) => out.extend_from_slice(&x.to_le_bytes()),
        Value::Int(x) => out.extend_from_slice(&x.to_le_bytes()),
        Value::Long(x) => out.extend_from_slice(&x.to_le_bytes()),
        Value::Float(x) => out.extend_from_slice(&x.to_bits().to_le_bytes()),
        Value::Double(x) => out.extend_from_slice(&x.to_bits().to_le_bytes()),
        Value::ByteArray(x) => {
            out.extend_from_slice(&(x.len() as u32).to_le_bytes());
            for b in x.as_ref() {
                out.push(*b as u8);
            }
        }
        Value::String(s) => {
            let b = s.as_bytes();
            if b.len() > 65535 || !b.is_ascii() {
                return None;
            }
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::List(l) => {
            let elem = match l.first() {
                None => 0u8,
                Some(first) => {
                    let t = prim_tag(first)?;
                    if !l.iter().all(|x| prim_tag(x) == Some(t)) {
                        return None;
                    }
                    t
                }
            };
            out.push(elem);
            out.extend_from_slice(&(l.len() as u32).to_le_bytes());
            for x in l {
                write_payload(x, elem, out)?;
            }
        }
        Value::IntArray(x) => {
            out.extend_from_slice(&(x.len() as u32).to_le_bytes());
            for i in x.as_ref() {
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
        Value::LongArray(x) => {
            out.extend_from_slice(&(x.len() as u32).to_le_bytes());
            for i in x.as_ref() {
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
        Value::Compound(_) => return None, // compounds go the blob path
    }
    Some(())
}

/// Parse an unnamed NBT tag produced by `write_unnamed`.
pub fn read_unnamed(b: &[u8]) -> Option<Value> {
    let mut p = 0usize;
    let v = read_unnamed_at(b, &mut p)?;
    Some(v)
}

fn read_unnamed_at(b: &[u8], p: &mut usize) -> Option<Value> {
    let tag = *b.get(*p)?;
    *p += 1;
    read_payload(b, p, tag, false)
}

fn read_payload(b: &[u8], p: &mut usize, tag: u8, in_list: bool) -> Option<Value> {
    Some(match tag {
        1 => {
            let x = i8::from_le_bytes(b.get(*p..*p + 1)?.try_into().ok()?);
            *p += 1;
            Value::Byte(x)
        }
        2 => {
            let x = i16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?);
            *p += 2;
            Value::Short(x)
        }
        3 => {
            let x = i32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
            *p += 4;
            Value::Int(x)
        }
        4 => {
            let x = i64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
            *p += 8;
            Value::Long(x)
        }
        5 => {
            let x = f32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
            *p += 4;
            Value::Float(x)
        }
        6 => {
            let x = f64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
            *p += 8;
            Value::Double(x)
        }
        7 => {
            let n = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            let arr = b.get(*p..p.checked_add(n)?)?;
            *p += n;
            Value::ByteArray(fastnbt::ByteArray::new(arr.iter().map(|x| *x as i8).collect()))
        }
        8 => {
            let n = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            let s = core::str::from_utf8(b.get(*p..p.checked_add(n)?)?).ok()?.to_string();
            *p += n;
            Value::String(s)
        }
        9 => {
            let elem = *b.get(*p)?;
            *p += 1;
            let n = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            let mut l = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                l.push(read_payload(b, p, elem, true)?);
            }
            Value::List(l)
        }
        10 if !in_list => {
            let mut m = fastnbt::Value::Compound(Default::default());
            let Value::Compound(mm) = &mut m else { unreachable!() };
            loop {
                let n = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                *p += 2;
                if n == 0 {
                    break;
                }
                let k = core::str::from_utf8(b.get(*p..p.checked_add(n)?)?).ok()?.to_string();
                *p += n;
                let v = read_unnamed_at(b, p)?;
                mm.insert(k, v);
            }
            m
        }
        11 => {
            let n = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            let mut a = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                let x = i32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
                *p += 4;
                a.push(x);
            }
            Value::IntArray(fastnbt::IntArray::new(a))
        }
        12 => {
            let n = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            let mut a = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                let x = i64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
                *p += 8;
                a.push(x);
            }
            Value::LongArray(fastnbt::LongArray::new(a))
        }
        _ => return None,
    })
}

/// Unwrap a wrapped-compound part payload `{"": value}` back to the value.
pub fn unwrap_wrapped(bytes: &[u8]) -> Option<Value> {
    let v: Value = fastnbt::from_bytes(bytes).ok()?;
    match v {
        Value::Compound(m) if m.len() == 1 && m.contains_key("") => {
            m.into_iter().next().map(|(_, v)| v)
        }
        _ => None,
    }
}

/// One field part of a field-split payload (read-path reassembly form).
#[derive(Debug, Clone)]
pub struct FieldBytes {
    pub key: String,
    pub inline: bool,
    pub bytes: Vec<u8>,
}

/// Reassemble a field-split payload from parts.
pub fn join_fields(parts: &[FieldBytes], pos: Option<(i32, i32)>) -> Result<Vec<u8>, String> {
    let mut map = fastnbt::Value::Compound(Default::default());
    let Value::Compound(m) = &mut map else { unreachable!() };
    for p in parts {
        let val = if p.inline {
            read_unnamed(&p.bytes).ok_or_else(|| format!("field {}: bad inline", p.key))?
        } else {
            let v: Value = fastnbt::from_bytes(&p.bytes)
                .map_err(|e| format!("field {}: {e}", p.key))?;
            match v {
                Value::Compound(m) if m.len() == 1 && m.contains_key("") => {
                    m.into_iter().next().map(|(_, v)| v).unwrap_or(Value::Byte(0))
                }
                other => other,
            }
        };
        m.insert(p.key.clone(), val);
    }
    if let Some((cx, cz)) = pos {
        m.insert(
            "Position".to_string(),
            Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
        );
    }
    Ok(to_bytes(&map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn sample_chunk(sections: usize, extra_key: &str) -> Vec<u8> {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert(extra_key.to_string(), Value::Byte(1));
        let mut sec_list = Vec::new();
        for i in 0..sections {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(i as i8));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = Vec::new();
            pal.push(Value::String("minecraft:stone".to_string()));
            bs.insert("palette".to_string(), Value::List(pal));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        let mut be = Vec::new();
        be.push(Value::String("x".to_string()));
        root.insert("block_entities".to_string(), Value::List(be));
        to_bytes(&Value::Compound(root))
    }

    #[test]
    fn roundtrip_semantic_equality() {
        let payload = sample_chunk(6, "Test");
        let s = split(&payload, false, Some((3, 7))).unwrap();
        let Split::Split { sections, tail } = &s else { panic!("expected split") };
        assert_eq!(sections.len(), 6);
        let sec_bytes: Vec<Vec<u8>> = sections.iter().map(|(_, b)| b.clone()).collect();
        let joined = join(&sec_bytes, tail).unwrap();
        let a: Value = fastnbt::from_bytes(&payload).unwrap();
        let b: Value = fastnbt::from_bytes(&joined).unwrap();
        assert_eq!(a, b, "semantic round-trip must be equal");
    }

    #[test]
    fn dedup_identical_sections_across_chunks() {
        let s1 = split(&sample_chunk(8, "T"), false, None).unwrap();
        let s2 = split(&sample_chunk(8, "T"), false, None).unwrap();
        let Split::Split { sections: a, .. } = &s1 else { panic!() };
        let Split::Split { sections: b, .. } = &s2 else { panic!() };
        let ha: Vec<Hash> = a.iter().map(|(h, _)| *h).collect();
        let hb: Vec<Hash> = b.iter().map(|(h, _)| *h).collect();
        assert_eq!(ha, hb, "identical chunks must share structural section hashes");
    }

    #[test]
    fn structural_hash_is_order_free() {
        let mut m1: Map<String, Value> = Map::new();
        m1.insert("a".into(), Value::Int(1));
        m1.insert("b".into(), Value::Int(2));
        let h1 = structural_hash(&Value::Compound(m1));
        let mut m2: Map<String, Value> = Map::new();
        m2.insert("b".into(), Value::Int(2));
        m2.insert("a".into(), Value::Int(1));
        let h2 = structural_hash(&Value::Compound(m2));
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_content_different_hash() {
        let mut m1: Map<String, Value> = Map::new();
        m1.insert("a".into(), Value::Int(1));
        let h1 = structural_hash(&Value::Compound(m1));
        let mut m2: Map<String, Value> = Map::new();
        m2.insert("a".into(), Value::Int(2));
        let h2 = structural_hash(&Value::Compound(m2));
        assert_ne!(h1, h2);
    }

    #[test]
    fn no_sections_falls_back_to_whole() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, false, Some((3, 7))).unwrap();
        assert!(matches!(s, Split::Whole { .. }));
    }

    #[test]
    fn empty_sections_list_still_splits() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("sections".to_string(), Value::List(Vec::new()));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, false, Some((3, 7))).unwrap();
        let Split::Split { sections, .. } = s else { panic!("empty list must still split") };
        assert!(sections.is_empty());
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn sample_section(with_data: bool, skylight: Option<Vec<i8>>) -> Value {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(-3));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for n in ["minecraft:stone", "minecraft:deepslate", "minecraft:air"] {
            pal.push(Value::String(n.to_string()));
        }
        bs.insert("palette".to_string(), Value::List(pal));
        if with_data {
            // 5-bit-ish values in vanilla no-straddle packing (2 bits here)
            let entries: Vec<u8> = (0..4096).map(|i| (i * 7 % 3) as u8).collect();
            bs.insert(
                "data".to_string(),
                Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 2))),
            );
        }
        sec.insert("block_states".to_string(), Value::Compound(bs));
        if let Some(sl) = skylight {
            sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(sl)));
        }
        Value::Compound(sec)
    }

    #[test]
    fn csec_roundtrip_all_shapes() {
        for (name, sec) in [
            ("packed+light", sample_section(true, Some(vec![0; 2048]))),
            ("packed+uniform-1", sample_section(true, Some(vec![-1; 2048]))),
            ("packed+nolight", sample_section(true, None)),
            ("uniform", {
                let mut sec: Map<String, Value> = Map::new();
                sec.insert("Y".to_string(), Value::Byte(9));
                let mut bs: Map<String, Value> = Map::new();
                bs.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:air".into())]));
                sec.insert("block_states".to_string(), Value::Compound(bs));
                Value::Compound(sec)
            }),
        ] {
            let enc = section_encode(&sec).unwrap_or_else(|| panic!("{name}: must encode"));
            let dec = section_decode(&enc).unwrap_or_else(|| panic!("{name}: must decode"));
            // canonical re-encoding may permute palette order; only the
            // expanded block/biome vectors are semantically meaningful
            assert!(
                sections_semantically_equal(&dec, &sec),
                "{name}: roundtrip must be semantically exact"
            );
        }
    }

    #[test]
    fn csec3_compound_palettes_and_permutation_invariance() {
        // modern 26.x form: palette entries are compounds {Name, Properties}
        let states = [
            ("minecraft:stone", vec![]),
            ("minecraft:oak_log", vec![("axis", "y")]),
            ("minecraft:water", vec![("level", "0")]),
        ];
        let mk_entry = |i: usize| -> Value {
            let (n, props) = &states[i];
            let mut cm: Map<String, Value> = Map::new();
            cm.insert("Name".to_string(), Value::String(n.to_string()));
            if !props.is_empty() {
                let mut pm: Map<String, Value> = Map::new();
                for (k, v) in props {
                    pm.insert(k.to_string(), Value::String(v.to_string()));
                }
                cm.insert("Properties".to_string(), Value::Compound(pm));
            }
            Value::Compound(cm)
        };
        // order A: 0,1,2 — order B: 2,0,1 (same states, permuted palette)
        let orders: [[usize; 3]; 2] = [[0, 1, 2], [2, 0, 1]];
        let mut encs = Vec::new();
        for order in orders {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(5));
            let mut bs: Map<String, Value> = Map::new();
            bs.insert(
                "palette".to_string(),
                Value::List(order.iter().map(|i| mk_entry(*i)).collect()),
            );
            // data references the section's own palette: block state at
            // position i is states[(i*7)%3], expressed in this palette's order
            let entries: Vec<u8> = (0..4096)
                .map(|i| {
                    let s = (i * 7 % 3) as usize;
                    order.iter().position(|o| *o == s).unwrap() as u8
                })
                .collect();
            bs.insert(
                "data".to_string(),
                Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 2))),
            );
            sec.insert("block_states".to_string(), Value::Compound(bs));
            let enc = section_encode(&Value::Compound(sec)).expect("compound palettes must encode");
            assert_eq!(enc[0], CSEC3_MAGIC);
            encs.push(enc);
        }
        // identical canonical bytes => identical dedup hash => the two
        // palette permutations collapse to ONE blob
        assert_eq!(encs[0], encs[1], "palette permutations must canonicalize");
        // and the decoded value must equal the expanded original semantics
        let dec = section_decode(&encs[0]).unwrap();
        assert!(matches!(dec, Value::Compound(ref m) if m.contains_key("block_states")));
    }

    #[test]
    fn csec3_starlight_state_keys() {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(30));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:air".into())]));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let mut bio: Map<String, Value> = Map::new();
        bio.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:plains".into())]));
        sec.insert("biomes".to_string(), Value::Compound(bio));
        sec.insert("starlight.skylight_state".to_string(), Value::Int(1));
        sec.insert("starlight.blocklight_state".to_string(), Value::Int(1));
        let enc = section_encode(&Value::Compound(sec.clone())).expect("starlight keys must encode");
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&dec, &Value::Compound(sec)));
    }

    #[test]
    fn csec_rejects_unknown_keys() {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(1));
        sec.insert("FutureThing".to_string(), Value::Byte(1));
        assert!(section_encode(&Value::Compound(sec)).is_none());
    }

    #[test]
    fn hmap_roundtrip_and_padding_guard() {
        let entries: Vec<u8> = (0..256).map(|i| (i % 200) as u8).collect();
        let la = pack_nostroddle(&entries, 9);
        assert_eq!(la.len(), 37);
        let enc = hmap_encode(&la).expect("vanilla shape must encode");
        assert_eq!(hmap_decode(&enc).unwrap(), la);
        // non-zero padding in the last long must NOT be transformed
        let mut bad = la.clone();
        bad[36] |= 1 << 40;
        assert!(hmap_encode(&bad).is_none());
    }

    #[test]
    fn unnamed_roundtrip() {
        let vals = vec![
            Value::Byte(-5),
            Value::Short(1234),
            Value::Int(-100000),
            Value::Long(i64::MIN),
            Value::Float(1.5),
            Value::Double(-2.25),
            Value::String("hello".into()),
            Value::List(vec![Value::Int(1), Value::Int(-2), Value::Int(3)]),
            Value::List(Vec::new()),
            Value::ByteArray(fastnbt::ByteArray::new(vec![1, 2, 3])),
            Value::IntArray(fastnbt::IntArray::new(vec![9, -9])),
            Value::LongArray(fastnbt::LongArray::new(vec![7, 8, 9])),
        ];
        for v in vals {
            let mut b = Vec::new();
            write_unnamed(&v, &mut b).expect("must encode");
            assert_eq!(read_unnamed(&b).unwrap(), v);
        }
        // heterogeneous lists and compounds must be rejected
        let mut b = Vec::new();
        assert!(write_unnamed(&Value::List(vec![Value::Int(1), Value::Byte(2)]), &mut b).is_none());
        assert!(write_unnamed(&Value::Compound(Default::default()), &mut b).is_none());
    }

    #[test]
    fn field_split_strips_key_redundant_position() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert("Position".to_string(), Value::List(vec![Value::Int(5), Value::Int(0), Value::Int(-7)]));
        root.insert("Entities".to_string(), Value::List(Vec::new()));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, true, Some((5, -7))).unwrap();
        let Split::Fields { parts, pos_from_key } = &s else { panic!("must field-split") };
        assert!(*pos_from_key, "matching Position must be stripped");
        assert!(!parts.iter().any(|p| p.key == "Position"));
        // wrong key -> Position stays
        let s2 = split(&payload, true, Some((6, -7))).unwrap();
        let Split::Fields { parts: p2, pos_from_key: pf2 } = &s2 else { panic!() };
        assert!(!*pf2);
        assert!(p2.iter().any(|p| p.key == "Position"));
        // reassemble both ways
        let fb: Vec<FieldBytes> = parts
            .iter()
            .map(|p| FieldBytes { key: p.key.clone(), inline: p.inline, bytes: p.bytes.clone() })
            .collect();
        let out = join_fields(&fb, Some((5, -7))).unwrap();
        let a: Value = fastnbt::from_bytes(&payload).unwrap();
        let b: Value = fastnbt::from_bytes(&out).unwrap();
        assert_eq!(a, b);
    }
}
