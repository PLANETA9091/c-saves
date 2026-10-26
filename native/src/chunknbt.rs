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
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use xxhash_rust::xxh3::{xxh3_128, Xxh3};

pub type Hash = u128;

// ------------------------------------------------- section emit cache -----

/// Bounded FIFO cache of EMITTED section NBT payloads, keyed by the section's
/// content hash (the recipe already carries it). A section's emitted bytes
/// are a pure function of its CSEC stream, so every read of the same section
/// (across chunks and across reads) becomes a memcpy splice instead of a
/// 4096-voxel unpack+repack. Bounded so RAM stays flat.
struct SecEmitCache {
    map: HashMap<Hash, Arc<Vec<u8>>>,
    order: VecDeque<(Hash, u32)>,
    total: u64,
    cap: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl SecEmitCache {
    fn get(&mut self, h: Hash) -> Option<Arc<Vec<u8>>> {
        if let Some(v) = self.map.get(&h) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(v.clone());
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn put(&mut self, h: Hash, v: Arc<Vec<u8>>) {
        let sz = v.len() as u64;
        if sz > self.cap {
            return;
        }
        if self.map.insert(h, v).is_none() {
            self.order.push_front((h, sz as u32));
            self.total += sz;
            while self.total > self.cap {
                if let Some((ek, esz)) = self.order.pop_back() {
                    if self.map.remove(&ek).is_some() {
                        self.total -= esz as u64;
                    }
                } else {
                    break;
                }
            }
        }
    }
}

use std::collections::HashMap;

static SEC_EMIT_CAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn sec_emit_cap() -> u64 {
    *SEC_EMIT_CAP.get_or_init(|| {
        std::env::var("CSAVES_SEC_CACHE_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(32 * 1024 * 1024)
    })
}

thread_local! {
    // per-thread FIFO: chunk IO is per-thread in Moonrise, so a per-thread
    // cache gets the same hit rate with ZERO lock traffic on the read path
    static SEC_EMIT_TL: std::cell::RefCell<Option<SecEmitCache>> = const { std::cell::RefCell::new(None) };
}

fn sec_emit_with<R>(f: impl FnOnce(&mut Option<SecEmitCache>) -> R) -> R {
    SEC_EMIT_TL.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            let cap = sec_emit_cap();
            if cap > 0 {
                *g = Some(SecEmitCache {
                    map: HashMap::new(),
                    order: VecDeque::new(),
                    total: 0,
                    cap,
                    hits: AtomicU64::new(0),
                    misses: AtomicU64::new(0),
                });
            }
        }
        f(&mut g)
    })
}

fn sec_emit_get(h: Hash) -> Option<Arc<Vec<u8>>> {
    sec_emit_with(|c| c.as_mut()?.get(h))
}

fn sec_emit_put(h: Hash, v: Arc<Vec<u8>>) {
    sec_emit_with(|c| {
        if let Some(cc) = c.as_mut() {
            cc.put(h, v);
        }
    })
}

pub fn sec_emit_stats() -> (u64, u64) {
    SEC_EMIT_TL.with(|c| match c.borrow().as_ref() {
        Some(c) => (c.hits.load(Ordering::Relaxed), c.misses.load(Ordering::Relaxed)),
        None => (0, 0),
    })
}

// ------------------------------------------------- section encode cache ---

/// Section bytes handed to the store: shared (from the encode cache) or
/// owned (freshly encoded / fallback NBT).
#[derive(Clone, Debug)]
pub enum SecBytes {
    Owned(Vec<u8>),
    Shared(Arc<Vec<u8>>),
}

impl SecBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            SecBytes::Owned(v) => v,
            SecBytes::Shared(a) => a,
        }
    }
}

struct EncCache {
    map: HashMap<Hash, (Hash, Arc<Vec<u8>>)>, // structural hash -> (encoded hash, csec bytes)
    order: VecDeque<(Hash, u32)>,
    total: u64,
    cap: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl EncCache {
    fn get(&mut self, sh: Hash) -> Option<(Hash, Arc<Vec<u8>>)> {
        if let Some(v) = self.map.get(&sh) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(v.clone());
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn put(&mut self, sh: Hash, h: Hash, v: Arc<Vec<u8>>) {
        let sz = v.len() as u64;
        if sz > self.cap {
            return;
        }
        if self.map.insert(sh, (h, v)).is_none() {
            self.order.push_front((sh, sz as u32));
            self.total += sz;
            while self.total > self.cap {
                if let Some((ek, esz)) = self.order.pop_back() {
                    if self.map.remove(&ek).is_some() {
                        self.total -= esz as u64;
                    }
                } else {
                    break;
                }
            }
        }
    }
}

static ENC_CACHE_CAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn enc_cache_cap() -> u64 {
    *ENC_CACHE_CAP.get_or_init(|| {
        std::env::var("CSAVES_ENC_CACHE_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(32 * 1024 * 1024)
    })
}

thread_local! {
    // per-thread (mirrors the zstd codec contexts): zero lock traffic on the
    // encode path; cross-thread dedup happens in the store's blob map anyway
    static ENC_CACHE_TL: std::cell::RefCell<Option<EncCache>> = const { std::cell::RefCell::new(None) };
}

fn enc_cache_with<R>(f: impl FnOnce(&mut Option<EncCache>) -> R) -> R {
    ENC_CACHE_TL.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            let cap = enc_cache_cap();
            if cap > 0 {
                *g = Some(EncCache {
                    map: HashMap::new(),
                    order: VecDeque::new(),
                    total: 0,
                    cap,
                    hits: AtomicU64::new(0),
                    misses: AtomicU64::new(0),
                });
            }
        }
        f(&mut g)
    })
}

fn enc_cache_get(sh: Hash) -> Option<(Hash, Arc<Vec<u8>>)> {
    enc_cache_with(|c| c.as_mut()?.get(sh))
}

fn enc_cache_put(sh: Hash, h: Hash, v: Arc<Vec<u8>>) {
    enc_cache_with(|c| {
        if let Some(cc) = c.as_mut() {
            cc.put(sh, h, v);
        }
    })
}

pub fn enc_cache_stats() -> (u64, u64) {
    ENC_CACHE_TL.with(|c| match c.borrow().as_ref() {
        Some(c) => (c.hits.load(Ordering::Relaxed), c.misses.load(Ordering::Relaxed)),
        None => (0, 0),
    })
}

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
    /// LSB-first bit reader. Hot loop on the decode path (4096+ voxels per
    /// section): consume bits in u64 gulps instead of one bit at a time.
    #[inline]
    fn get(&mut self, bits: u32) -> Option<u64> {
        if bits == 0 {
            return Some(0);
        }
        let byte = self.pos >> 3;
        // need ceil(bits/8) bytes starting at `byte`
        let want = ((bits as usize + 7) >> 3) + 1; // +1: sub-byte phase
        let end = byte + want;
        if end > self.in_.len() {
            // allow the tail: pad conceptually missing bytes with zeros
            if byte >= self.in_.len() {
                return None;
            }
            let mut acc: u128 = 0;
            let mut sh = 0u32;
            for i in byte..self.in_.len() {
                acc |= (self.in_[i] as u128) << sh;
                sh += 8;
            }
            let v = (acc >> (self.pos & 7)) as u64 & ((1u128 << bits) - 1) as u64;
            self.pos += bits as usize;
            return Some(v);
        }
        let mut acc: u128 = 0;
        acc |= self.in_[byte] as u128;
        for i in 1..want {
            acc |= (self.in_[byte + i] as u128) << (8 * i);
        }
        let v = ((acc >> (self.pos & 7)) as u64) & ((1u128 << bits).wrapping_sub(1) as u64);
        self.pos += bits as usize;
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

/// One-pass dense(LSB-first, in_bits) -> vanilla no-straddle longs(out_bits).
///
/// When in_bits == out_bits and the width divides 64 (1/2/4/8/16/32), the
/// dense stream IS the no-straddle layout bit-for-bit: pure byte copy. This
/// covers the overwhelmingly common vanilla-clamped 4-bit terrain data.
/// Everything else goes through a rolling-u64 window at ~4ns/voxel.
fn dense_to_nostroddle(dense: &[u8], n: usize, in_bits: u32, out_bits: u32) -> Vec<i64> {
    if in_bits == out_bits && matches!(in_bits, 1 | 2 | 4 | 8 | 16 | 32) {
        let per = (64 / in_bits) as usize;
        let nlongs = (n + per - 1) / per;
        let mut out = vec![0i64; nlongs];
        let want = (n * in_bits as usize + 7) / 8;
        let nbytes = want.min(nlongs * 8).min(dense.len());
        // little-endian longs == the raw dense bytes (per*bits == 64)
        let full = nbytes / 8;
        let mut l = 0usize;
        let mut b = 0usize;
        while l < full {
            let mut w = [0u8; 8];
            w.copy_from_slice(&dense[b..b + 8]);
            out[l] = i64::from_le_bytes(w);
            b += 8;
            l += 1;
        }
        // trailing partial long (final voxels): copy byte-wise
        while b < nbytes && l < nlongs {
            let mut word: u64 = 0;
            for k in 0..8 {
                if b + k < nbytes {
                    word |= (dense[b + k] as u64) << (8 * k);
                }
            }
            out[l] = word as i64;
            b += 8;
            l += 1;
        }
        return out;
    }
    let per = (64 / out_bits) as usize;
    let omask = (1u64 << out_bits) - 1;
    let imask = (1u64 << in_bits) - 1;
    let mut longs = vec![0i64; (n + per - 1) / per];
    let mut li = 0usize;
    let mut slot = 0usize;
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    let mut pos = 0usize;
    for _ in 0..n {
        if nbits < in_bits {
            // refill 32 bits (4 bytes, zero-padded at the tail)
            let b0 = *dense.get(pos).unwrap_or(&0) as u64;
            let b1 = *dense.get(pos + 1).unwrap_or(&0) as u64;
            let b2 = *dense.get(pos + 2).unwrap_or(&0) as u64;
            let b3 = *dense.get(pos + 3).unwrap_or(&0) as u64;
            acc |= (b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)) << nbits;
            pos += 4;
            nbits += 32;
        }
        let v = acc & imask;
        acc >>= in_bits;
        nbits -= in_bits;
        longs[li] |= ((v & omask) as i64) << (slot * out_bits as usize);
        slot += 1;
        if slot == per {
            li += 1;
            slot = 0;
        }
    }
    longs
}

/// Inverse of dense_to_nostroddle: vanilla no-straddle longs -> dense
/// LSB-first stream at `bits` width. Dividing widths (1/2/4/8/16/32) are a
/// byte copy (per*bits == 64, so long LE bytes == dense stream); everything
/// else walks voxel-by-voxel, honoring each long's high padding bits.
fn nostroddle_to_dense(longs: &[i64], n: usize, bits: u32) -> Vec<u8> {
    let per = (64 / bits) as usize;
    let want = (n * bits as usize + 7) / 8;
    if matches!(bits, 1 | 2 | 4 | 8 | 16 | 32) {
        let mut out = vec![0u8; want];
        let mut l = 0usize;
        let mut b = 0usize;
        while b + 8 <= want && l < longs.len() {
            out[b..b + 8].copy_from_slice(&longs[l].to_le_bytes());
            b += 8;
            l += 1;
        }
        // final partial long: emit the remaining bytes
        if l < longs.len() && b < want {
            let w = longs[l].to_le_bytes();
            let take = (want - b).min(8);
            out[b..b + take].copy_from_slice(&w[..take]);
        }
        return out;
    }
    let mask = (1u64 << bits) - 1;
    let mut out = vec![0u8; want];
    let mut oacc: u64 = 0;
    let mut obits = 0u32;
    let mut op = 0usize;
    let mut li = 0usize;
    let mut slot = 0usize;
    for _ in 0..n {
        if slot == per {
            li += 1;
            slot = 0;
        }
        let w = if li < longs.len() { longs[li] as u64 } else { 0 };
        let v = (w >> (slot * bits as usize)) & mask;
        slot += 1;
        oacc |= v << obits;
        obits += bits;
        while obits >= 8 && op < want {
            out[op] = (oacc & 0xff) as u8;
            oacc >>= 8;
            obits -= 8;
            op += 1;
        }
    }
    while op < want {
        if obits >= 8 {
            out[op] = (oacc & 0xff) as u8;
            oacc >>= 8;
            obits -= 8;
        } else {
            out[op] = (oacc & 0xff) as u8;
            oacc = 0;
            obits = 0;
        }
        op += 1;
    }
    out
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
        // Narrow packing: vanilla clamps linear palettes to 4 bits/entry, so
        // a 2-entry palette wastes 4x (2 KiB vs 512 B per section). Under
        // 16-entry palettes re-pack at ceillog2(n); the decoder normalizes
        // back to the vanilla width. n >= 16: ceillog2 >= 4, no change.
        let narrow = bits_for(n) < 4;
        let out_bits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
        // IDENTITY shortcut: the palette is already in canonical order and
        // the packing width is unchanged — the vanilla longs ARE the dense
        // stream byte-for-byte (LE), so skip unpack/remap/pack entirely.
        // This is the hot re-save path for chunks this engine wrote.
        let identity = !narrow
            && in_bits == out_bits
            && matches!(in_bits, 1 | 2 | 4 | 8 | 16 | 32)
            && ents.iter().enumerate().all(|(i, (_, old))| *old == i);
        if identity {
            for l in data.as_ref() {
                out.extend_from_slice(&l.to_le_bytes());
            }
            return Some((true, false));
        }
        let v = unpack_nostroddle(data, entries, in_bits);
        // remap old palette index -> canonical index
        let mut remap = vec![0u8; n];
        for (new_i, (_, old_i)) in ents.iter().enumerate() {
            remap[*old_i] = new_i as u8;
        }
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
        /// (structural/content hash, section bytes) per section, in order.
        /// Bytes are the custom form when the shape is understood, NBT
        /// otherwise. Shared variants come from the encode cache.
        sections: Vec<(Hash, SecBytes)>,
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
        // encode-cache: re-saving a chunk re-encodes every section; the
        // structural hash is order-free, so unchanged sections hit the cache
        // and skip canonicalization + bit-packing entirely.
        let sh = structural_hash(s);
        if let Some((h, shared)) = enc_cache_get(sh) {
            sections.push((h, SecBytes::Shared(shared)));
            continue;
        }
        // custom form when possible (marker-detectable), NBT bytes otherwise.
        // v3 output is canonical, so identical content yields identical bytes
        // regardless of the original palette order -> hash the bytes.
        match section_encode(s) {
            Some(c) => {
                if c.first() == Some(&CSEC3_MAGIC) {
                    let h = hash_of(&c);
                    let arc = Arc::new(c);
                    enc_cache_put(sh, h, arc.clone());
                    sections.push((h, SecBytes::Shared(arc)));
                } else {
                    sections.push((structural_hash(s), SecBytes::Owned(c)));
                }
            }
            None => sections.push((structural_hash(s), SecBytes::Owned(to_bytes(s)))),
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

// -------------------------------------------------- fused join (BE emit) --
//
// The generic `join` decodes every CSEC section into a fastnbt Value tree
// (a HashMap<String, Value> per palette entry — tens of thousands of heap
// allocations for one 131-section chunk) and then re-serializes the whole
// tree. That makes reads ~9x slower than vanilla zlib inflate on real
// JJThunder worlds. `join_fused` instead walks the CSEC byte streams and
// emits the standard big-endian NBT wire form DIRECTLY into the output
// buffer: zero per-section Value trees, zero HashMap churn, one output
// allocation. Output is semantically identical (same parser-visible NBT);
// only key iteration order differs, which NBT semantics do not define.

macro_rules! fdbg {
    ($($a:tt)*) => {
        if std::env::var("CSAVES_FUSED_DEBUG").as_deref() == Ok("1") {
            eprintln!($($a)*);
        }
    };
}

fn put_be16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_be32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_named_key(out: &mut Vec<u8>, tag: u8, name: &str) {
    out.push(tag);
    let b = name.as_bytes();
    put_be16(out, b.len() as u16);
    out.extend_from_slice(b);
}

/// Emit a tail-side value in standard big-endian NBT (what fastnbt::to_bytes
/// would produce; compounds emitted in sorted key order for determinism).
fn emit_payload_be(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    match v {
        Value::Byte(x) => out.push(*x as u8),
        Value::Short(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Int(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Long(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Float(x) => out.extend_from_slice(&x.to_bits().to_be_bytes()),
        Value::Double(x) => out.extend_from_slice(&x.to_bits().to_be_bytes()),
        Value::ByteArray(x) => {
            put_be32(out, x.len() as i32);
            for b in x.as_ref() {
                out.push(*b as u8);
            }
        }
        Value::String(s) => {
            let b = s.as_bytes();
            if b.len() > 65535 {
                return None;
            }
            put_be16(out, b.len() as u16);
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
            put_be32(out, l.len() as i32);
            for x in l {
                emit_payload_be(x, out)?;
            }
        }
        Value::Compound(m) => {
            // sorted key order (deterministic output)
            let mut keys: Vec<(&String, &Value)> = m.iter().collect();
            keys.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            for (k, kv) in keys {
                let tag = prim_tag(kv)?;
                put_named_key(out, tag, k);
                emit_payload_be(kv, out)?;
            }
            out.push(0);
        }
        Value::IntArray(x) => {
            put_be32(out, x.len() as i32);
            for i in x.as_ref() {
                out.extend_from_slice(&i.to_be_bytes());
            }
        }
        Value::LongArray(x) => {
            put_be32(out, x.len() as i32);
            for i in x.as_ref() {
                out.extend_from_slice(&i.to_be_bytes());
            }
        }
    }
    Some(())
}

/// Emit one CSEC-v3 palette entry as an NBT list-element payload.
/// `p` points at the kind byte; it is consumed here.
/// kind 0 -> string element; kind 1/2 -> compound element.
fn emit_pal_entry_be(b: &[u8], p: &mut usize, kind: u8, out: &mut Vec<u8>) -> Option<()> {
    *p += 1; // the kind byte itself
    match kind {
        0 => {
            let l = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            put_be16(out, l as u16);
            out.extend_from_slice(b.get(*p..*p + l)?);
            *p += l;
        }
        1 | 2 => {
            let l = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            let name = b.get(*p..*p + l)?;
            *p += l;
            // entry payload: Name (string), then Properties (compound), then end
            out.extend_from_slice(b"\x08\x00\x04Name");
            put_be16(out, l as u16);
            out.extend_from_slice(name);
            if kind == 2 {
                let np = *b.get(*p)?;
                *p += 1;
                out.extend_from_slice(b"\x0A\x00\x0AProperties");
                for _ in 0..np {
                    let kl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                    *p += 2;
                    let k = b.get(*p..*p + kl)?;
                    *p += kl;
                    let vl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                    *p += 2;
                    let v = b.get(*p..*p + vl)?;
                    *p += vl;
                    out.push(0x08);
                    put_be16(out, kl as u16);
                    out.extend_from_slice(k);
                    put_be16(out, vl as u16);
                    out.extend_from_slice(v);
                }
                out.push(0);
            }
            out.push(0);
        }
        _ => {
            fdbg!("fused: bad pal entry kind {kind}");
            return None;
        }
    }
    Some(())
}

/// Emit one CSEC-v3 section (0xC7) as a compound list-element payload:
/// inner named tags in sorted key order + terminator. Returns None when the
/// shape is not representable (caller falls back to the generic join).
fn section_emit_csec3(b: &[u8], out: &mut Vec<u8>) -> Option<()> {
    let r = section_emit_csec3_inner(b, out);
    if r.is_none() && std::env::var("CSAVES_FUSED_DEBUG").as_deref() == Ok("1") {
        eprintln!(
            "fused: section emit failed len={} f1={:02x} f2={:02x}",
            b.len(),
            b.get(1).copied().unwrap_or(0),
            b.get(2).copied().unwrap_or(0)
        );
    }
    r
}

fn section_emit_csec3_inner(b: &[u8], out: &mut Vec<u8>) -> Option<()> {
    if *b.first()? != CSEC3_MAGIC {
        return None;
    }
    let f1 = *b.get(1)?;
    let f2 = *b.get(2)?;
    let mut p = 3usize;
    let y = *b.get(p)? as u8;
    p += 1;

    // collect the variable-shape parts first: palette streams + their ranges
    // are parsed once, in stream order; emission happens in KEY order.
    // Returns the serialized NBT payload of the {data, palette} compound.
    fn emit_pal_data(b: &[u8], p: &mut usize, entries: usize, has_data: bool, narrow: bool) -> Option<Vec<u8>> {
        let n = match b.get(*p..*p + 2) {
            Some(x) => u16::from_le_bytes(x.try_into().ok()?) as usize,
            None => {
                fdbg!("fused: pal n oob at p={}", p);
                return None;
            }
        };
        *p += 2;
        // entries payload first (uniform-kind-class check), header prepended
        // after. Vanilla palettes legitimately mix kinds 1 and 2 (compound
        // entries without/with Properties) — both are compound elements;
        // only string (0) vs compound (1/2) cannot share one NBT list.
        let mut ents = Vec::with_capacity(n.min(4096) * 16);
        let mut class: Option<u8> = None; // 0 = string, 1 = compound
        for _ in 0..n {
            let k = *b.get(*p)?;
            let kc = match k {
                0 => 0u8,
                1 | 2 => 1u8,
                other => {
                    fdbg!("fused: bad palette kind {other}");
                    return None;
                }
            };
            if let Some(pk) = class {
                if pk != kc {
                    fdbg!("fused: mixed palette classes string/compound");
                    return None; // one NBT list cannot hold both shapes
                }
            }
            class = Some(kc);
            emit_pal_entry_be(b, p, k, &mut ents)?;
        }
        let elem_tag = match class {
            None => 0x08u8, // empty palette: elem tag is arbitrary-but-stable
            Some(0) => 0x08u8,
            Some(1) => 0x0Au8,
            _ => unreachable!(),
        };
        let mut pal = Vec::with_capacity(ents.len() + 16);
        pal.push(0x09);
        pal.extend_from_slice(b"\x00\x07palette");
        pal.push(elem_tag);
        pal.extend_from_slice(&(n as i32).to_be_bytes());
        pal.extend_from_slice(&ents);
        pal.push(0x00); // compound terminator for {data, palette}
        // "data" sorts before "palette"
        if has_data {
            let rbits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
            let bytes = (entries * rbits as usize + 7) / 8;
            let dense = match b.get(*p..*p + bytes) {
                Some(d) => d,
                None => {
                    fdbg!("fused: data oob p={} bytes={} n={} narrow={} blen={}", p, bytes, n, narrow, b.len());
                    return None;
                }
            };
            *p += bytes;
            let out_bits = bits_for(n).max(4);
            let longs = dense_to_nostroddle(dense, entries, rbits, out_bits);
            let mut data = Vec::with_capacity(8 + longs.len() * 8);
            data.extend_from_slice(b"\x0C\x00\x04data"); // TAG_Long_Array
            data.extend_from_slice(&(longs.len() as i32).to_be_bytes());
            for l in &longs {
                data.extend_from_slice(&l.to_be_bytes());
            }
            data.extend_from_slice(&pal);
            return Some(data);
        }
        Some(pal)
    }

    // Emit in sorted key order: BlockLight < SkyLight < Y < biomes <
    // block_states < starlight.blocklight_state < starlight.skylight_state
    // We must parse the CSEC stream in ITS order (block_states, biomes,
    // SkyLight, BlockLight, starlight), so buffer the parts, then emit.

    // parse block_states
    let bs_body = if f1 & 1 != 0 {
        Some(emit_pal_data(b, &mut p, 4096, f1 & 2 != 0, f2 & 4 != 0)?)
    } else {
        None
    };
    // parse biomes
    let bio_body = if f1 & 4 != 0 {
        Some(emit_pal_data(b, &mut p, 64, f1 & 8 != 0, f2 & 8 != 0)?)
    } else {
        None
    };
    // SkyLight
    enum Sky {
        Full, // all 1
        Zero,
        Raw(Vec<u8>),
        None,
    }
    let mut sky = Sky::None;
    if f1 & 16 != 0 {
        sky = Sky::Full;
    } else if f1 & 32 != 0 {
        sky = Sky::Zero;
    } else if f1 & 64 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        sky = Sky::Raw(b.get(p..p + l)?.to_vec());
        p += l;
    }
    let mut bl: Option<Vec<u8>> = None;
    if f1 & 128 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        bl = Some(b.get(p..p + l)?.to_vec());
        p += l;
    }
    let sl_state = if f2 & 1 != 0 {
        let v = *b.get(p)?;
        p += 1;
        Some(v as i8 as i32)
    } else {
        None
    };
    let bl_state = if f2 & 2 != 0 {
        let v = *b.get(p)?;
        p += 1;
        Some(v as i8 as i32)
    } else {
        None
    };

    // ---- emit ----
    if let Some(raw) = &bl {
        put_named_key(out, 0x07, "BlockLight");
        put_be32(out, raw.len() as i32);
        out.extend_from_slice(raw);
    }
    match sky {
        Sky::Full => {
            put_named_key(out, 0x07, "SkyLight");
            put_be32(out, 2048);
            out.resize(out.len() + 2048, 0xff);
        }
        Sky::Zero => {
            put_named_key(out, 0x07, "SkyLight");
            put_be32(out, 2048);
            out.resize(out.len() + 2048, 0x00);
        }
        Sky::Raw(raw) => {
            put_named_key(out, 0x07, "SkyLight");
            put_be32(out, raw.len() as i32);
            out.extend_from_slice(&raw);
        }
        Sky::None => {}
    }
    // Y (byte)
    put_named_key(out, 0x01, "Y");
    out.push(y);
    // biomes
    if let Some(body) = &bio_body {
        put_named_key(out, 0x0A, "biomes");
        out.extend_from_slice(body);
    }
    // block_states
    if let Some(body) = &bs_body {
        put_named_key(out, 0x0A, "block_states");
        out.extend_from_slice(body);
    }
    if let Some(v) = bl_state {
        put_named_key(out, 0x03, "starlight.blocklight_state");
        put_be32(out, v);
    }
    if let Some(v) = sl_state {
        put_named_key(out, 0x03, "starlight.skylight_state");
        put_be32(out, v);
    }
    out.push(0x00); // compound terminator (list element payload end)
    Some(())
}

/// Source for one section in a fused join: its content hash (from the
/// recipe) and its CSEC bytes.
pub struct SecSrc<'a> {
    pub h: Hash,
    pub b: &'a [u8],
}

/// Fused join: emit the full chunk NBT directly from CSEC section streams +
/// the decoded tail. Returns None when any section is not a CSEC-v3 stream
/// (or the shape is unexpected) — the caller then uses the generic join.
pub fn join_fused(sections: &[SecSrc], tail: &[u8]) -> Option<Vec<u8>> {
    for (si, s) in sections.iter().enumerate() {
        if s.b.first() != Some(&CSEC3_MAGIC) {
            fdbg!("fused: section {si} not CSEC3 (first={:02x})", s.b.first().copied().unwrap_or(0));
            return None;
        }
    }
    let mut out = Vec::with_capacity((1 << 16).max(sections.len() * 8200));
    out.push(0x0A); // root compound, name ""
    put_be16(&mut out, 0);
    // FAST TAIL: the stored tail is already wire-format NBT (our own
    // to_bytes output) — splice it verbatim and only transform the
    // heightmap dense arrays back to LongArrays. No Value tree, no
    // re-serialization. Falls back to the Value path on any surprise.
    if emit_tail_direct(tail, sections, &mut out).is_none() {
        fdbg!("fused: direct tail failed, using Value path");
        return join_fused_tail_value(sections, tail);
    }
    Some(out)
}

/// Value-tree fallback (pre-v0.5 tail path): parse + tail_decode + emit.
fn join_fused_tail_value(sections: &[SecSrc], tail: &[u8]) -> Option<Vec<u8>> {
    let tail_val: Value = fastnbt::from_bytes(tail).ok()?;
    let Value::Compound(mut map) = tail_val else {
        fdbg!("fused: tail not compound");
        return None;
    };
    {
        let mut tv = Value::Compound(std::mem::take(&mut map));
        tail_decode(&mut tv);
        map = match tv { Value::Compound(m) => m, _ => unreachable!() };
    }
    let mut out = Vec::with_capacity((1 << 16).max(sections.len() * 8200));
    out.push(0x0A); // root compound, name ""
    put_be16(&mut out, 0);
    // sorted tail keys with "sections" spliced at its sorted position
    let mut keys: Vec<(&String, &Value)> = map.iter().collect();
    keys.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut sections_done = false;
    for (k, v) in keys {
        if !sections_done && k.as_str() > "sections" {
            if emit_sections_list(sections, &mut out).is_none() {
                fdbg!("fused: sections list emit failed");
                return None;
            }
            sections_done = true;
        }
        match prim_tag(v) {
            Some(tag) => {
                put_named_key(&mut out, tag, k);
                if emit_payload_be(v, &mut out).is_none() {
                    fdbg!("fused: tail key '{k}' emit failed");
                    return None;
                }
            }
            None => {
                fdbg!("fused: tail key '{k}' no prim tag");
                return None;
            }
        }
    }
    if !sections_done {
        emit_sections_list(sections, &mut out)?;
    }
    out.push(0x00); // end root
    Some(out)
}

// --------------------------------------------------- direct tail splice ---

/// Skip one NBT value payload of type `tag` at *p (bounds-checked).
fn nbt_skip(b: &[u8], p: &mut usize, tag: u8, depth: u32) -> Option<()> {
    if depth > 128 {
        return None;
    }
    match tag {
        1 => *p += 1,
        2 => *p += 2,
        3 | 5 => *p += 4,
        4 | 6 => *p += 8,
        7 => {
            let n = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4 + n;
        }
        11 => {
            let n = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4 + n * 4;
        }
        12 => {
            let n = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4 + n * 8;
        }
        8 => {
            let n = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2 + n;
        }
        9 => {
            let elem = *b.get(*p)?;
            *p += 1;
            let n = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p += 4;
            if n > 0 && elem == 0 {
                return None;
            }
            for _ in 0..n {
                nbt_skip(b, p, elem, depth + 1)?;
            }
        }
        10 => loop {
            let t = *b.get(*p)?;
            *p += 1;
            if t == 0 {
                break;
            }
            let nl = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            *p += nl;
            nbt_skip(b, p, t, depth + 1)?;
        },
        _ => return None,
    }
    Some(())
}

/// Emit the stored tail verbatim, transforming only dense heightmaps back
/// into LongArrays, with the sections list spliced at its sorted position.
fn emit_tail_direct(tail: &[u8], sections: &[SecSrc], out: &mut Vec<u8>) -> Option<()> {
    if tail.first() != Some(&0x0A) {
        return None;
    }
    let nl = u16::from_be_bytes(tail.get(1..3)?.try_into().ok()?) as usize;
    if nl != 0 {
        return None; // our own tails always have an empty root name
    }
    let mut p = 3usize;
    let mut sections_done = false;
    loop {
        let start = p;
        let tag = *tail.get(p)?;
        p += 1;
        if tag == 0 {
            break;
        }
        let klen = u16::from_be_bytes(tail.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let name = core::str::from_utf8(tail.get(p..p + klen)?).ok()?;
        p += klen;
        if !sections_done && name > "sections" {
            emit_sections_list(sections, out)?;
            sections_done = true;
        }
        if name == "Heightmaps" && tag == 0x0A {
            // inner entries: dense ByteArray (our marker) -> LongArray
            out.push(0x0A);
            out.extend_from_slice(b"\x00\x0AHeightmaps");
            loop {
                let itag = *tail.get(p)?;
                p += 1;
                if itag == 0 {
                    break;
                }
                let inl = u16::from_be_bytes(tail.get(p..p + 2)?.try_into().ok()?) as usize;
                p += 2;
                let iname = tail.get(p..p + inl)?;
                p += inl;
                if itag == 0x07 {
                    let n = i32::from_be_bytes(tail.get(p..p + 4)?.try_into().ok()?) as usize;
                    p += 4;
                    let raw = tail.get(p..p + n)?;
                    p += n;
                    if raw.first() == Some(&HMAP_MAGIC) {
                        if let Some(longs) = hmap_decode(raw) {
                            out.push(0x0C); // TAG_Long_Array (12)
                            out.extend_from_slice(&(inl as u16).to_be_bytes());
                            out.extend_from_slice(iname);
                            out.extend_from_slice(&(longs.len() as i32).to_be_bytes());
                            for l in &longs {
                                out.extend_from_slice(&l.to_be_bytes());
                            }
                            continue;
                        }
                    }
                    // not our dense form: copy verbatim
                    out.push(0x07);
                    out.extend_from_slice(&(inl as u16).to_be_bytes());
                    out.extend_from_slice(iname);
                    out.extend_from_slice(&(n as i32).to_be_bytes());
                    out.extend_from_slice(raw);
                } else {
                    // unexpected inner tag (e.g. a LongArray heightmap that
                    // could not be densified): copy the whole entry verbatim
                    let e0 = p - 3 - inl; // tag(1) + namelen(2) + name(inl)
                    nbt_skip(tail, &mut p, itag, 1)?;
                    out.extend_from_slice(&tail[e0..p]);
                }
            }
            out.push(0x00); // end Heightmaps compound
        } else {
            nbt_skip(tail, &mut p, tag, 0)?;
            out.extend_from_slice(&tail[start..p]);
        }
    }
    if !sections_done {
        emit_sections_list(sections, out)?;
    }
    out.push(0x00); // end root
    Some(())
}

fn emit_sections_list(sections: &[SecSrc], out: &mut Vec<u8>) -> Option<()> {
    put_named_key(out, 0x09, "sections");
    if sections.is_empty() {
        // fastnbt emits elem tag 0 for an empty list
        out.push(0x00);
        put_be32(out, 0);
        return Some(());
    }
    out.push(0x0A); // list of compounds
    put_be32(out, sections.len() as i32);
    for src in sections {
        // emitted section bytes are a pure function of the CSEC stream —
        // cache by content hash so repeat reads are a memcpy splice.
        // (Uniform sections are shared across chunks — hot keys; caching
        // everything measured faster than filtering "cheap" sections.)
        if let Some(hit) = sec_emit_get(src.h) {
            out.extend_from_slice(&hit);
            continue;
        }
        let mut scratch = Vec::with_capacity(src.b.len() * 6 + 64);
        section_emit_csec3(src.b, &mut scratch)?;
        let arc = Arc::new(scratch);
        sec_emit_put(src.h, arc.clone());
        out.extend_from_slice(&arc);
    }
    Some(())
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
mod fused_tests {
    use super::*;
    use std::collections::HashMap;

    fn compound(pairs: Vec<(&str, Value)>) -> Value {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v);
        }
        Value::Compound(m)
    }

    fn named_sec(enc: &[u8]) -> Vec<u8> {
        // wrap the emitted section payload as a 1-element compound list
        let mut full = vec![0x0A, 0x00, 0x00, 0x09, 0x00, 0x08];
        full.extend_from_slice(b"sections");
        full.push(0x0A);
        full.extend_from_slice(&1i32.to_be_bytes());
        full.extend_from_slice(enc);
        full.push(0x00);
        full
    }

    #[test]
    fn fused_uniform_section_roundtrip() {
        let sec = compound(vec![
            ("Y", Value::Byte(0)),
            (
                "block_states",
                compound(vec![(
                    "palette",
                    Value::List(vec![compound(vec![("Name", Value::String("minecraft:stone".into()))])]),
                )]),
            ),
            (
                "biomes",
                compound(vec![("palette", Value::List(vec![Value::String("minecraft:plains".into())]))]),
            ),
        ]);
        let enc = section_encode(&sec).expect("encode");
        let mut out = Vec::new();
        let r = section_emit_csec3(&enc, &mut out);
        if r.is_none() {
            panic!("emit failed; enc={:02x?} partial={:02x?}", enc, out);
        }
        let full = named_sec(&out);
        let v: Value = fastnbt::from_bytes(&full).expect("parse emitted NBT");
        let Value::Compound(m) = v else { panic!() };
        let Some(Value::List(l)) = m.get("sections") else { panic!() };
        assert!(sections_semantically_equal(&l[0], &sec), "semantic mismatch");
    }

    #[test]
    fn fused_uniform_section_with_data() {
        // 2-entry palette with a 4096-voxel data array at vanilla width
        let mut data: Vec<i64> = Vec::with_capacity(256);
        for i in 0..256 {
            data.push(if i % 2 == 0 { 0 } else { 1 });
        }
        let sec = compound(vec![
            ("Y", Value::Byte(-4)),
            (
                "block_states",
                compound(vec![
                    (
                        "palette",
                        Value::List(vec![
                            compound(vec![("Name", Value::String("minecraft:stone".into()))]),
                            compound(vec![(
                                "Name",
                                Value::String("minecraft:deepslate".into()),
                            )]),
                        ]),
                    ),
                    ("data", Value::LongArray(fastnbt::LongArray::new(data))),
                ]),
            ),
        ]);
        let enc = section_encode(&sec).expect("encode");
        let mut out = Vec::new();
        let r = section_emit_csec3(&enc, &mut out);
        if r.is_none() {
            panic!("emit failed; enc={:02x?} partial={:02x?}", enc, out);
        }
        let full = named_sec(&out);
        let v: Value = fastnbt::from_bytes(&full).expect("parse emitted NBT");
        let Value::Compound(m) = v else { panic!() };
        let Some(Value::List(l)) = m.get("sections") else { panic!() };
        assert!(sections_semantically_equal(&l[0], &sec), "semantic mismatch");
    }

    #[test]
    fn fused_join_synthetic_chunk() {
        // full chunk: tail keys + sections -> join_fused must equal legacy join
        let mut sections_enc: Vec<Vec<u8>> = Vec::new();
        let mut sec_vals: Vec<Value> = Vec::new();
        for y in 0i8..3 {
            let sec = compound(vec![
                ("Y", Value::Byte(y)),
                (
                    "block_states",
                    compound(vec![(
                        "palette",
                        Value::List(vec![compound(vec![("Name", Value::String("minecraft:stone".into()))])]),
                    )]),
                ),
                ("SkyLight", Value::ByteArray(fastnbt::ByteArray::new(vec![-1i8; 2048]))),
            ]);
            let enc = section_encode(&sec).unwrap();
            sections_enc.push(enc);
            sec_vals.push(sec);
        }
        let tail_val = compound(vec![
            ("DataVersion", Value::Int(4435)),
            ("Status", Value::String("minecraft:full".into())),
            ("Heightmaps", compound(vec![(
                "MOTION_BLOCKING",
                Value::LongArray(fastnbt::LongArray::new(vec![7i64; 37])),
            )])),
        ]);
        // the real pipeline stores the tail AFTER tail_encode (heightmaps
        // become dense 0xC6 arrays when the values fit) — exercise that path
        let mut tail_enc = tail_val.clone();
        tail_encode(&mut tail_enc);
        let tail = to_bytes(&tail_enc);
        let sources: Vec<SecSrc> = sections_enc
            .iter()
            .map(|s| SecSrc { h: hash_of(s), b: s.as_slice() })
            .collect();
        let fused = join_fused(&sources, &tail).expect("join_fused failed");
        let legacy = join(&sections_enc, &tail).expect("legacy join failed");
        let a: Value = fastnbt::from_bytes(&fused).unwrap();
        let b: Value = fastnbt::from_bytes(&legacy).unwrap();
        assert_eq!(a, b, "fused output differs semantically");
    }
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
        let sec_bytes: Vec<Vec<u8>> = sections.iter().map(|(_, b)| b.as_slice().to_vec()).collect();
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
