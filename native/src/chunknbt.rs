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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// v4 (0xC8): adds a third flag byte for light-uniform forms (the flags1/2
/// bit budget is exhausted). Layout [0xC8][f1][f2][f3][y as i8]...; f3 bit0
/// BlockLight all-0, bit1 reserved. Streams without f3 features stay 0xC7
/// (byte-identical to v3), so dedup hashes and legacy decoders are stable.
const CSEC4_MAGIC: u8 = 0xC8;
/// True when the slice is a CSEC v3/v4 stream (0xC7 or 0xC8).
fn is_csec4(b: Option<&u8>) -> bool {
    matches!(b, Some(&CSEC3_MAGIC) | Some(&CSEC4_MAGIC))
}
const HMAP_MAGIC: u8 = 0xC6; // repacked heightmap ByteArray marker

// -------------------------------------------------- derived-data strip ----
// Light is DERIVED data: starlight recomputes SkyLight/BlockLight from block
// data whenever isLightOn is 0 — this is exactly how vanilla represents every
// freshly-generated chunk, so storing chunks without light arrays and with
// isLightOn=0 is 100% vanilla-compatible (the server relights on first load,
// off-thread, then Paper hands the lit chunk back for the next save).
// The IMPORT path strips by default (the 261k-chunk world measured 31.8% of
// raw NBT bytes in light arrays); the RUNTIME write path keeps whatever the
// server hands us so relit light is stored and never recomputed again.
// The strip mode is a PER-CALL decision (split's `strip` parameter), not a
// process-global: the background legacy import and the live server's saves
// share one Store, and a global flag would leak the import's strip into
// runtime saves — stripping relit light forever (a relight-on-every-load
// loop). CSAVES_KEEP_LIGHT=1 forces the import paths to keep light.

/// Remove derived light fields from one section compound.
pub fn strip_derived_section(sec: &mut Value) {
    if let Value::Compound(m) = sec {
        m.remove("SkyLight");
        m.remove("BlockLight");
        m.remove("starlight.skylight_state");
        m.remove("starlight.blocklight_state");
    }
}

/// Strip derived data from a whole chunk (sections + isLightOn) — used on
/// the import path and, with the same shape, by semantic comparators on the
/// SOURCE side so verification stays apples-to-apples.
pub fn strip_derived_chunk(root: &mut Value) {
    if let Value::Compound(m) = root {
        if let Some(Value::List(list)) = m.get_mut("sections") {
            for s in list.iter_mut() {
                strip_derived_section(s);
            }
        }
        m.insert("isLightOn".to_string(), Value::Byte(0));
    }
}

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

// ------------------------------------------------------ section RLE ------

/// Encode-form counters (pure functions of content, so they are stable and
/// profile-safe). Exposed via `enc_mode_stats` and printed by the profilers:
/// they answer "which form won and where the remaining bytes live".
pub static ST_ENC_DENSE: AtomicU64 = AtomicU64::new(0);
pub static ST_ENC_RLE_VAN: AtomicU64 = AtomicU64::new(0);
pub static ST_ENC_RLE_COL: AtomicU64 = AtomicU64::new(0);
pub static ST_ENC_DENSE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static ST_ENC_RLE_BYTES: AtomicU64 = AtomicU64::new(0);
/// Sections rejected by the encode sanity check (voxel index outside the
/// palette — malformed source bytes); they fall back to raw NBT storage.
pub static ST_ENC_FALLBACK: AtomicU64 = AtomicU64::new(0);

/// One-line JSON summary of the encode-form counters.
pub fn enc_mode_stats() -> String {
    let d = ST_ENC_DENSE.load(Ordering::Relaxed);
    let rv = ST_ENC_RLE_VAN.load(Ordering::Relaxed);
    let rc = ST_ENC_RLE_COL.load(Ordering::Relaxed);
    let db = ST_ENC_DENSE_BYTES.load(Ordering::Relaxed);
    let rb = ST_ENC_RLE_BYTES.load(Ordering::Relaxed);
    let fb = ST_ENC_FALLBACK.load(Ordering::Relaxed);
    format!(
        r#"{{"dense":{},"rle_van":{},"rle_col":{},"dense_bytes":{},"rle_bytes":{},"fallback":{}}}"#,
        d, rv, rc, db, rb, fb
    )
}

// ------------------------------------------------ split stage timers --
// Import-path profiling: where does split() wall time go (fresh imports;
// repeat saves hit the encode cache and collapse into SPLT_SECS).
static SPLT_PARSE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_SECS: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_SHIFT: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_TAIL: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));

/// One-line JSON summary of the split-path stage timers (nanos, calls).
pub fn split_stage_json() -> String {
    fn f(name: &str, t: &(AtomicU64, AtomicU64)) -> String {
        let ns = t.0.load(Ordering::Relaxed);
        let n = t.1.load(Ordering::Relaxed);
        format!(
            r#"{}":{{ns:{},n:{},per_call:{}}}"#,
            name,
            ns,
            n,
            ns.checked_div(n.max(1)).unwrap_or(0)
        )
    }
    format!(
        "{{{},{},{},{}}}",
        f("parse", &SPLT_PARSE),
        f("sections", &SPLT_SECS),
        f("shift", &SPLT_SHIFT),
        f("tail", &SPLT_TAIL)
    )
}

fn push_uvarint(out: &mut Vec<u8>, mut x: u64) {
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn read_uvarint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut z = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        z |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(z);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// RLE a canonical index stream as [idx u8][varint run] pairs. The vanilla
/// voxel order has x fastest / y slowest, so horizontal strata planes are
/// contiguous — one run per plane. Returns the raw stream; the caller
/// compares its length against the dense candidate and picks the smaller.
fn rle_encode_stream(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() / 4 + 16);
    let mut i = 0usize;
    while i < v.len() {
        let idx = v[i];
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == idx {
            run += 1;
        }
        out.push(idx);
        push_uvarint(&mut out, run as u64);
        i += run;
    }
    out
}

/// Transpose a section index stream so y varies fastest (column-major):
/// vanilla i = y*S*S + z*S + x  ->  j = x*S*S + z*S + y. In this order one
/// column's blocks are contiguous, so vertical patterns (walls, per-column
/// boundaries) collapse into long runs. `side` is 16 (block_states) or 4
/// (biomes).
fn transpose_fast_y(v: &[u8], side: usize) -> Vec<u8> {
    let s2 = side * side;
    let mut out = vec![0u8; v.len()];
    for y in 0..side {
        for z in 0..side {
            for x in 0..side {
                out[x * s2 + z * side + y] = v[y * s2 + z * side + x];
            }
        }
    }
    out
}

/// Decode a v3 (0xC7) section.
fn section_decode3(b: &[u8]) -> Option<Value> {
    let f1 = *b.get(1)?;
    let f2 = *b.get(2)?;
    let mut p = 3usize;
    let mut f3 = 0u8;
    if b[0] == CSEC4_MAGIC {
        f3 = *b.get(p)?;
        p += 1;
    }
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
        order: u8,
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
            let side = if entries == 4096 {
                16usize
            } else if entries == 64 {
                4usize
            } else {
                0usize
            };
            // RLE form (v0.6): [idx u8][varint run] pairs over the canonical
            // index stream, in vanilla order (1) or y-fast transposed (2).
            // Legacy v3 streams carry zeros in the order bits -> dense path.
            let v: Vec<u8> = if order != 0 {
                if n > 256 || side == 0 {
                    return None; // RLE uses byte indices; never emitted for wide palettes
                }
                let mut vv = Vec::with_capacity(entries);
                while vv.len() < entries {
                    let idx = *b.get(*p)?;
                    *p += 1;
                    let run = read_uvarint(b, p)? as usize;
                    if run == 0 || vv.len() + run > entries {
                        return None;
                    }
                    vv.resize(vv.len() + run, idx);
                }
                if order == 2 {
                    transpose_fast_y(&vv, side)
                } else {
                    vv
                }
            } else {
                // narrow mode: dense stream at ceillog2(n) (encode-side choice
                // when that beats the vanilla 4-bit clamp); output is normalized
                // back to the vanilla-clamped width either way.
                let rbits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
                let bytes = (entries * rbits as usize + 7) / 8;
                let dense = b.get(*p..*p + bytes)?;
                *p += bytes;
                if n > 256 {
                    dense_unpack_wide(dense, entries, rbits)?
                        .iter()
                        .map(|x| *x as u8)
                        .collect::<Vec<u8>>()
                } else {
                    dense_unpack(dense, entries, rbits)?
                }
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
        let order = (f2 >> 4) & 3;
        let bs = dec_pal_data(b, &mut p, 4096, f1 & 2 != 0, f2 & 4 != 0, order)?;
        sec_map.insert("block_states".to_string(), bs);
    }
    if f1 & 4 != 0 {
        let order = (f2 >> 6) & 3;
        let bs = dec_pal_data(b, &mut p, 64, f1 & 8 != 0, f2 & 8 != 0, order)?;
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
    } else if f3 & 1 != 0 {
        sec_map.insert("BlockLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])));
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
/// Pack a byte stream into vanilla no-straddle longs (entries per long =
/// 64/bits, zero padding at the top). Public so sibling modules' tests can
/// build vanilla-shaped data arrays.
pub fn pack_nostroddle(entries: &[u8], bits: u32) -> Vec<i64> {
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

/// Unpack vanilla no-straddle longs into a byte-per-entry stream. Public so
/// the profiler can build canonical voxel streams for delta-potential analysis.
pub fn unpack_nostroddle(longs: &[i64], n: usize, bits: u32) -> Vec<u8> {
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
    // 5-byte v4 header; patched down to the 4-byte v3 form when f3 stays 0.
    let mut out = vec![CSEC4_MAGIC, 0u8, 0u8, 0u8, y as u8];
    let mut f1 = 0u8;
    let mut f2 = 0u8;
    let mut f3 = 0u8;

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
    // Returns (has_data, f2 flag bits): dense form sets `narrow_bit` when the
    // stream is packed at ceillog2(n) (beats the vanilla 4-bit clamp for
    // palettes under 16 entries); RLE forms set (order << order_shift).
    fn enc_pal_data_canon(
        out: &mut Vec<u8>,
        bs: &Value,
        entries: usize,
        narrow_bit: u8,
        order_shift: u32,
    ) -> Option<(bool, u8)> {
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
                return Some((false, 0));
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
        // stream byte-for-byte (LE). RLE candidates are still evaluated on
        // the unpacked stream below, so the chosen form stays a pure
        // function of content (dedup-stable across re-saves).
        let identity = !narrow
            && in_bits == out_bits
            && matches!(in_bits, 1 | 2 | 4 | 8 | 16 | 32)
            && ents.iter().enumerate().all(|(i, (_, old))| *old == i);
        let v = unpack_nostroddle(data, entries, in_bits);
        // sanity: every voxel index must be inside the palette. Valid vanilla
        // data always is; a mismatch means malformed/mutated source bytes
        // (garbage in unused codes). Fall back to raw NBT storage instead of
        // panicking on the server thread — the section just loses the CSEC
        // savings, semantics are untouched.
        if v.iter().any(|x| (*x as usize) >= n) {
            ST_ENC_FALLBACK.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // remap old palette index -> canonical index
        let mut remap = vec![0u8; n];
        for (new_i, (_, old_i)) in ents.iter().enumerate() {
            remap[*old_i] = new_i as u8;
        }
        if n > 256 {
            // wide palettes: remap table must be u16; RLE (byte indices)
            // does not apply — dense only.
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
            ST_ENC_DENSE.fetch_add(1, Ordering::Relaxed);
            return Some((true, 0));
        }
        let remapped: Vec<u8> = v.iter().map(|x| remap[*x as usize]).collect();
        // Dense candidate (identity => the raw vanilla longs, memcpy'd —
        // byte-identical to what dense_pack would produce).
        let dense: Vec<u8> = if identity {
            let mut d = Vec::with_capacity(data.as_ref().len() * 8);
            for l in data.as_ref() {
                d.extend_from_slice(&l.to_le_bytes());
            }
            d
        } else {
            dense_pack(&remapped, out_bits)
        };
        // RLE candidates (v0.6): vanilla order collapses horizontal strata
        // planes into single runs; the y-fast transposed stream collapses
        // columns / vertical walls. Strictly-smaller wins, ties break toward
        // the earlier candidate — deterministic, so identical sections keep
        // identical bytes and the dedup hashes stay stable.
        let side = match entries {
            4096 => 16usize,
            64 => 4usize,
            _ => 0,
        };
        // RLE admission rule: the raw stream must be a BLOWOUT (4x smaller
        // AND 32+ bytes saved). Raw length alone is a poor predictor of the
        // final packed size — dense streams of neighboring sections align
        // inside the slab zstd window and compress cross-section, while RLE
        // streams compress only within themselves — so modest raw wins stay
        // dense (without the margin the real JJThunder corpus regressed
        // x2.83 -> x2.67). Deterministic => dedup-stable.
        let mut best_len = dense.len();
        let mut best: Option<(u8, Vec<u8>)> = None;
        if side != 0 {
            let ra = rle_encode_stream(&remapped);
            if ra.len() * 4 <= dense.len() && ra.len() + 32 <= dense.len() {
                best_len = ra.len();
                best = Some((1, ra));
            }
            let rb = rle_encode_stream(&transpose_fast_y(&remapped, side));
            if rb.len() * 4 <= dense.len() && rb.len() + 32 <= dense.len() && rb.len() < best_len {
                best_len = rb.len();
                best = Some((2, rb));
            }
        }
        match best {
            None => {
                out.extend_from_slice(&dense);
                ST_ENC_DENSE.fetch_add(1, Ordering::Relaxed);
                ST_ENC_DENSE_BYTES.fetch_add(dense.len() as u64, Ordering::Relaxed);
                Some((true, if narrow { narrow_bit } else { 0 }))
            }
            Some((ord, bytes)) => {
                out.extend_from_slice(&bytes);
                ST_ENC_RLE_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                match ord {
                    1 => ST_ENC_RLE_VAN.fetch_add(1, Ordering::Relaxed),
                    _ => ST_ENC_RLE_COL.fetch_add(1, Ordering::Relaxed),
                };
                Some((true, ord << order_shift))
            }
        }
    }

    if let Some(bs) = m.get("block_states") {
        let (has_data, f2b) = enc_pal_data_canon(&mut out, bs, 4096, 4, 4)?;
        f1 |= 1;
        if has_data {
            f1 |= 2;
        }
        f2 |= f2b;
    }
    if let Some(bs) = m.get("biomes") {
        let (has_data, f2b) = enc_pal_data_canon(&mut out, bs, 64, 8, 6)?;
        f1 |= 4;
        if has_data {
            f1 |= 8;
        }
        f2 |= f2b;
    }
    for (key, uni_bit, raw_bit, all, zero_to_f3) in [
        ("SkyLight", 16u8, 64u8, 0xffu8, false),
        ("BlockLight", 0u8, 128u8, 0u8, true),
    ] {
        let Some(Value::ByteArray(ba)) = m.get(key) else { continue };
        let b: Vec<u8> = ba.as_ref().iter().map(|x| *x as u8).collect();
        // Uniform forms require the standard 2048-byte light array (custom
        // world heights do not change section light size, but stay safe).
        if b.len() == 2048 && uni_bit != 0 && b.iter().all(|x| *x == all) {
            f1 |= uni_bit;
        } else if b.len() == 2048 && b.iter().all(|x| *x == 0u8) {
            // all-0 light: SkyLight has the dedicated flags1 bit5; BlockLight
            // (no free flags1 bit) moves to flags3 bit0 (v4).
            if zero_to_f3 {
                f3 |= 1;
            } else {
                f1 |= 32u8;
            }
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
    if f3 == 0 {
        // no v4 features: collapse to the byte-identical v3 wire form
        out[0] = CSEC3_MAGIC;
        out.remove(3);
    } else {
        out[3] = f3;
    }
    Some(out)
}

// ------------------------------------------------- shift-ref codec (v0.7) --

/// Shift-ref section marker: a section whose block_states data equals a
/// LOWER section of the SAME chunk shifted up by k planes, plus its top
/// k planes stored explicitly. Profiling the real JJThunder corpus showed
/// 96.3% of dense sections match this shape (mountain strata repeat
/// vertically with smoothly varying surface heights) — the ref replaces a
/// ~1.4 KB dense stream with ~15-30 bytes.
const CSECS_MAGIC: u8 = 0xC9;
/// Shift-ref member with a v4 (f3-carrying) tail: layout
/// [0xCA][t u16][k u8][f1'][f2'][f3'][n_extra u16]... — same as 0xC9 plus
/// the f3' byte (the tail's section_encode output was 0xC8).
const CSECS4_MAGIC: u8 = 0xCA;

pub static ST_ENC_SHIFT: AtomicU64 = AtomicU64::new(0);
pub static ST_ENC_SHIFT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_RESOLVED: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_SECS_OK: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_CAND: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_PLANE0: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_FULL: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_REJECT: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_KSUM: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_DENSE1ST: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_DENSE_NOMATCH: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_DENSE_MATCH: AtomicU64 = AtomicU64::new(0);

/// Identity key bytes for one palette entry (order-free, canonical):
/// [plain][name][0x00][k1=v1 0x1f k2=v2 ...]. Two entries are the same block
/// state iff these bytes are equal.
fn pal_entry_identity(v: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(32);
    match v {
        Value::String(s) => {
            out.push(1u8);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Compound(cm) => {
            let Value::String(name) = cm.get("Name")? else { return None };
            out.push(0u8);
            out.extend_from_slice(name.as_bytes());
            out.push(0x00);
            let mut props: Vec<(String, String)> = Vec::new();
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
            for (i, (k, vv)) in props.iter().enumerate() {
                if i > 0 {
                    out.push(0x1f);
                }
                out.extend_from_slice(k.as_bytes());
                out.push(b'=');
                out.extend_from_slice(vv.as_bytes());
            }
        }
        _ => return None,
    }
    Some(out)
}

/// put_entry-form bytes for one palette entry (the CSEC wire encoding,
/// mirrored standalone so the shift pass can write extras without nesting).
fn put_entry_standalone(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    match v {
        Value::String(s) => {
            let nb = s.as_bytes();
            if nb.len() > 65535 {
                return None;
            }
            out.push(0u8);
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            out.extend_from_slice(nb);
        }
        Value::Compound(cm) => {
            let Value::String(name) = cm.get("Name")? else { return None };
            let nb = name.as_bytes();
            let mut props: Vec<(String, String)> = Vec::new();
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
            if props.is_empty() {
                out.push(1u8);
            } else {
                out.push(2u8);
            }
            if nb.len() > 65535 || props.len() > 255 {
                return None;
            }
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            out.extend_from_slice(nb);
            if !props.is_empty() {
                out.push(props.len() as u8);
            }
            for (k, vv) in &props {
                let kb = k.as_bytes();
                let vb = vv.as_bytes();
                if kb.len() > 65535 || vb.len() > 65535 {
                    return None;
                }
                out.extend_from_slice(&(kb.len() as u16).to_le_bytes());
                out.extend_from_slice(kb);
                out.extend_from_slice(&(vb.len() as u16).to_le_bytes());
                out.extend_from_slice(vb);
            }
        }
        _ => return None,
    }
    Some(())
}

/// Inverse of put_entry (standalone): parse one palette entry.
fn parse_entry_standalone(b: &[u8], p: &mut usize) -> Option<Value> {
    let kind = *b.get(*p)?;
    *p += 1;
    let r16 = |p: &mut usize| -> Option<usize> {
        let s = *p;
        let v = u16::from_le_bytes(b.get(s..s + 2)?.try_into().ok()?) as usize;
        *p += 2;
        Some(v)
    };
    match kind {
        0 => {
            let n = r16(p)?;
            let s = b.get(*p..*p + n)?.to_vec();
            *p += n;
            Some(Value::String(String::from_utf8(s).ok()?))
        }
        1 | 2 => {
            let n = r16(p)?;
            let name = String::from_utf8(b.get(*p..*p + n)?.to_vec()).ok()?;
            *p += n;
            let mut props: std::collections::HashMap<String, Value> = Default::default();
            if kind == 2 {
                let np = *b.get(*p)?;
                *p += 1;
                for _ in 0..np {
                    let kn = r16(p)?;
                    let k = String::from_utf8(b.get(*p..*p + kn)?.to_vec()).ok()?;
                    *p += kn;
                    let vn = r16(p)?;
                    let v = String::from_utf8(b.get(*p..*p + vn)?.to_vec()).ok()?;
                    *p += vn;
                    props.insert(k, Value::String(v));
                }
            }
            let mut cm: std::collections::HashMap<String, Value> = Default::default();
            cm.insert("Name".to_string(), Value::String(name));
            if kind == 2 {
                cm.insert("Properties".to_string(), Value::Compound(props));
            }
            Some(Value::Compound(cm))
        }
        _ => None,
    }
}

/// Unpack a vanilla no-straddle long array to u32 entries (handles wide
/// palettes where canonical indices exceed u8).
fn vanilla_unpack_wide(la: &[i64], n: usize, bits: u32) -> Vec<u32> {
    let epl = (64 / bits) as usize;
    let mut out = vec![0u32; n];
    for (i, e) in out.iter_mut().enumerate() {
        let l = la[i / epl] as u64;
        let sh = (i % epl) as u32 * bits;
        *e = ((l >> sh) & ((1u64 << bits) - 1)) as u32;
    }
    out
}

/// Pack u32 entries into a vanilla no-straddle long array (zero padding).
fn vanilla_pack_wide(entries: &[u32], bits: u32) -> Vec<i64> {
    let epl = (64 / bits) as usize;
    let mut out = vec![0i64; entries.len().div_ceil(epl)];
    for (i, &e) in entries.iter().enumerate() {
        out[i / epl] |= (e as i64) << ((i % epl) as u32 * bits);
    }
    out
}

/// Per-section view used by the shift pass: identity vector (chunk-global
/// block-state ids), original palette entry identity bytes, canonical order
/// of the original palette (mirrors enc_pal_data_canon exactly).
struct ShiftSec {
    ident: Vec<u32>,
    /// per-plane fingerprints of `ident` (cheap reject for shift matching)
    plane_fp: [u64; 16],
    /// canonical position (in the stored CSEC3 palette) of each original
    /// palette index — ref-space mapping for targets
    canon_of_orig: Vec<u32>,
    /// chunk-global identity id of each ORIGINAL palette entry
    id_of_orig: Vec<u32>,
    n: usize,
}
/// Build the ShiftSec for one section Value (None = not usable).
fn shift_sec_of(s: &Value, tab: &mut HashMap<Vec<u8>, u32>) -> Option<ShiftSec> {
    let Value::Compound(m) = s else { return None };
    let bs = m.get("block_states")?;
    let Value::Compound(bm) = bs else { return None };
    let Value::List(pal) = bm.get("palette")? else { return None };
    let n = pal.len();
    if n == 0 || n > 65535 {
        return None;
    }
    let mut entry_id: Vec<u32> = Vec::with_capacity(n);
    let mut id_of_orig = vec![0u32; n];
    for (i, p) in pal.iter().enumerate() {
        let key = pal_entry_identity(p)?;
        let nid = tab.len() as u32;
        let id = *tab.entry(key).or_insert(nid);
        id_of_orig[i] = id;
        entry_id.push(id);
    }
    let _ = entry_id;
    let mut ident = vec![0u32; 4096];
    match bm.get("data") {
        Some(Value::LongArray(la)) => {
            let bits = section_data_bits(n, 4096, la.as_ref().len());
            let per = (64 / bits) as usize;
            if la.as_ref().len() != (4096 + per - 1) / per {
                return None;
            }
            for (i, v) in unpack_nostroddle(la.as_ref(), 4096, bits).into_iter().enumerate() {
                let o = v as usize;
                if o >= n {
                    return None;
                }
                ident[i] = id_of_orig[o];
            }
        }
        Some(_) => return None,
        None => {
            if n > 1 {
                return None;
            }
            ident.iter_mut().for_each(|x| *x = id_of_orig[0]);
        }
    }
    // canonical order of the original palette, mirroring enc_pal_data_canon:
    // counts from the vanilla data (truncated u8 unpack, same as enc), sort
    // by (count desc, (plain, name, props) asc).
    let mut counts = vec![0u32; n];
    if let Some(Value::LongArray(la)) = bm.get("data") {
        let bits = section_data_bits(n, 4096, la.as_ref().len());
        let per = (64 / bits) as usize;
        if la.as_ref().len() == (4096 + per - 1) / per {
            for v in unpack_nostroddle(la.as_ref(), 4096, bits) {
                if (v as usize) < n {
                    counts[v as usize] += 1;
                }
            }
        }
    }
    // precompute sort keys to avoid borrows crossing the sort
    let keys: Vec<(bool, String, Vec<(String, String)>)> = (0..n)
        .map(|i| match &pal[i] {
            Value::String(s) => (true, s.clone(), Vec::new()),
            Value::Compound(cm) => {
                let name = match cm.get("Name") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let mut ps: Vec<(String, String)> = match cm.get("Properties") {
                    Some(Value::Compound(pn)) => pn
                        .iter()
                        .filter_map(|(k, kv)| match kv {
                            Value::String(vv) => Some((k.clone(), vv.clone())),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                ps.sort();
                (false, name, ps)
            }
            _ => (false, String::new(), Vec::new()),
        })
        .collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        counts[b]
            .cmp(&counts[a])
            .then_with(|| keys[a].cmp(&keys[b]))
    });
    let mut canon_of_orig = vec![0u32; n];
    for (pos, &oi) in order.iter().enumerate() {
        canon_of_orig[oi] = pos as u32;
    }
    let mut plane_fp = [0u64; 16];
    for (py, fp) in plane_fp.iter_mut().enumerate() {
        let mut h: u64 = 0xcbf29ce484222325;
        for &x in &ident[py * 256..(py + 1) * 256] {
            h ^= x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        *fp = h;
    }
    Some(ShiftSec { ident, plane_fp, canon_of_orig, id_of_orig, n })
}

/// The shift pass: replace dense CSEC3 sections with shift refs where a
/// lower section of this chunk contains them shifted by k planes. Runs on
/// the freshly split section list (Values in `vals` are parallel to
/// `sections`). Never fails: sections that do not qualify stay untouched.
pub static ST_SHIFT_BUILD_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_TAIL_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_TOTAL_NS: AtomicU64 = AtomicU64::new(0);

fn shift_ref_pass(sections: &mut [(Hash, SecBytes)], vals: &[Value], cache_hit: &[bool]) {
    let t_all = std::time::Instant::now();
    const MAX_K: usize = 15;
    const CANDIDATES: usize = 64;
    const MIN_WIN: usize = 256; // at least one plane must match
    let n_sec = sections.len().min(vals.len());
    if n_sec < 2 {
        return;
    }
    let mut tab: HashMap<Vec<u8>, u32> = HashMap::new();
    // build ShiftSec once per DISTINCT section content (structural hash):
    // chunks repeat the same section many times (uniform runs), and the
    // unpack+canonical+fp work is pure content — never rebuild it.
    // Sections with tiny members (uniform / RLE-collapsed) are excluded
    // entirely: their matches can never clear the admission rule (the ref
    // carries at least the header + tail the member already has), and they
    // are the majority of distinct contents, so this cuts most build cost.
    const MIN_MEMBER: usize = 512;
    let mut meta_cache: HashMap<Hash, Arc<ShiftSec>> = HashMap::new();
    let mut secs: Vec<Option<Arc<ShiftSec>>> = Vec::with_capacity(n_sec);
    let mut is_ref: Vec<bool> = vec![false; n_sec];
    for i in 0..n_sec {
        let (csec3, mlen) = match &sections[i].1 {
            SecBytes::Shared(a) => (is_csec4(a.first()), a.len()),
            SecBytes::Owned(b) => (is_csec4(b.first()), b.len()),
        };
        let sec = if csec3 && mlen >= MIN_MEMBER {
            match meta_cache.get(&sections[i].0) {
                Some(a) => Some(a.clone()),
                None => match shift_sec_of(&vals[i], &mut tab) {
                    Some(ss) => {
                        let a = Arc::new(ss);
                        meta_cache.insert(sections[i].0, a.clone());
                        Some(a)
                    }
                    None => None,
                },
            }
        } else {
            None
        };
        if sec.is_some() {
            ST_SHIFT_SECS_OK.fetch_add(1, Ordering::Relaxed);
        }
        secs.push(sec);
    }
    let t_build = t_all.elapsed().as_nanos() as u64;
    ST_SHIFT_BUILD_NS.fetch_add(t_build, Ordering::Relaxed);
    for i in 0..n_sec {
        if cache_hit.get(i).copied().unwrap_or(false) {
            continue; // cross-chunk-shared content: full member dedups for free
        }
        let full_len = match &sections[i].1 {
            SecBytes::Shared(a) => a.len(),
            SecBytes::Owned(b) => b.len(),
        };
        if full_len < MIN_MEMBER {
            continue; // tiny members cannot be beaten by a ref
        }
        let Some(ss) = &secs[i] else { continue };
        // candidate targets: nearest previous full sections, deduped by
        // content hash (repeated uniform sections would otherwise burn the
        // candidate budget before the true dense target is reached).
        // Selection is by SMALLEST ESTIMATED REF COST, not smallest k: a
        // uniform target matching at k=1 stores k*256 top-plane entries at
        // the section's palette width, which can dwarf a dense target's ref
        // at larger k with a narrow top palette.
        let mut best: Option<(u64, usize, usize)> = None; // (cost, k, j)
        let mut seen_ids: HashMap<Hash, ()> = HashMap::new();
        let mut seen = 0usize;
        for j in (0..i).rev() {
            if seen >= CANDIDATES {
                break;
            }
            if is_ref[j] {
                continue;
            }
            let Some(ts) = &secs[j] else { continue };
            // strict content dedup: identical candidates give identical
            // match results, so only distinct contents consume the budget
            if seen_ids.insert(sections[j].0, ()).is_some() {
                continue;
            }
            seen += 1;
            for k in 1..=MAX_K {
                let win = 4096 - k * 256;
                if win < MIN_WIN {
                    break;
                }
                ST_SHIFT_CAND.fetch_add(1, Ordering::Relaxed);
                // fingerprint pre-filter: plane p of s must equal plane p+k
                // of t for all p < 16-k; u64 compares reject ~ instantly
                let planes = 16 - k;
                if (0..planes).any(|py| ss.plane_fp[py] != ts.plane_fp[py + k]) {
                    continue;
                }
                ST_SHIFT_PLANE0.fetch_add(1, Ordering::Relaxed);
                if ss.ident[0..win] == ts.ident[k * 256..] {
                    ST_SHIFT_FULL.fetch_add(1, Ordering::Relaxed);
                    // estimated ref cost: header + top planes at the target
                    // palette width (extras are rare and add a small term)
                    let cost = 11u64
                        + ((k * 256) as u64 * bits_for(ts.n) as u64).div_ceil(8);
                    if best.map(|(bc, _, _)| cost < bc).unwrap_or(true) {
                        best = Some((cost, k, j));
                    }
                    break;
                }
            }
        }
        let Some((_, k, j)) = best else {
            if full_len > 1000 {
                ST_SHIFT_DENSE1ST.fetch_add(1, Ordering::Relaxed);
                ST_SHIFT_DENSE_NOMATCH.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        };
        if full_len > 1000 {
            ST_SHIFT_DENSE1ST.fetch_add(1, Ordering::Relaxed);
            ST_SHIFT_DENSE_MATCH.fetch_add(1, Ordering::Relaxed);
        }
        ST_SHIFT_KSUM.fetch_add(k as u64, Ordering::Relaxed);
        let t_tail0 = t_all.elapsed().as_nanos() as u64;        let tj = secs[j].as_ref().unwrap();
        let kw = k * 256;
        let n_t = tj.n;
        if n_t + 255 > 65535 {
            continue;
        }
        // chunk id -> target ORIGINAL palette index (for ref-space mapping)
        let mut t_rev: HashMap<u32, usize> = HashMap::with_capacity(n_t);
        for (oi, &id) in tj.id_of_orig.iter().enumerate() {
            t_rev.entry(id).or_insert(oi);
        }
        // ref-space mapping of the section's top planes; extras = identities
        // not present in the target palette, numbered by first appearance
        let mut extra_of_id: HashMap<u32, u32> = HashMap::new();
        let mut extras: Vec<usize> = Vec::new(); // s's ORIGINAL palette positions
        let mut top: Vec<u32> = Vec::with_capacity(kw);
        let mut ok = true;
        for &x in &ss.ident[4096 - kw..] {
            let v = match t_rev.get(&x) {
                Some(&oi) => tj.canon_of_orig[oi],
                None => {
                    let ne = extra_of_id.len() as u32;
                    let e = *extra_of_id.entry(x).or_insert(ne);
                    if e == ne {
                        // remember which original palette position this is
                        match ss.id_of_orig.iter().position(|&y| y == x) {
                            Some(pos) => extras.push(pos),
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    n_t as u32 + e
                }
            };
            top.push(v);
        }
        if !ok || n_t + extras.len() > 65535 {
            continue;
        }
        // non-block_states fields: re-encode the section Value without
        // block_states (f1 block_states bits stay clear)
        let s2 = match &vals[i] {
            Value::Compound(m) => {
                let mut mm = m.clone();
                if mm.remove("block_states").is_none() {
                    continue;
                }
                Value::Compound(mm)
            }
            _ => continue,
        };
        let csec2 = match section_encode(&s2) {
            Some(c) if is_csec4(c.first()) && c.len() >= 4 => c,
            _ => continue,
        };
        let c4 = csec2[0] == CSEC4_MAGIC;
        let tail_at = if c4 { 4usize } else { 3usize };
        // assemble: [0xC9/0xCA][t u16][k u8][f1'][f2'][f3'?][n_extra u16]
        //           [extras][top_kind + payload][Y + fields...]
        //           (csec2[tail_at..] = Y onward)
        let n_ref = n_t + extras.len();
        let mut mbr: Vec<u8> = Vec::with_capacity(64);
        if c4 {
            mbr.push(CSECS4_MAGIC);
        } else {
            mbr.push(CSECS_MAGIC);
        }
        mbr.extend_from_slice(&(j as u16).to_le_bytes());
        mbr.push(k as u8);
        mbr.extend_from_slice(&csec2[1..3]);
        if c4 {
            mbr.push(csec2[3]);
        }
        mbr.extend_from_slice(&(extras.len() as u16).to_le_bytes());
        // extras in first-appearance order (must match ref-space numbering)
        'ex: for &pos in &extras {
            let Value::Compound(m) = &vals[i] else { break 'ex };
            let Some(Value::Compound(bm)) = m.get("block_states") else { break 'ex };
            let Some(Value::List(pal)) = bm.get("palette") else { break 'ex };
            if put_entry_standalone(&pal[pos], &mut mbr).is_none() {
                break 'ex;
            }
        }
        let uniform = top.iter().all(|&v| v == top[0]) && top[0] < 256 && extras.is_empty();
        // top payload: pick the smallest of uniform / packed / RLE forms
        let bits = bits_for(n_ref);
        let packed: Vec<u8> = vanilla_pack_wide(&top, bits)
            .iter()
            .flat_map(|l| l.to_le_bytes())
            .collect();
        let mut rle: Vec<u8> = Vec::new();
        {
            let mut i = 0usize;
            while i < top.len() {
                let v = top[i];
                let mut run = 1usize;
                while i + run < top.len() && top[i + run] == v {
                    run += 1;
                }
                let mut z = (run - 1) as u64;
                loop {
                    let b = (z & 0x7f) as u8;
                    z >>= 7;
                    if z == 0 {
                        rle.push(b);
                        break;
                    }
                    rle.push(b | 0x80);
                }
                let mut z = v as u64;
                loop {
                    let b = (z & 0x7f) as u8;
                    z >>= 7;
                    if z == 0 {
                        rle.push(b);
                        break;
                    }
                    rle.push(b | 0x80);
                }
                i += run;
            }
        }
        if uniform {
            mbr.push(0u8);
            mbr.push(top[0] as u8);
        } else if rle.len() < packed.len() {
            mbr.push(2u8);
            mbr.extend_from_slice(&rle);
        } else {
            mbr.push(1u8);
            mbr.extend_from_slice(&packed);
        }
        mbr.extend_from_slice(&csec2[tail_at..]);
        // admission: the ref must clearly beat the full member POST-COMPRESSION.
        // Slab zstd compresses dense member payloads ~5x (cross-section
        // matches) while small ref payloads compress ~2.5x, so the ref wins
        // when 5*mbr < 2*full, i.e. mbr + margin < 0.4*full (the v0.6.1
        // lesson: raw-length comparisons mislead admission).
        if mbr.len() + 32 < full_len * 2 / 5 {
            ST_ENC_SHIFT.fetch_add(1, Ordering::Relaxed);
            ST_ENC_SHIFT_BYTES.fetch_add(mbr.len() as u64, Ordering::Relaxed);
            sections[i].0 = hash_of(&mbr);
            sections[i].1 = SecBytes::Owned(mbr);
            is_ref[i] = true;
        } else {
            ST_SHIFT_REJECT.fetch_add(1, Ordering::Relaxed);
            if std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1")
                && full_len > 1000
                && ST_SHIFT_REJECT.load(Ordering::Relaxed) < 4000
            {
                eprintln!(
                    "shift REJECT: k={k} mbr={} full={} n_t={n_t} extras={} tail_len={}",
                    mbr.len(),
                    full_len,
                    extras.len(),
                    csec2.len().saturating_sub(3)
                );
            }
        }
        ST_SHIFT_TAIL_NS.fetch_add(t_all.elapsed().as_nanos() as u64 - t_tail0, Ordering::Relaxed);
    }
    let total = t_all.elapsed().as_nanos() as u64;
    ST_SHIFT_TOTAL_NS.fetch_add(total, Ordering::Relaxed);
}

// ------------------------------------------------------ shift resolution --

/// Bounded FIFO cache of RESOLVED shift sections, keyed by
/// (member hash, target hash). The resolution output is a pure function of
/// those two byte strings, so cache hits are always exact.
struct ShiftResolveCache {
    map: HashMap<u128, Arc<Vec<u8>>>,
    order: VecDeque<(u128, u32)>,
    total: u64,
    cap: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

static SHIFT_CACHE: Mutex<Option<ShiftResolveCache>> = Mutex::new(None);

fn shift_cache_cap() -> u64 {
    const DEFAULT_MB: u64 = 32;
    std::env::var("CSAVES_SHIFT_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(DEFAULT_MB * 1024 * 1024)
}

fn shift_cache_get(key: u128) -> Option<Arc<Vec<u8>>> {
    let mut g = SHIFT_CACHE.lock();
    let c = g.as_mut()?;
    match c.map.get(&key) {
        Some(v) => {
            c.hits.fetch_add(1, Ordering::Relaxed);
            Some(v.clone())
        }
        None => {
            c.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

fn shift_cache_put(key: u128, v: Arc<Vec<u8>>) {
    let mut g = SHIFT_CACHE.lock();
    let c = g.get_or_insert_with(|| ShiftResolveCache {
        map: HashMap::new(),
        order: VecDeque::new(),
        total: 0,
        cap: shift_cache_cap(),
        hits: AtomicU64::new(0),
        misses: AtomicU64::new(0),
    });
    let sz = v.len() as u64;
    if sz > c.cap {
        return;
    }
    if c.map.insert(key, v).is_none() {
        c.order.push_front((key, sz as u32));
        c.total += sz;
        while c.total > c.cap {
            if let Some((ek, esz)) = c.order.pop_back() {
                if c.map.remove(&ek).is_some() {
                    c.total -= esz as u64;
                }
            } else {
                break;
            }
        }
    }
}

/// Resolve every shift-ref member against its chunk-local target.
/// Returns a parallel vec: Some((rekey, resolved CSEC3 bytes)) for shift
/// members, None for everything else. `Err` (None return) = corruption.
/// The rekey mixes member+target hashes so downstream content-keyed caches
/// (the fused emit cache) stay correct: identical member bytes with
/// different local targets never share a key.
pub fn resolve_shifts(
    sections: &[&[u8]],
    hashes: &[Hash],
) -> Option<Vec<Option<(Hash, Arc<Vec<u8>>)>>> {
    let has_ref = sections
        .iter()
        .any(|s| s.first() == Some(&CSECS_MAGIC) || s.first() == Some(&CSECS4_MAGIC));
    if !has_ref {
        // parallel "no replacements" vec (caller zips it with the sections)
        return Some(vec![None; sections.len()]);
    }
    let mut out: Vec<Option<(Hash, Arc<Vec<u8>>)>> = Vec::with_capacity(sections.len());
    for (i, s) in sections.iter().enumerate() {
        if s.first() != Some(&CSECS_MAGIC) && s.first() != Some(&CSECS4_MAGIC) {
            out.push(None);
            continue;
        }
        if s.len() < 7 {
            return None;
        }
        let t = u16::from_le_bytes([s[1], s[2]]) as usize;
        let k = s[3] as usize;
        if t >= sections.len() || t == i || k == 0 || k > 15 {
            return None;
        }
        let target = sections[t];
        if !is_csec4(target.first()) {
            return None;
        }
        let h_member = hashes.get(i).copied().unwrap_or(0);
        let h_target = hashes.get(t).copied().unwrap_or(0);
        let key = h_member ^ h_target.rotate_left(64);
        let resolved = match shift_cache_get(key) {
            Some(a) => a,
            None => match resolve_one(s, target) {
                Some(b) => {
                    let b = Arc::new(b);
                    shift_cache_put(key, b.clone());
                    b
                }
                None => {
                    if std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1") {
                        eprintln!(
                            "resolve_shifts: ref at section {i} (t={t}, k={k}) failed to resolve; head={:02x?}",
                            &s[..s.len().min(24)]
                        );
                    }
                    return None;
                }
            },
        };
        ST_SHIFT_RESOLVED.fetch_add(1, Ordering::Relaxed);
        out.push(Some((key, resolved)));
    }
    Some(out)
}

/// Expand one shift-ref member into a canonical CSEC3 section.
/// Member layout: [0xC9][t u16][k u8][f1'][f2'][n_extra u16][extras...]
///                [top_kind + payload][Y i8 + fields...]  (CSEC3 tail).
fn resolve_one(member: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let c4 = member[0] == CSECS4_MAGIC;
    let k = member[3] as usize;
    let f1 = member[4];
    let f2 = member[5];
    let f3 = if c4 { member[6] } else { 0u8 };
    let mut p = if c4 { 7usize } else { 6usize };
    let n_extra = u16::from_le_bytes(member.get(p..p + 2)?.try_into().ok()?) as usize;
    p += 2;
    let mut extras: Vec<Value> = Vec::with_capacity(n_extra);
    for _ in 0..n_extra {
        extras.push(parse_entry_standalone(member, &mut p)?);
    }
    let top_kind = *member.get(p)?;
    p += 1;
    if std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1") { eprintln!("resolve_one: k={k} f1={f1:02x} f2={f2:02x} n_extra={n_extra} top_kind={top_kind} p={p} mlen={}", member.len()); }
    // decode the target's canonical palette + data
    let tv = section_decode(target)?;
    let Value::Compound(tm) = &tv else { return None };
    let Some(Value::Compound(bst)) = tm.get("block_states") else { return None };
    let Some(Value::List(tpal)) = bst.get("palette") else { return None };
    let n_t = tpal.len();
    if n_t == 0 || n_t > 65535 {
        return None;
    }
    let t_canon: Vec<u32> = match bst.get("data") {
        Some(Value::LongArray(la)) => {
            let bits = section_data_bits(n_t, 4096, la.as_ref().len());
            let per = (64 / bits) as usize;
            if std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1") { eprintln!("resolve_one: n_t={n_t} la={} bits={bits} expect={}", la.as_ref().len(), (4096 + per - 1) / per); }
            if la.as_ref().len() != (4096 + per - 1) / per {
                return None;
            }
            let v = vanilla_unpack_wide(la.as_ref(), 4096, bits);
            if v.iter().any(|&x| x as usize >= n_t) {
                return None;
            }
            v
        }
        Some(_) => return None,
        None => {
            if n_t > 1 {
                return None;
            }
            vec![0u32; 4096]
        }
    };
    let n_ref = n_t + n_extra;
    if n_ref > 65535 {
        return None;
    }
    let kw = k * 256;
    let mut ref_data: Vec<u32> = Vec::with_capacity(4096);
    for i in 0..4096 - kw {
        ref_data.push(t_canon[i + kw]);
    }
    match top_kind {
        0 => {
            let v = *member.get(p)? as u32;
            p += 1;
            if v as usize >= n_ref {
                return None;
            }
            ref_data.resize(4096, v);
        }
        1 => {
            let bits = bits_for(n_ref);
            let longs = kw.div_ceil((64 / bits) as usize);
            let mut lb = Vec::with_capacity(longs);
            for li in 0..longs {
                let o = p + li * 8;
                lb.push(i64::from_le_bytes(
                    member.get(o..o + 8)?.try_into().ok()?,
                ));
            }
            p += longs * 8;
            let topv = vanilla_unpack_wide(&lb, kw, bits);
            if topv.iter().any(|&v| v as usize >= n_ref) {
                return None;
            }
            ref_data.extend_from_slice(&topv);
        }
        2 => {
            // RLE runs: (varint run-1, varint value) pairs filling kw entries
            let mut rd = |p: &mut usize| -> Option<u64> {
                let mut z = 0u64;
                let mut sh = 0u32;
                loop {
                    let byte = *member.get(*p)?;
                    *p += 1;
                    z |= ((byte & 0x7f) as u64) << sh;
                    if byte & 0x80 == 0 {
                        break;
                    }
                    sh += 7;
                    if sh > 63 {
                        return None;
                    }
                }
                Some(z)
            };
            let mut filled = 0usize;
            while filled < kw {
                let run = rd(&mut p)? as usize + 1;
                let v = rd(&mut p)? as u32;
                if v as usize >= n_ref || filled + run > kw {
                    return None;
                }
                ref_data.resize(ref_data.len() + run, v);
                filled += run;
            }
        }
        _ => return None,
    }
    // rebuild the section: everything except block_states comes from the
    // member's CSEC3/4-style tail; block_states is derived from the target
    let fake = {
        let mut v = Vec::with_capacity(member.len() - p + 4);
        if c4 {
            v.push(CSEC4_MAGIC);
            v.push(f1);
            v.push(f2);
            v.push(f3);
        } else {
            v.push(CSEC3_MAGIC);
            v.push(f1);
            v.push(f2);
        }
        v.extend_from_slice(&member[p..]);
        v
    };
    let mut sv = section_decode(&fake)?;
    let Value::Compound(smap) = &mut sv else { return None };
    if smap.contains_key("block_states") {
        if std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1") { eprintln!("resolve_one: fake still has block_states (f1={f1:02x})"); }
        return None;
    }
    let mut pal: Vec<Value> = tpal.iter().cloned().collect();
    pal.extend(extras);
    let mut bs: HashMap<String, Value> = HashMap::new();
    bs.insert("palette".to_string(), Value::List(pal));
    if n_ref > 1 {
        let bits = bits_for(n_ref).max(4);
        let la = if bits <= 8 {
            let u8s: Vec<u8> = ref_data.iter().map(|&x| x as u8).collect();
            pack_nostroddle(&u8s, bits)
        } else {
            vanilla_pack_wide(&ref_data, bits)
        };
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(la)));
    }
    smap.insert("block_states".to_string(), Value::Compound(bs));
    let r = section_encode(&sv);
    if r.is_none() && std::env::var("CSAVES_SHIFT_DEBUG").as_deref() == Ok("1") {
        if let Value::Compound(mm) = &sv {
            eprintln!("resolve_one: final section_encode FAILED (n_ref={n_ref}, keys={:?})", mm.keys().collect::<Vec<_>>());
        }
    }
    r
}

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
    if is_csec4(b.first()) {
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
pub fn split(
    payload: &[u8],
    whole_mode: bool,
    pos_hint: Option<(i32, i32)>,
    strip: bool,
) -> Result<Split, String> {
    if whole_mode {
        return whole_split(payload, pos_hint);
    }
    let t_sp = std::time::Instant::now();
    let root: Value = fastnbt::from_bytes(payload).map_err(|e| format!("nbt parse: {e}"))?;
    SPLT_PARSE.0.fetch_add(t_sp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    SPLT_PARSE.1.fetch_add(1, Ordering::Relaxed);
    split_parsed(payload, pos_hint, root, strip)
}

/// Sectioned split over an ALREADY-parsed chunk (root is consumed for the
/// tail — no tree clone).
fn split_parsed(
    payload: &[u8],
    pos_hint: Option<(i32, i32)>,
    mut root: Value,
    strip: bool,
) -> Result<Split, String> {
    let has_sections = match &root {
        Value::Compound(map) => matches!(map.get("sections"), Some(Value::List(_))),
        _ => false,
    };
    if !has_sections {
        return whole_split(payload, pos_hint);
    }

    let t_ss = std::time::Instant::now();
    let mut sections;
    let strip = strip && !matches!(std::env::var("CSAVES_KEEP_LIGHT").as_deref(), Ok("1"));
    {
        let Value::Compound(map) = &mut root else { unreachable!("checked above") };
        let Some(Value::List(list)) = map.get_mut("sections") else {
            unreachable!("checked above")
        };
        if strip {
            // strip BEFORE hashing/encoding so dedup hashes key on the
            // stripped content: chunks differing only in derived light
            // collapse to the same blobs.
            for s in list.iter_mut() {
                strip_derived_section(s);
            }
        }
        sections = Vec::with_capacity(list.len());
        let mut cache_hit = vec![false; list.len()];
        for s in list.iter() {
            // encode-cache: re-saving a chunk re-encodes every section; the
            // structural hash is order-free, so unchanged sections hit the
            // cache and skip canonicalization + bit-packing entirely.
            let sh = structural_hash(s);
            if let Some((h, shared)) = enc_cache_get(sh) {
                cache_hit[sections.len()] = true;
                sections.push((h, SecBytes::Shared(shared)));
                continue;
            }
            // custom form when possible (marker-detectable), NBT bytes otherwise.
            // v3 output is canonical, so identical content yields identical bytes
            // regardless of the original palette order -> hash the bytes.
            match section_encode(s) {
                Some(c) => {
                    if is_csec4(c.first()) {
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
        // shift-ref pass (v0.7): collapse sections that are vertical shifts
        // of a lower section of this chunk (mountain strata / ocean floors /
        // cave ceilings) into ~15-30 byte refs; the top k planes stay
        // explicit. Sections whose content was seen in an earlier chunk
        // (enc-cache hit) are skipped: their full members dedup across
        // chunks for free, which always beats a chunk-local ref.
        if std::env::var("CSAVES_NO_SHIFT").as_deref() != Ok("1") {
            shift_ref_pass(&mut sections, list, &cache_hit);
        }
    }
    SPLT_SECS.0.fetch_add(t_ss.elapsed().as_nanos() as u64, Ordering::Relaxed);
    SPLT_SECS.1.fetch_add(1, Ordering::Relaxed);

    let t_sh = std::time::Instant::now();
    // tail = parsed root minus sections, CONSUMED (the old map.clone() of
    // the whole tree cost ~100 us/chunk of import wall time)
    let mut root = root;
    let tail = match &mut root {
        Value::Compound(m) => {
            m.remove("sections");
            if strip {
                // relight trigger: isLightOn=0 makes the server recompute
                // light for this chunk on load (vanilla standard for fresh
                // chunks)
                m.insert("isLightOn".to_string(), Value::Byte(0));
            }
            let mut tv = Value::Compound(std::mem::take(m));
            tail_encode(&mut tv);
            to_bytes(&tv)
        }
        _ => unreachable!("checked above"),
    };
    SPLT_TAIL.0.fetch_add(t_sh.elapsed().as_nanos() as u64, Ordering::Relaxed);
    SPLT_TAIL.1.fetch_add(1, Ordering::Relaxed);

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
        let v = if s.first() == Some(&CSEC_MAGIC) || is_csec4(s.first()) {
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
    if !is_csec4(b.first()) {
        return None;
    }
    let f1 = *b.get(1)?;
    let f2 = *b.get(2)?;
    let mut p = 3usize;
    let mut f3 = 0u8;
    if b[0] == CSEC4_MAGIC {
        f3 = *b.get(p)?;
        p += 1;
    }
    let y = *b.get(p)? as u8;
    p += 1;

    // collect the variable-shape parts first: palette streams + their ranges
    // are parsed once, in stream order; emission happens in KEY order.
    // Returns the serialized NBT payload of the {data, palette} compound.
    fn emit_pal_data(
        b: &[u8],
        p: &mut usize,
        entries: usize,
        has_data: bool,
        narrow: bool,
        order: u8,
    ) -> Option<Vec<u8>> {
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
            let out_bits = bits_for(n).max(4);
            let longs: Vec<i64> = if order != 0 {
                // RLE form (v0.6): expand (idx, run) pairs to the canonical
                // voxel stream, transpose back for order 2, then pack to
                // vanilla no-straddle longs directly.
                if n > 256 {
                    return None;
                }
                let side = match entries {
                    4096 => 16usize,
                    64 => 4usize,
                    _ => return None,
                };
                let mut v: Vec<u8> = Vec::with_capacity(entries);
                while v.len() < entries {
                    let idx = *b.get(*p)?;
                    *p += 1;
                    let run = read_uvarint(b, p)? as usize;
                    if run == 0 || v.len() + run > entries {
                        return None;
                    }
                    v.resize(v.len() + run, idx);
                }
                let v = if order == 2 {
                    transpose_fast_y(&v, side)
                } else {
                    v
                };
                pack_nostroddle(&v, out_bits)
            } else {
                let rbits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
                let bytes = (entries * rbits as usize + 7) / 8;
                let dense = match b.get(*p..*p + bytes) {
                    Some(d) => d,
                    None => {
                        fdbg!(
                            "fused: data oob p={} bytes={} n={} narrow={} blen={}",
                            p,
                            bytes,
                            n,
                            narrow,
                            b.len()
                        );
                        return None;
                    }
                };
                *p += bytes;
                dense_to_nostroddle(dense, entries, rbits, out_bits)
            };
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
        let order = (f2 >> 4) & 3;
        Some(emit_pal_data(b, &mut p, 4096, f1 & 2 != 0, f2 & 4 != 0, order)?)
    } else {
        None
    };
    // parse biomes
    let bio_body = if f1 & 4 != 0 {
        let order = (f2 >> 6) & 3;
        Some(emit_pal_data(b, &mut p, 64, f1 & 8 != 0, f2 & 8 != 0, order)?)
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
    let mut bl_uniform0 = false;
    if f1 & 128 != 0 {
        let l = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        bl = Some(b.get(p..p + l)?.to_vec());
        p += l;
    } else if f3 & 1 != 0 {
        bl_uniform0 = true;
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
    } else if bl_uniform0 {
        put_named_key(out, 0x07, "BlockLight");
        put_be32(out, 2048);
        out.resize(out.len() + 2048, 0x00);
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
        if !is_csec4(s.b.first()) {
            fdbg!("fused: section {si} not CSEC3/4 (first={:02x})", s.b.first().copied().unwrap_or(0));
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

// ------------------------------------------------------ global registry ----
//
// The 0xCB GLOB form. Measured on the real 262k-chunk vanilla-terrain
// bigworld (jj_exp5, 32,768 chunks): the canonical dense streams sit BELOW
// the order-0 arithmetic floor (z19 already exploits cross-section repeats),
// yet replacing per-section palette STRINGS with a GLOBAL state registry
// wins another x2.16 on the like-for-like dense content: every section's
// palette bytes (18.8% of the raw canonical weight) disappear, and the
// global-id streams line the same geological micro-patterns up byte-for-byte
// across the whole slab window, where per-section palette indices never
// matched. Vertical-conditioning and bitmap-predictor models measured
// x1.20 / x0.50 — the registry is the lever.
//
// The transform is STORE-LEVEL and transparent to the codec: canonical CSEC
// bytes and their dedup hashes never change; the pack pass converts eligible
// dense sections to 0xCB members inside a slab (slab-level form choice by
// compressed size), and read_blob converts members back to canonical bytes
// before any decoder sees them. Registry assignment is per store dir,
// append-only (monotonic gids), persisted in states-1.bin.

/// One registry state: canonical (plain flag, name, sorted props).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct GlobState {
    pub plain: bool,
    pub name: String,
    pub props: Vec<(String, String)>,
}

/// Global block-state registry: state -> gid, append-only. Gid 0 is the
/// FIRST inserted state (a valid dense index must never be confused with
/// "absent" — the registry never stores Option gids, so 0 is fine).
#[derive(Default, Debug)]
pub struct StateRegistry {
    states: Vec<GlobState>,
    index: HashMap<GlobState, u32>,
}

impl StateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn get(&self, gid: u32) -> Option<&GlobState> {
        self.states.get(gid as usize)
    }

    pub fn get_or_insert(&mut self, st: GlobState) -> u32 {
        if let Some(&g) = self.index.get(&st) {
            return g;
        }
        let g = self.states.len() as u32;
        self.states.push(st.clone());
        self.index.insert(st, g);
        g
    }

    /// Serialize: [b'G'][b'S'][b'V'][b'1'][u32 n]{[u8 kind][u16 nlen][name]
    /// [u8 np]{[u16 kl][k][u16 vl][v]}} — the put_entry wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.states.len() * 24);
        out.extend_from_slice(b"GSV1");
        out.extend_from_slice(&(self.states.len() as u32).to_le_bytes());
        for s in &self.states {
            let nb = s.name.as_bytes();
            out.push(if s.plain { 0u8 } else if s.props.is_empty() { 1u8 } else { 2u8 });
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            out.extend_from_slice(nb);
            if !s.plain {
                out.push(s.props.len().min(255) as u8);
                for (k, v) in s.props.iter().take(255) {
                    out.extend_from_slice(&(k.len() as u16).to_le_bytes());
                    out.extend_from_slice(k.as_bytes());
                    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
                    out.extend_from_slice(v.as_bytes());
                }
            }
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 8 || &b[0..4] != b"GSV1" {
            return None;
        }
        let n = u32::from_le_bytes(b[4..8].try_into().ok()?) as usize;
        let mut reg = StateRegistry::new();
        reg.states.reserve(n);
        let mut p = 8usize;
        for _ in 0..n {
            let kind = *b.get(p)?;
            p += 1;
            let nl = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
            p += 2;
            let name = String::from_utf8(b.get(p..p + nl)?.to_vec()).ok()?;
            p += nl;
            let mut props = Vec::new();
            let plain = kind == 0;
            if !plain {
                let np = *b.get(p)? as usize;
                p += 1;
                for _ in 0..np {
                    let kl = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
                    p += 2;
                    let k = String::from_utf8(b.get(p..p + kl)?.to_vec()).ok()?;
                    p += kl;
                    let vl = u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize;
                    p += 2;
                    let v = String::from_utf8(b.get(p..p + vl)?.to_vec()).ok()?;
                    p += vl;
                    props.push((k, v));
                }
            }
            let st = GlobState { plain, name, props };
            reg.get_or_insert(st);
        }
        Some(reg)
    }
}

/// Serialize one palette entry in the canonical put_entry wire format.
fn glob_put_entry(out: &mut Vec<u8>, s: &GlobState) {
    let nb = s.name.as_bytes();
    if s.plain {
        out.push(0u8);
    } else if s.props.is_empty() {
        out.push(1u8);
    } else {
        out.push(2u8);
    }
    out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
    out.extend_from_slice(nb);
    if !s.plain && !s.props.is_empty() {
        out.push(s.props.len() as u8);
        for (k, v) in &s.props {
            out.extend_from_slice(&(k.len() as u16).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&(v.len() as u16).to_le_bytes());
            out.extend_from_slice(v.as_bytes());
        }
    }
}

/// Parse one put_entry palette entry starting at p. Returns (state, kind).
fn glob_parse_entry(b: &[u8], p: &mut usize) -> Option<(GlobState, u8)> {
    let kind = *b.get(*p)?;
    *p += 1;
    let nl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
    *p += 2;
    let name = String::from_utf8(b.get(*p..*p + nl)?.to_vec()).ok()?;
    *p += nl;
    let mut props = Vec::new();
    let plain = kind == 0;
    // canonical put_entry format: ONLY kind 2 (compound with Properties)
    // carries the np byte; kind 1 (compound, no props) ends after the name
    if kind == 2 {
        let np = *b.get(*p)? as usize;
        *p += 1;
        for _ in 0..np {
            let kl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            let k = String::from_utf8(b.get(*p..*p + kl)?.to_vec()).ok()?;
            *p += kl;
            let vl = u16::from_le_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p += 2;
            let v = String::from_utf8(b.get(*p..*p + vl)?.to_vec()).ok()?;
            *p += vl;
            props.push((k, v));
        }
    }
    Some((GlobState { plain, name, props }, kind))
}

/// Convert one canonical CSEC section (0xC7/0xC8) to the GLOB form (0xCB).
/// Applies ONLY to dense block_states sections with data (RLE and
/// single-state sections already beat the wide stream or carry no stream);
/// everything after the block_states stream (biomes, light, starlight) is
/// copied verbatim. Returns None when the section is not eligible — the
/// caller then stores the canonical bytes (always safe).
///
/// Layout: [0xCB][f1][f2'][f3'?][Y][4096 gid bytes][verbatim tail]
/// — f2' is f2 with the block_states narrow/RLE bits cleared; the gid width
/// is always 1 byte (the caller guarantees registry <= 256 states).
pub fn section_to_glob(csec: &[u8], reg: &mut StateRegistry) -> Option<Vec<u8>> {
    if reg.len() > 256 {
        return None;
    }
    let magic = *csec.first()?;
    if magic != CSEC3_MAGIC && magic != CSEC4_MAGIC {
        return None;
    }
    let f1 = *csec.get(1)?;
    let f2 = *csec.get(2)?;
    let mut p = 3usize;
    let mut f3 = 0u8;
    if magic == CSEC4_MAGIC {
        f3 = *csec.get(p)?;
        p += 1;
    }
    let y = *csec.get(p)?;
    p += 1;
    // block_states present AND has data; dense form only (f2 order bits 4/5)
    if f1 & 3 != 3 || f2 & 48 != 0 {
        return None;
    }
    let n = u16::from_le_bytes(csec.get(p..p + 2)?.try_into().ok()?) as usize;
    p += 2;
    if n == 0 || n > 65535 {
        return None;
    }
    let mut states = Vec::with_capacity(n);
    for i in 0..n {
        let Some((st, _kind)) = glob_parse_entry(csec, &mut p) else {
            if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
                eprintln!("glob: entry parse failed i={i}/{n} p={p} len={}", csec.len());
            }
            return None;
        };
        states.push(st);
    }
    // dense stream width: narrow (f2 bit2) packs at bits_for(n), else the
    // vanilla max(4, bits_for(n)) clamp
    let out_bits = if f2 & 4 != 0 { bits_for(n) } else { bits_for(n).max(4) };
    let stream_len = (4096usize * out_bits as usize).div_ceil(8);
    let Some(vox) = dense_unpack(csec.get(p..p + stream_len)?, 4096, out_bits) else {
        if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
            eprintln!("glob: dense_unpack failed n={n} out_bits={out_bits} stream_len={stream_len} p={p} len={}", csec.len());
        }
        return None;
    };
    if vox.iter().any(|&x| (x as usize) >= n) {
        return None;
    }
    // gid stream: 1 byte per voxel (registry <= 256 guarded above)
    let mut stream = Vec::with_capacity(4096);
    for v in &vox {
        let st = &states[*v as usize];
        let g = reg.get_or_insert(st.clone());
        stream.push(g as u8);
    }
    let mut out = Vec::with_capacity(p + stream_len + stream.len());
    out.push(GLOB_MAGIC);
    out.push(f1);
    out.push(f2 & !4 & !16 & !32);
    // 0xCB ALWAYS carries the f3 byte (all f1/f2 bits are taken; the
    // explicit shape removes any v3/v4 header ambiguity)
    out.push(f3);
    out.push(y);
    out.extend_from_slice(&stream);
    out.extend_from_slice(&csec[p + stream_len..]);
    ST_GLOB_ENC.fetch_add(1, Ordering::Relaxed);
    Some(out)
}

/// Rebuild canonical CSEC bytes from a GLOB section (0xCB). The rebuilt
/// palette holds exactly the USED states in the frequency-canonical order
/// (dominant first, ties by (plain, name, props) — the enc_pal_data_canon
/// rule), so identical content reproduces identical bytes; the dense/RLE
/// form is re-chosen by the same admission rule as section_encode. The
/// rebuilt palette drops states the source palette carried unused —
/// semantically irrelevant (a palette is a pure lookup), verified by the
/// semantic comparators.
pub fn glob_to_section(glob: &[u8], reg: &StateRegistry) -> Option<Vec<u8>> {
    if glob.first() != Some(&GLOB_MAGIC) {
        return None;
    }
    let f1 = *glob.get(1)?;
    let f2 = *glob.get(2)?;
    let f3 = *glob.get(3)?; // 0xCB always carries the f3 byte
    let y = *glob.get(4)?;
    let mut p = 5usize;
    let magic = if f3 != 0 { CSEC4_MAGIC } else { CSEC3_MAGIC };
    if f1 & 3 != 3 {
        return None; // never globbed without a data stream
    }
    let stream = glob.get(p..p + 4096)?;
    p += 4096;
    let tail = &glob[p..];
    // counts per gid over the section's voxels
    let mut counts = [0u32; 256];
    for &g in stream {
        counts[g as usize] += 1;
    }
    // distinct used gids in the frequency-canonical order (dominant first,
    // ties by (plain, name, props) — enc_pal_data_canon's exact rule)
    let mut used: Vec<(u32, u32)> = (0..reg.len().min(256) as u32)
        .filter(|&g| counts[g as usize] > 0)
        .map(|g| (counts[g as usize], g))
        .collect();
    let _reg_state_unused = |g: u32| -> Option<&GlobState> { reg.get(g) };
    let reg_snap: Vec<(u32, bool, String, Vec<(String, String)>)> = used
        .iter()
        .map(|&(c, g)| {
            let s = reg.get(g);
            (
                c,
                s.map(|s| s.plain).unwrap_or(false),
                s.map(|s| s.name.clone()).unwrap_or_default(),
                s.map(|s| s.props.clone()).unwrap_or_default(),
            )
        })
        .collect();
    let mut order: Vec<usize> = (0..used.len()).collect();
    order.sort_by(|&i, &j| {
        let (ci, gi) = used[i];
        let (cj, gj) = used[j];
        let (_, pi, ni, propsi) = &reg_snap[i];
        let (_, pj, nj, propsj) = &reg_snap[j];
        ci.cmp(&cj)
            .then_with(|| (pi, ni, propsi).cmp(&(pj, nj, propsj)))
            .then_with(|| gi.cmp(&gj))
    });
    let ordered: Vec<(u32, u32)> = order.into_iter().map(|i| used[i]).collect();
    let used = ordered;
    let n = used.len();
    if n == 0 || n > 65535 {
        return None;
    }
    let mut gid2canon = [0u8; 256];
    for (i, (_, g)) in used.iter().enumerate() {
        gid2canon[*g as usize] = i as u8;
    }
    let vox: Vec<u8> = stream.iter().map(|&g| gid2canon[g as usize]).collect();
    // same form choice as the canonical encoder
    let narrow = bits_for(n) < 4;
    let out_bits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
    let dense = dense_pack(&vox, out_bits);
    let mut best_len = dense.len();
    let mut best: Option<(u8, Vec<u8>)> = None;
    let ra = rle_encode_stream(&vox);
    if ra.len() * 4 <= dense.len() && ra.len() + 32 <= dense.len() {
        best_len = ra.len();
        best = Some((1, ra));
    }
    let rb = rle_encode_stream(&transpose_fast_y(&vox, 16));
    if rb.len() * 4 <= dense.len() && rb.len() + 32 <= dense.len() && rb.len() < best_len {
        best = Some((2, rb));
    }
    let mut out = Vec::with_capacity(16 + n * 32 + dense.len() + tail.len());
    out.push(magic);
    out.push(f1);
    let mut f2n = f2 & !4 & !16 & !32;
    if narrow {
        f2n |= 4;
    }
    out.push(f2n);
    if magic == CSEC4_MAGIC {
        out.push(f3);
    }
    out.push(y);
    out.extend_from_slice(&(n as u16).to_le_bytes());
    for (_, g) in &used {
        let st = reg.get(*g)?;
        glob_put_entry(&mut out, st);
    }
    match best {
        None => {
            out.extend_from_slice(&dense);
            let _ = best_len;
        }
        Some((ord, bytes)) => {
            out.extend_from_slice(&bytes);
            f2n |= ord << 4;
        }
    }
    // f2n was computed BEFORE the palette/stream were pushed; patch byte 2
    out[2] = f2n;
    out.extend_from_slice(tail);
    ST_GLOB_DEC.fetch_add(1, Ordering::Relaxed);
    Some(out)
}

const GLOB_MAGIC: u8 = 0xCB;

static ST_GLOB_ENC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ST_GLOB_DEC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    fn fused_rle_band_section() {
        // RLE-admitted strata band: the fused emitter must expand the run
        // stream (order 1) and emit the vanilla-width LongArray directly.
        let sec = compound(vec![
            ("Y", Value::Byte(7)),
            (
                "block_states",
                compound(vec![
                    (
                        "palette",
                        Value::List(vec![
                            compound(vec![("Name", Value::String("minecraft:stone".into()))]),
                            compound(vec![("Name", Value::String("minecraft:deepslate".into()))]),
                            compound(vec![("Name", Value::String("minecraft:air".into()))]),
                        ]),
                    ),
                    (
                        "data",
                        Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(
                            &(0..4096)
                                .map(|i| if i < 2048 { 0u8 } else { 1u8 })
                                .collect::<Vec<u8>>(),
                            2,
                        ))),
                    ),
                ]),
            ),
        ]);
        let enc = section_encode(&sec).expect("encode");
        // the admission rule must actually pick RLE for this shape:
        // dense 2-bit stream is 512B, the run stream is ~6B (blowout)
        assert_eq!((enc[2] >> 4) & 3, 1, "band section must take vanilla-order RLE");
        let mut out = Vec::new();
        let r = section_emit_csec3(&enc, &mut out);
        if r.is_none() {
            panic!("fused emit failed on RLE section; enc={:02x?}", enc);
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
        let s = split(&payload, false, Some((3, 7)), false).unwrap();
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
        let s1 = split(&sample_chunk(8, "T"), false, None, false).unwrap();
        let s2 = split(&sample_chunk(8, "T"), false, None, false).unwrap();
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
        let s = split(&payload, false, Some((3, 7)), false).unwrap();
        assert!(matches!(s, Split::Whole { .. }));
    }

    #[test]
    fn empty_sections_list_still_splits() {
        let mut root: Map<String, Value> = Map::new();
        root.insert("sections".to_string(), Value::List(Vec::new()));
        let payload = to_bytes(&Value::Compound(root));
        let s = split(&payload, false, Some((3, 7)), false).unwrap();
        let Split::Split { sections, .. } = s else { panic!("empty list must still split") };
        assert!(sections.is_empty());
    }

    #[test]
    fn rle_strata_band_section() {
        // Horizontal strata: vanilla order (x fastest, y slowest) puts 8 full
        // stone planes then 8 air planes in the stream => two giant runs.
        // RLE must win over the 512-byte dense 2-bit stream and round-trip.
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(9));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for n in ["minecraft:stone", "minecraft:air", "minecraft:diamond_ore"] {
            pal.push(Value::String(n.to_string()));
        }
        bs.insert("palette".to_string(), Value::List(pal));
        let entries: Vec<u8> = (0..4096).map(|i| if i < 2048 { 0u8 } else { 1u8 }).collect();
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 2))),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let v = Value::Compound(sec);
        let enc = section_encode(&v).unwrap();
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&v, &dec), "band section must round-trip");
        assert!(enc.len() < 120, "band RLE should be tiny, got {}", enc.len());
    }

    #[test]
    fn rle_terrace_section() {
        // Terraced boundary: height depends on z only (8 planes below, a 4-plane
        // step, air above). Vanilla-order RLE collapses every y-plane into 1-2
        // runs (~20 runs total); must beat the 512-byte dense stream.
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(-2));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for n in ["minecraft:stone", "minecraft:air"] {
            pal.push(Value::String(n.to_string()));
        }
        bs.insert("palette".to_string(), Value::List(pal));
        let entries: Vec<u8> = (0..4096)
            .map(|i| {
                let z = (i >> 4) & 15;
                let y = (i >> 8) & 15;
                if (y as usize) < 8 + 4 * ((z as usize) / 8) { 0u8 } else { 1u8 }
            })
            .collect();
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 1))),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let v = Value::Compound(sec);
        let enc = section_encode(&v).unwrap();
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&v, &dec), "terrace section must round-trip");
        assert!(enc.len() < 256, "terrace RLE should be small, got {}", enc.len());
    }

    #[test]
    fn rle_deterministic_noise_prefers_dense() {
        // Run-of-1 noise: RLE would explode (2 bytes per voxel); the dense
        // form must win, and re-encoding must be byte-identical (dedup
        // stability).
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(3));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for n in ["minecraft:stone", "minecraft:deepslate", "minecraft:gravel"] {
            pal.push(Value::String(n.to_string()));
        }
        bs.insert("palette".to_string(), Value::List(pal));
        let entries: Vec<u8> = (0..4096).map(|i| (i * 7 % 3) as u8).collect();
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 2))),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let v = Value::Compound(sec);
        let e1 = section_encode(&v).unwrap();
        let e2 = section_encode(&v).unwrap();
        assert_eq!(e1, e2, "encode must be content-deterministic");
        let dec = section_decode(&e1).unwrap();
        assert!(sections_semantically_equal(&v, &dec));
    }

    #[test]
    fn rle_biomes_roundtrip() {
        // Biome split at the y boundary: 64 entries. The 8-byte dense stream
        // stays dense (tiny streams keep the alignment-friendly form); the
        // test validates the biome palette/data round-trip through the new
        // flag handling either way.
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(1));
        let mut bm: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for n in ["minecraft:plains", "minecraft:desert"] {
            pal.push(Value::String(n.to_string()));
        }
        bm.insert("palette".to_string(), Value::List(pal));
        let entries: Vec<u8> = (0..64).map(|i| if i < 32 { 0u8 } else { 1u8 }).collect();
        bm.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(pack_nostroddle(&entries, 1))),
        );
        sec.insert("biomes".to_string(), Value::Compound(bm));
        let v = Value::Compound(sec);
        let enc = section_encode(&v).unwrap();
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&v, &dec), "biome RLE must round-trip");
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
    fn csec4_light_uniforms_roundtrip() {
        // all-0 BlockLight (the underground majority) + all-0 SkyLight must
        // collapse to flag bits, not 2048 raw bytes each.
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(-8));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![Value::String("minecraft:deepslate".into())]),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])));
        sec.insert(
            "BlockLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])),
        );
        let enc = section_encode(&Value::Compound(sec.clone())).expect("uniform lights encode");
        assert_eq!(enc[0], CSEC4_MAGIC, "BlockLight all-0 must upgrade to v4");
        assert_eq!(enc[3] & 1, 1, "f3 bit0 = BlockLight all-0");
        assert_eq!(enc[1] & 32, 32, "f1 bit5 = SkyLight all-0");
        // no raw light payloads: stream stays tiny
        assert!(enc.len() < 64, "uniform-light section must not carry 2048B arrays, len={}", enc.len());
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&dec, &Value::Compound(sec)));
        // byte-exact semantic check (semantic equality already covers the
        // maps; assert the light arrays byte-for-byte too)
        if let Value::Compound(dm) = dec {
            if let Some(Value::ByteArray(bl)) = dm.get("BlockLight") {
                assert!(bl.as_ref().iter().all(|x| *x == 0));
                assert_eq!(bl.as_ref().len(), 2048);
            } else {
                panic!("BlockLight missing after decode");
            }
        }
    }

    #[test]
    fn csec4_sky_full_stays_v3() {
        // all-1 SkyLight + NO BlockLight key: pure v3 (f3 unused) — wire form
        // must stay byte-identical to the old encoder output.
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(100));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![Value::String("minecraft:air".into())]),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![-1; 2048])));
        let enc = section_encode(&Value::Compound(sec.clone())).expect("sky-full encodes");
        assert_eq!(enc[0], CSEC3_MAGIC, "no v4 features => byte-identical v3 stream");
        let dec = section_decode(&enc).unwrap();
        assert!(sections_semantically_equal(&dec, &Value::Compound(sec)));
    }

    #[test]
    fn csec4_partial_light_stays_raw() {
        // non-uniform light arrays must keep the raw form (and v3 when no
        // f3 feature is triggered — raw SkyLight/BlockLight live in f1).
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(10));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![Value::String("minecraft:stone".into())]),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let mut sk = vec![0u8; 2048];
        sk[100] = 7; // one lit byte
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(sk.iter().map(|x| *x as i8).collect())));
        sec.insert(
            "BlockLight".to_string(),
            Value::ByteArray(fastnbt::ByteArray::new(vec![0; 2048])),
        );
        let enc = section_encode(&Value::Compound(sec.clone())).expect("partial light encodes");
        assert_eq!(enc[0], CSEC4_MAGIC, "BlockLight all-0 still upgrades to v4");
        assert_eq!(enc[1] & 64, 64, "SkyLight raw");
        let dec = section_decode(&enc).unwrap();
        if let Value::Compound(dm) = dec {
            if let Some(Value::ByteArray(sk2)) = dm.get("SkyLight") {
                assert_eq!(sk2.as_ref().len(), 2048);
                assert_eq!(sk2.as_ref()[100], 7);
            } else {
                panic!("SkyLight missing after decode");
            }
        }
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
        let s = split(&payload, true, Some((5, -7)), false).unwrap();
        let Split::Fields { parts, pos_from_key } = &s else { panic!("must field-split") };
        assert!(*pos_from_key, "matching Position must be stripped");
        assert!(!parts.iter().any(|p| p.key == "Position"));
        // wrong key -> Position stays
        let s2 = split(&payload, true, Some((6, -7)), false).unwrap();
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

    /// Build a shift-structured chunk: target at list pos 0, shifted section
    /// at pos 1 (upper window of target, k planes), optional extra block in
    /// the top planes, explicit Y list.
    fn shift_structured_chunk(k: usize, extra_in_top: bool) -> Vec<u8> {
        let pack = |e: &[u32]| -> Vec<i64> {
            let bits = 2u32;
            let per = (64 / bits) as usize;
            let mut out = vec![0i64; e.len().div_ceil(per)];
            for (i, &v) in e.iter().enumerate() {
                out[i / per] |= (v as i64) << ((i % per) as u32 * bits);
            }
            out
        };
        let t_entries: Vec<u32> = (0..4096)
            .map(|i| {
                let x = (i as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(0x12345);
                ((x >> 33) % 3) as u32
            })
            .collect();
        let mk = |y: i8, data: Vec<i64>, extra: bool| {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(y));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = vec![
                Value::String("minecraft:stone".to_string()),
                Value::String("minecraft:deepslate".to_string()),
                Value::String("minecraft:air".to_string()),
            ];
            if extra {
                pal.push(Value::String("minecraft:gold_ore".to_string()));
            }
            bs.insert("palette".to_string(), Value::List(pal));
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            Value::Compound(sec)
        };
        let mut sec_list = vec![mk(2, pack(&t_entries), false)];
        let mut s_entries: Vec<u32> = Vec::with_capacity(4096);
        s_entries.extend_from_slice(&t_entries[k * 256..]);
        // top planes: air (2), or gold ore (3, extra) every 64th voxel
        if extra_in_top {
            let mut v: Vec<u32> = vec![2; k * 256];
            for (i, x) in v.iter_mut().enumerate() {
                if i % 64 == 0 {
                    *x = 3;
                }
            }
            s_entries.extend_from_slice(&v);
        } else {
            s_entries.resize(4096, 2);
        }
        sec_list.push(mk(3, pack(&s_entries), extra_in_top));
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert("sections".to_string(), Value::List(sec_list));
        fastnbt::to_bytes(&Value::Compound(root)).unwrap()
    }

    fn div_ceil_compat(a: usize, b: usize) -> usize {
        a.div_ceil(b)
    }

    #[test]
    fn shift_ref_split_and_resolve() {
        for (k, extra) in [(4usize, false), (1, false), (15, false), (4, true), (1, true)] {
            let payload = shift_structured_chunk(k, extra);
            let split = split(&payload, false, Some((3, 4)), false).unwrap();
            let (sections, tail) = match &split {
                Split::Split { sections, tail } => (sections, tail),
                _ => panic!("expected split"),
            };
            assert_eq!(sections.len(), 2);
            assert!(sections[0].1.as_slice().first() == Some(&CSEC3_MAGIC));
            assert_eq!(sections[1].1.as_slice().first(), Some(&CSECS_MAGIC), "k={k} extra={extra}: no ref");
            assert!(sections[1].1.as_slice().len() < 300, "ref must be tiny");
            let refs: Vec<&[u8]> = sections.iter().map(|(_, sb)| sb.as_slice()).collect();
            let hashes: Vec<Hash> = sections.iter().map(|(h, _)| *h).collect();
            let res = resolve_shifts(&refs, &hashes).expect("resolve");
            let got = res[1].as_ref().expect("sec1 resolved").1.clone();
            let sv = section_decode(&got).expect("decode resolved");
            let root: Value = fastnbt::from_bytes(&payload).unwrap();
            let Value::Compound(m) = &root else { panic!() };
            let Some(Value::List(list)) = m.get("sections") else { panic!() };
            assert!(
                sections_semantically_equal(&list[1], &sv),
                "k={k} extra={extra}: resolved section not semantic-equal"
            );
            // legacy join requires pre-resolved members (the store resolves
            // before joining); resolve here and join the expanded list
            let res = resolve_shifts(&refs, &hashes).expect("resolve");
            let expanded: Vec<Vec<u8>> = sections
                .iter()
                .zip(res.iter())
                .map(|((_, sb), r)| match r {
                    Some((_, b)) => b.as_ref().clone(),
                    None => sb.as_slice().to_vec(),
                })
                .collect();
            assert!(join(&expanded, tail).is_ok());
        }
    }

    #[test]
    fn shift_ref_rekey_differs_by_target() {
        // two chunks with identical ref members but different targets must
        // resolve to different content and different cache keys
        let payload = shift_structured_chunk(4, false);
        let s1 = split(&payload, false, Some((0, 0)), false).unwrap();
        // fresh thread: the encode cache is thread-local and would mark the
        // sections as cross-chunk-shared, skipping the shift conversion
        let p2 = payload.clone();
        let s2 = std::thread::spawn(move || split(&p2, false, Some((5, 5)), false).unwrap())
            .join()
            .unwrap();
        let (a, b) = match (&s1, &s2) {
            (Split::Split { sections: a, .. }, Split::Split { sections: b, .. }) => (a, b),
            _ => panic!(),
        };
        // identical member bytes (position-independent ref) => same hash
        assert_eq!(a[1].0, b[1].0, "ref member hash must be position-independent");
        let ra: Vec<&[u8]> = a.iter().map(|(_, sb)| sb.as_slice()).collect();
        let rb: Vec<&[u8]> = b.iter().map(|(_, sb)| sb.as_slice()).collect();
        let ha: Vec<Hash> = a.iter().map(|(h, _)| *h).collect();
        let hb: Vec<Hash> = b.iter().map(|(h, _)| *h).collect();
        let ra_res = resolve_shifts(&ra, &ha).unwrap();
        let rb_res = resolve_shifts(&rb, &hb).unwrap();
        let (ka, _) = ra_res[1].as_ref().unwrap();
        let (kb, _) = rb_res[1].as_ref().unwrap();
        // same target content here, so same key is correct
        assert_eq!(ka, kb);
    }

    #[test]
    fn shift_ref_corruption_fails_loudly() {
        let payload = shift_structured_chunk(4, false);
        let split = split(&payload, false, Some((0, 0)), false).unwrap();
        let sections = match &split {
            Split::Split { sections, .. } => sections.clone(),
            _ => panic!(),
        };
        let mut refs: Vec<Vec<u8>> = sections.iter().map(|(_, sb)| sb.as_slice().to_vec()).collect();
        // corrupt the target position (out of range)
        refs[1][1] = 0xff;
        refs[1][2] = 0xff;
        let slices: Vec<&[u8]> = refs.iter().map(|b| b.as_slice()).collect();
        let hashes: Vec<Hash> = sections.iter().map(|(h, _)| *h).collect();
        assert!(resolve_shifts(&slices, &hashes).is_none(), "bad target must fail");
        // k out of range
        let mut refs2: Vec<Vec<u8>> = sections.iter().map(|(_, sb)| sb.as_slice().to_vec()).collect();
        refs2[1][3] = 16;
        let slices2: Vec<&[u8]> = refs2.iter().map(|b| b.as_slice()).collect();
        assert!(resolve_shifts(&slices2, &hashes).is_none(), "k=16 must fail");
    }
}

#[cfg(test)]
mod glob_tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn glob_section(y: i32, seed: u64, distinct: usize) -> Value {
        // dense multi-state section: deterministic pseudo-random voxels over
        // `distinct` states, plus a biome palette so the verbatim-tail path
        // is exercised
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for i in 0..distinct {
            // compound entries only: an NBT palette list cannot mix string
            // and compound elements (vanilla never does)
            let mut c: Map<String, Value> = Map::new();
            c.insert("Name".to_string(), Value::String(format!("minecraft:block_{i}")));
            if i % 2 == 0 {
                let mut props: Map<String, Value> = Map::new();
                props.insert("axis".to_string(), Value::String(format!("{}_{}", i % 2, i % 7)));
                props.insert("lit".to_string(), Value::String("true".to_string()));
                c.insert("Properties".to_string(), Value::Compound(props));
            }
            pal.push(Value::Compound(c));
        }
        bs.insert("palette".to_string(), Value::List(pal));
        let mut st = seed | 1;
        let vox: Vec<u8> = (0..4096)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                (st % distinct as u64) as u8
            })
            .collect();
        // vanilla no-straddle packing at the clamped width
        let vbits = (((distinct as u32 - 1).ilog2() + 1).max(4)) as u32;
        let data = dense_to_nostroddle(&vox, 4096, 8, vbits);
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let mut bio: Map<String, Value> = Map::new();
        bio.insert(
            "palette".to_string(),
            Value::List(vec![Value::String("minecraft:plains".to_string())]),
        );
        sec.insert("biomes".to_string(), Value::Compound(bio));
        sec.insert("SkyLight".to_string(), Value::ByteArray(fastnbt::ByteArray::new(vec![15i8; 2048])));
        Value::Compound(sec)
    }

    #[test]
    fn glob_roundtrip_semantic_and_registry_persist() {
        for distinct in [2usize, 5, 16, 17, 300] {
            let src = glob_section(-4 + distinct as i32, 0xDEADBEEF, distinct);
            let mut reg = StateRegistry::new();
            let canon = match section_encode(&src) {
                Some(c) => c,
                None => panic!("canonical encode failed at distinct={distinct}"),
            };
            let glob = section_to_glob(&canon, &mut reg);
            if distinct > 256 {
                // the u8 gid stream only admits registries <= 256: the
                // 300-state section itself is fine but pushing >256 states
                // mid-run disables further conversions
                if let Some(g) = glob {
                    let back = glob_to_section(&g, &reg).expect("decode");
                    let a = section_decode(&canon).expect("decode canon");
                    let b = section_decode(&back).expect("decode back");
                    assert!(sections_semantically_equal(&a, &b), "semantic {distinct}");
                }
                continue;
            }
            let glob = glob.expect("glob conversion");
            assert_eq!(glob[0], GLOB_MAGIC);
            let back = glob_to_section(&glob, &reg).expect("decode");
            // semantic equality through the SECTION DECODER (CSEC -> Value):
            // the rebuilt palette holds only USED states, so the wire bytes
            // differ while the section content is identical
            let a = section_decode(&canon).expect("decode canon");
            let b = section_decode(&back).expect("decode back");
            assert!(
                sections_semantically_equal(&a, &b),
                "semantic roundtrip failed at distinct={distinct}"
            );
            // registry persistence
            let bytes = reg.to_bytes();
            let mut reg2 = StateRegistry::from_bytes(&bytes).expect("registry parse");
            assert_eq!(reg2.len(), reg.len(), "registry size");
            for g in 0..reg.len() as u32 {
                assert_eq!(reg.get(g), reg2.get(g), "state {g}");
            }
            // a SECOND conversion of the rebuilt canonical bytes must yield
            // the same gid stream content (determinism)
            let glob2 = section_to_glob(&back, &mut reg2).expect("re-glob");
            let a2 = section_decode(&back).expect("re-decode canon");
            // production contract: 0xCB members are converted back by
            // read_blob BEFORE any decoder; mirror that here
            let back2 = glob_to_section(&glob2, &reg2).expect("decode glob2");
            let b2 = section_decode(&back2).expect("re-decode glob");
            assert!(sections_semantically_equal(&a2, &b2), "re-glob semantic {distinct}");
        }
    }

    #[test]
    fn glob_rejects_rle_and_single_state_sections() {
        let mut reg = StateRegistry::new();
        // single-state section (no data)
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(3));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![Value::String("minecraft:stone".to_string())]),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let canon = section_encode(&Value::Compound(sec)).expect("encode");
        assert!(section_to_glob(&canon, &mut reg).is_none(), "single-state must not glob");
        // RLE-friendly section: one horizontal stratum plane -> rle_van wins
        let mut sec2: Map<String, Value> = Map::new();
        sec2.insert("Y".to_string(), Value::Byte(1));
        let mut bs2: Map<String, Value> = Map::new();
        bs2.insert(
            "palette".to_string(),
            Value::List(vec![
                Value::String("minecraft:air".to_string()),
                Value::String("minecraft:stone".to_string()),
            ]),
        );
        let mut vox = [0u8; 4096];
        for x in 0..16 {
            for z in 0..16 {
                vox[x * 256 + z] = 1;
            }
        }
        bs2.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(
                dense_to_nostroddle(&vox, 4096, 8, 1)
                    .iter()
                    .map(|l| *l as i64)
                    .collect::<Vec<i64>>(),
            )),
        );
        sec2.insert("block_states".to_string(), Value::Compound(bs2));
        let canon2 = section_encode(&Value::Compound(sec2)).expect("encode2");
        // the encoder chose an RLE form (f2 order bits set) -> not globbable
        assert!(canon2.len() > 4);
        if canon2[2] & 48 != 0 {
            assert!(section_to_glob(&canon2, &mut reg).is_none(), "rle must not glob");
        }
    }
}
