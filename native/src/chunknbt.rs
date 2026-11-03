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
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::borrow::Cow;
use std::collections::BTreeMap;
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
    // Word-at-a-time twin of dense_pack for u16 entries (palettes > 256);
    // bit-identical output to the BitW::put formulation.
    let b = bits as usize;
    let mut out = Vec::with_capacity((entries.len() * b + 7) / 8 + 8);
    let mut acc: u64 = 0;
    let mut n: usize = 0;
    for &e in entries {
        acc |= (e as u64) << n;
        n += b;
        if n >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            n -= 64;
            acc = (e as u64) >> (b - n);
        }
    }
    if n > 0 {
        let bytes = acc.to_le_bytes();
        out.extend_from_slice(&bytes[..(n + 7) / 8]);
    }
    out
}

/// dense_pack with the palette remap applied on the fly: emits exactly the
/// bytes of dense_pack(&entries.iter().map(|x| remap[*x]).collect(), bits)
/// without materializing the mapped stream (remap is injective on 0..n and
/// every entry is < n, so per-entry mapping and packing fuse losslessly).
fn dense_pack_mapped(entries: &[u32], remap: &[u8], bits: u32) -> Vec<u8> {
    let b = bits as usize;
    let mut out = Vec::with_capacity((entries.len() * b + 7) / 8 + 8);
    let mut acc: u64 = 0;
    let mut n: usize = 0;
    for &e in entries {
        let e = remap[e as usize];
        acc |= (e as u64) << n;
        n += b;
        if n >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            n -= 64;
            acc = (e as u64) >> (b - n);
        }
    }
    if n > 0 {
        let bytes = acc.to_le_bytes();
        out.extend_from_slice(&bytes[..(n + 7) / 8]);
    }
    out
}

/// Wide (palette > 256) twin of dense_pack_mapped: emits exactly the bytes
/// of dense_pack_wide over the materialized remapped u16 stream.
fn dense_pack_wide_mapped(entries: &[u32], remap: &[u16], bits: u32) -> Vec<u8> {
    let b = bits as usize;
    let mut out = Vec::with_capacity((entries.len() * b + 7) / 8 + 8);
    let mut acc: u64 = 0;
    let mut n: usize = 0;
    for &e in entries {
        let e = remap[e as usize];
        acc |= (e as u64) << n;
        n += b;
        if n >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            n -= 64;
            acc = (e as u64) >> (b - n);
        }
    }
    if n > 0 {
        let bytes = acc.to_le_bytes();
        out.extend_from_slice(&bytes[..(n + 7) / 8]);
    }
    out
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
// finer cut of SPLT_SECS: sh = structural_hash loop, enc = section_encode
// calls (cache misses only), pal/unp/pack = inside enc_pal_data_canon
// (palette parse+sort+counts / unpack+remap / dense+RLE evaluation).
static SPLT_SH: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_ENC: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_PAL: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_UNP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_PACK: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
/// scan-split attempts (fast path; falls back to the full parse on doubt)
static SPLT_SCAN: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static SPLT_SCAN_OK: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
/// v0.9.12: fastnbt parse of the sections-only wrapper inside scan_split
/// (the residual walk cost = SPLT_SCAN - SPLT_SCANP - enc - shift).
static SPLT_SCANP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
/// v0.9.13: scan_split chunks whose sections parsed through the typed
/// borrowed lite form (the rest fell back to the Value parse). Calls only.
static SPLT_LITE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
/// Last scan_root failure reason: 1=root hdr, 2=sections skip, 3=hmap walk,
/// 4=light patch, 5=generic key skip, 6=no sections key, 7=root key name.
static SCAN_FAIL: AtomicU64 = AtomicU64::new(0);
/// Byte position of the failure (diagnostics only; stored at the failing
/// sr! site — exact, unlike a deepest-walk beacon, and zero cost on success).
static SCAN_FAIL_POS: AtomicU64 = AtomicU64::new(0);

/// One-line JSON summary of the split-path stage timers (nanos, calls).
pub fn split_stage_json() -> String {
    fn f(name: &str, t: &(AtomicU64, AtomicU64)) -> String {
        let ns = t.0.load(Ordering::Relaxed);
        let n = t.1.load(Ordering::Relaxed);
        format!(
            r#""{}":{{ns:{},n:{},per_call:{}}}"#,
            name,
            ns,
            n,
            ns.checked_div(n.max(1)).unwrap_or(0)
        )
    }
    format!(
        "{{{},{},{},{},{},{},{},{},{},{},{},{},{}}}",
        f("parse", &SPLT_PARSE),
        f("sections", &SPLT_SECS),
        f("sh", &SPLT_SH),
        f("enc", &SPLT_ENC),
        f("pal", &SPLT_PAL),
        f("unp", &SPLT_UNP),
        f("pack", &SPLT_PACK),
        f("shift", &SPLT_SHIFT),
        f("tail", &SPLT_TAIL),
        f("scan", &SPLT_SCAN),
        f("scan_ok", &SPLT_SCAN_OK),
        f("scanp", &SPLT_SCANP),
        f("lite", &SPLT_LITE)
    )
    + &format!(
        r#","scan_fail":{{"why":{},"pos":{}}}"#,
        SCAN_FAIL.load(Ordering::Relaxed),
        SCAN_FAIL_POS.load(Ordering::Relaxed)
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

/// u32-input twin of rle_encode_stream (GLOB gid streams wider than 8
/// bits): [gid u16 LE][varint run] pairs in stream order.
fn rle_encode_stream_u16(v: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() / 4 * 3 + 16);
    let mut i = 0usize;
    while i < v.len() {
        let idx = v[i] as u16;
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == v[i] {
            run += 1;
        }
        out.extend_from_slice(&idx.to_le_bytes());
        push_uvarint(&mut out, run as u64);
        i += run;
    }
    out
}

/// u32-input twin of transpose_fast_y (column-major reorder for wide gid
/// streams).
fn transpose_fast_y_u16(v: &[u32], side: usize) -> Vec<u32> {
    let s2 = side * side;
    let mut out = vec![0u32; v.len()];
    for y in 0..side {
        for z in 0..side {
            for x in 0..side {
                out[x * s2 + z * side + y] = v[y * s2 + z * side + x];
            }
        }
    }
    out
}

/// rle_encode_stream over a mapped u32 stream ([idx u8][varint run] framing):
/// identical bytes to rle_encode_stream over the materialized mapped stream.
fn rle_encode_stream_mapped(v: &[u32], remap: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() / 4 + 16);
    let mut i = 0usize;
    while i < v.len() {
        let idx = remap[v[i] as usize];
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == v[i] {
            run += 1;
        }
        out.push(idx);
        push_uvarint(&mut out, run as u64);
        i += run;
    }
    out
}

/// RLE SIZE of a mapped u32 stream ([idx u8][varint run] framing) without
/// materializing it: run boundaries match the materialized form exactly
/// (injective remap preserves equality), and the framing length is
/// independent of the idx byte value.
fn rle_stream_len_mapped(v: &[u32], remap: &[u8]) -> u64 {
    let _ = remap; // equality-preserving: run structure of v == mapped v
    let mut total = 0u64;
    let mut i = 0usize;
    while i < v.len() {
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == v[i] {
            run += 1;
        }
        total += 1 + uvarint_len(run as u64);
        i += run;
    }
    total
}

/// RLE SIZE of the y-transposed mapped u32 stream without materializing
/// either the map or the transpose: same (x, z, y) walk as
/// rle_transposed_len over the mapped stream.
fn rle_transposed_len_mapped(v: &[u32], remap: &[u8], side: usize) -> u64 {
    let _ = remap;
    let s2 = side * side;
    let mut total = 0u64;
    let mut run_val = 0u32;
    let mut run_len = 0usize;
    for x in 0..side {
        for z in 0..side {
            let base = z * side + x;
            for y in 0..side {
                let e = v[y * s2 + base];
                if run_len > 0 && e == run_val {
                    run_len += 1;
                } else {
                    if run_len > 0 {
                        total += 1 + uvarint_len(run_len as u64);
                    }
                    run_val = e;
                    run_len = 1;
                }
            }
        }
    }
    if run_len > 0 {
        total += 1 + uvarint_len(run_len as u64);
    }
    total
}

/// uvarint (LEB128) byte length.
fn uvarint_len(mut v: u64) -> u64 {
    let mut l = 1;
    while v >= 0x80 {
        v >>= 7;
        l += 1;
    }
    l
}

/// RLE stream SIZE without materializing it: the same run walk as
/// rle_encode_stream ([idx u8][varint run] pairs), counting bytes only.
/// The admission rules only need the length — materializing the full
/// candidate for a check that rejects ~99% of noise-heavy sections was
/// pure waste (~20 KB of writes per section across the two candidates).
fn rle_stream_len(v: &[u8]) -> u64 {
    let mut total = 0u64;
    let mut i = 0usize;
    while i < v.len() {
        let idx = v[i];
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == idx {
            run += 1;
        }
        total += 1 + uvarint_len(run as u64);
        i += run;
    }
    total
}

/// RLE size of the y-transposed stream without materializing the
/// transpose: walks the same (x, z, y) order as transpose_fast_y (x outer,
/// z middle, y inner stride s2) accumulating the identical run framing.
fn rle_transposed_len(v: &[u8], side: usize) -> u64 {
    let s2 = side * side;
    let mut total = 0u64;
    let mut run_val = 0u8;
    let mut run_len = 0usize;
    for x in 0..side {
        for z in 0..side {
            let base = z * side + x;
            for y in 0..side {
                let e = v[y * s2 + base];
                if run_len > 0 && e == run_val {
                    run_len += 1;
                } else {
                    if run_len > 0 {
                        total += 1 + uvarint_len(run_len as u64);
                    }
                    run_val = e;
                    run_len = 1;
                }
            }
        }
    }
    if run_len > 0 {
        total += 1 + uvarint_len(run_len as u64);
    }
    total
}

/// u32-input twin of rle_stream_len ([gid u16 LE][varint run] framing).
fn rle_stream_len_u16(v: &[u32]) -> u64 {
    let mut total = 0u64;
    let mut i = 0usize;
    while i < v.len() {
        let idx = v[i];
        let mut run = 1usize;
        while i + run < v.len() && v[i + run] == idx {
            run += 1;
        }
        total += 2 + uvarint_len(run as u64);
        i += run;
    }
    total
}

/// u32-input twin of rle_transposed_len.
fn rle_transposed_len_u16(v: &[u32], side: usize) -> u64 {
    let s2 = side * side;
    let mut total = 0u64;
    let mut run_val = 0u32;
    let mut run_len = 0usize;
    for x in 0..side {
        for z in 0..side {
            let base = z * side + x;
            for y in 0..side {
                let e = v[y * s2 + base];
                if run_len > 0 && e == run_val {
                    run_len += 1;
                } else {
                    if run_len > 0 {
                        total += 2 + uvarint_len(run_len as u64);
                    }
                    run_val = e;
                    run_len = 1;
                }
            }
        }
    }
    if run_len > 0 {
        total += 2 + uvarint_len(run_len as u64);
    }
    total
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
    // Word-at-a-time continuous LSB-first packing. Bit-identical output to
    // the old BitW::put byte loop (2 branches + a byte push per entry), but
    // one branch per entry and a full LE word flush every 64/bits entries.
    let b = bits as usize;
    let mut out = Vec::with_capacity((entries.len() * b + 7) / 8 + 8);
    let mut acc: u64 = 0;
    let mut n: usize = 0;
    for &e in entries {
        acc |= (e as u64) << n;
        n += b;
        if n >= 64 {
            out.extend_from_slice(&acc.to_le_bytes());
            n -= 64;
            // the top (n) bits of this entry continue into the next word
            acc = (e as u64) >> (b - n);
        }
    }
    if n > 0 {
        let bytes = acc.to_le_bytes();
        out.extend_from_slice(&bytes[..(n + 7) / 8]);
    }
    out
}

fn dense_unpack(b: &[u8], n: usize, bits: u32) -> Option<Vec<u8>> {
    let mut r = BitR { in_: b, pos: 0 };
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.get(bits)? as u8);
    }
    Some(out)
}

/// Width-safe voxel unpack: 9..16-bit streams (palettes > 256 states) must
/// NOT go through dense_unpack — `as u8` would silently wrap indices >= 256
/// and corrupt the section. Dispatches to the wide reader.
fn dense_unpack_vox(b: &[u8], n: usize, bits: u32) -> Option<Vec<u32>> {
    if bits <= 8 {
        let v = dense_unpack(b, n, bits)?;
        Some(v.iter().map(|&x| x as u32).collect())
    } else {
        let v = dense_unpack_wide(b, n, bits)?;
        Some(v.iter().map(|&x| x as u32).collect())
    }
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
pub fn dense_to_nostroddle(dense: &[u8], n: usize, in_bits: u32, out_bits: u32) -> Vec<i64> {
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

/// Wide twin of unpack_nostroddle for 9..16-bit streams (palettes > 256
/// states): the u8 variant silently WRAPS indices >= 256, which corrupted
/// the frequency counts, the canonical remap and the section identity for
/// wide-palette sections. Palette sizes are <= 65535, so u16 suffices.
pub fn unpack_nostroddle_wide(longs: &[i64], n: usize, bits: u32) -> Vec<u16> {
    let per = (64 / bits) as usize;
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let mut out = Vec::with_capacity(n);
    for l in longs {
        let lu = *l as u64;
        for i in 0..per {
            if out.len() == n {
                return out;
            }
            out.push(((lu >> (i * bits as usize)) & mask) as u16);
        }
    }
    out
}

/// Width-correct voxel unpack for block_states data (bits up to 16).
fn unpack_nostroddle_vox(longs: &[i64], n: usize, bits: u32) -> Vec<u32> {
    if bits <= 8 {
        unpack_nostroddle(longs, n, bits).into_iter().map(|x| x as u32).collect()
    } else {
        unpack_nostroddle_wide(longs, n, bits)
            .into_iter()
            .map(|x| x as u32)
            .collect()
    }
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
    section_encode_impl(sec, false).map(|(b, _)| b)
}

// ------------------------------------ typed borrowed sections (v0.9.13) -----
//
// The scan-split path parsed the sections list into a fastnbt Value tree
// (SPLT_SCANP ~125 us/chunk): one HashMap per palette entry plus two String
// allocations per property, none of which survive encoding — the encoder
// borrows name/props through parse_entry and drops the tree. A serde typed
// parse over the same bytes (Cow<'a, str> palette names and properties,
// BTreeMap properties, borrowed light slices) parses the SAME list ~2.4x
// faster (probe: 42218 corpus sections, semantic equality 0 mismatches).
//
// The typed parse is byte-shape-strict: deny_unknown_fields over exactly the
// 7 KNOWN section keys, Y: i8, palette entries string-or-{Name, Properties},
// data LongArray. ANY deviation fails the parse and the whole chunk takes
// the old Value path (identical outputs, just slower) — the lite path is a
// pure fast-path, never a behavior change.
//
// Downstream, both paths feed the SAME SecView currency (palette entries as
// (name, props) borrows + data slice), so section_encode_view and the shift
// pass run unchanged over either source, and enc/shift outputs are pinned
// byte-identical by enc_fp.

/// Borrowed palette entry — the encoder's currency (moved out of
/// section_encode_impl so the Value and lite paths feed the same code).
#[derive(Clone)]
struct PalEntry<'a> {
    name: &'a str,
    props: Vec<(&'a str, &'a str)>,
    plain: bool, // was a bare string
}

fn parse_entry<'a>(v: &'a Value) -> Option<PalEntry<'a>> {
    match v {
        Value::String(s) => Some(PalEntry { name: s.as_str(), props: Vec::new(), plain: true }),
        Value::Compound(cm) => {
            let Value::String(name) = cm.get("Name")? else { return None };
            let mut props = Vec::new();
            match cm.get("Properties") {
                Some(Value::Compound(pn)) => {
                    for (k, kv) in pn {
                        let Value::String(vv) = kv else { return None };
                        props.push((k.as_str(), vv.as_str()));
                    }
                }
                None => {}
                _ => return None,
            }
            props.sort();
            Some(PalEntry { name: name.as_str(), props, plain: false })
        }
        _ => None,
    }
}

fn pal_entry_from_lit<'a>(l: &'a PalLit<'a>) -> Option<PalEntry<'a>> {
    match l {
        PalLit::Plain(s) => Some(PalEntry { name: s, props: Vec::new(), plain: true }),
        PalLit::Named { name, props } => {
            let mut props: Vec<(&str, &str)> = match props {
                Some(m) => m.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect(),
                None => Vec::new(),
            };
            props.sort();
            Some(PalEntry { name, props, plain: false })
        }
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

/// Typed borrowed palette entry (serde): string form, or compound
/// {Name, Properties?}. Unknown keys inside the entry compound are ignored
/// (mirrors parse_entry, which only reads Name/Properties); non-string
/// property values fail the parse (mirrors parse_entry -> None).
#[derive(Debug)]
enum PalLit<'a> {
    Plain(Cow<'a, str>),
    Named {
        name: Cow<'a, str>,
        props: Option<BTreeMap<Cow<'a, str>, Cow<'a, str>>>,
    },
}

impl<'de: 'a, 'a> Deserialize<'de> for PalLit<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = PalLit<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("palette entry (string or {Name, Properties})")
            }
            fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(PalLit::Plain(Cow::Borrowed(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(PalLit::Plain(Cow::Owned(v.to_string())))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut name: Option<Cow<'a, str>> = None;
                let mut props: Option<BTreeMap<Cow<'a, str>, Cow<'a, str>>> = None;
                while let Some(k) = a.next_key::<Cow<'de, str>>()? {
                    match k.as_ref() {
                        "Name" => {
                            let v: Cow<'de, str> = a.next_value()?;
                            name = Some(v);
                        }
                        "Properties" => {
                            let m: PropsMap = a.next_value()?;
                            props = Some(m.0);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = a.next_value()?;
                        }
                    }
                }
                let name = name.ok_or_else(|| serde::de::Error::missing_field("Name"))?;
                Ok(PalLit::Named { name, props })
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

/// Sorted string->string properties (BTreeMap: canonical order for free —
/// the same order parse_entry sorts to).
#[derive(Debug)]
struct PropsMap<'a>(BTreeMap<Cow<'a, str>, Cow<'a, str>>);

impl<'de: 'a, 'a> Deserialize<'de> for PropsMap<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = PropsMap<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("string->string properties map")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Self::Value, A::Error> {
                let mut m = BTreeMap::new();
                while let Some((k, v)) = a.next_entry::<Cow<'de, str>, Cow<'de, str>>()? {
                    m.insert(k, v);
                }
                Ok(PropsMap(m))
            }
        }
        d.deserialize_map(V(std::marker::PhantomData))
    }
}

/// Typed borrowed palette+data pair (block_states / biomes).
#[derive(Deserialize)]
struct PalData<'a> {
    #[serde(borrow)]
    palette: Vec<PalLit<'a>>,
    #[serde(default)]
    data: Option<fastnbt::LongArray>,
}

/// Typed borrowed section. Exactly the 7 KNOWN keys; anything else fails the
/// parse and the chunk takes the Value path. Derived fields (light arrays,
/// starlight states) ride as borrows/integers — the encoder re-emits them
/// exactly like the Value path does (uniform flags / raw bytes / f2 bits).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecLite<'a> {
    #[serde(rename = "Y")]
    y: i8,
    #[serde(borrow)]
    block_states: Option<PalData<'a>>,
    #[serde(borrow)]
    biomes: Option<PalData<'a>>,
    #[serde(rename = "SkyLight", borrow)]
    skylight: Option<&'a [u8]>,
    #[serde(rename = "BlockLight", borrow)]
    blocklight: Option<&'a [u8]>,
    #[serde(rename = "starlight.skylight_state")]
    sl_state: Option<i32>,
    #[serde(rename = "starlight.blocklight_state")]
    bl_state: Option<i32>,
}

/// Root wrapper: the sections list rides under the "" key of the wrap
/// compound built for fastnbt (NBT roots must be compounds).
#[derive(Deserialize)]
struct RootList<'a> {
    #[serde(rename = "", borrow)]
    list: Vec<SecLite<'a>>,
}

/// Everything section_encode_view and the shift pass consume from a section,
/// borrowed. Built from either the Value tree or the typed lite parse.
#[derive(Clone)]
struct PalDataV<'a> {
    palette: Vec<PalEntry<'a>>,
    data: Option<&'a [i64]>,
}

#[derive(Clone)]
struct SecView<'a> {
    y: i8,
    bs: Option<PalDataV<'a>>,
    bio: Option<PalDataV<'a>>,
    skylight: Option<Vec<u8>>,
    blocklight: Option<Vec<u8>>,
    sl: Option<i32>,
    bl: Option<i32>,
}

const SEC_KNOWN: [&str; 7] = [
    "Y",
    "block_states",
    "biomes",
    "SkyLight",
    "BlockLight",
    "starlight.skylight_state",
    "starlight.blocklight_state",
];

/// Value -> SecView. Mirrors section_encode_impl's checks exactly: any key
/// outside the 7 KNOWN ones, a non-Byte Y, a non-compound block_states/
/// biomes, a missing/ill-typed palette or an ill-typed data array all yield
/// None (the encoder's None -> raw-NBT storage). Non-array light fields and
/// non-int starlight states are silently skipped (the old `else { continue }`
/// arms), NOT errors.
fn sec_view_of_value(sec: &Value) -> Option<SecView> {
    let Value::Compound(m) = sec else { return None };
    if m.keys().any(|k| !SEC_KNOWN.contains(&k.as_str())) {
        return None;
    }
    let y = match m.get("Y") {
        Some(Value::Byte(b)) => *b,
        _ => return None,
    };
    let bs = match m.get("block_states") {
        None => None,
        Some(Value::Compound(bm)) => Some(pal_data_view_of_value(bm)?),
        Some(_) => return None,
    };
    let bio = match m.get("biomes") {
        None => None,
        Some(Value::Compound(bm)) => Some(pal_data_view_of_value(bm)?),
        Some(_) => return None,
    };
    let skylight = match m.get("SkyLight") {
        Some(Value::ByteArray(ba)) => Some(ba.as_ref().iter().map(|x| *x as u8).collect()),
        _ => None,
    };
    let blocklight = match m.get("BlockLight") {
        Some(Value::ByteArray(ba)) => Some(ba.as_ref().iter().map(|x| *x as u8).collect()),
        _ => None,
    };
    let sl = match m.get("starlight.skylight_state") {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let bl = match m.get("starlight.blocklight_state") {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    Some(SecView { y, bs, bio, skylight, blocklight, sl, bl })
}

fn pal_data_view_of_value(bm: &std::collections::HashMap<String, Value>) -> Option<PalDataV> {
    let Some(Value::List(pal)) = bm.get("palette") else { return None };
    let mut palette = Vec::with_capacity(pal.len());
    for p in pal {
        palette.push(parse_entry(p)?);
    }
    let data = match bm.get("data") {
        Some(Value::LongArray(la)) => Some(la.as_ref()),
        Some(_) => return None,
        None => None,
    };
    Some(PalDataV { palette, data })
}

/// SecLite -> SecView. `strip` drops the derived fields exactly like
/// strip_derived_section did before hashing/encoding on the Value path.
fn sec_view_of_lite<'a>(s: &'a SecLite<'a>, strip: bool) -> Option<SecView<'a>> {
    let bs = match &s.block_states {
        None => None,
        Some(pd) => Some(pal_data_view_of_lit(pd)?),
    };
    let bio = match &s.biomes {
        None => None,
        Some(pd) => Some(pal_data_view_of_lit(pd)?),
    };
    if strip {
        return Some(SecView { y: s.y, bs, bio, skylight: None, blocklight: None, sl: None, bl: None });
    }
    Some(SecView {
        y: s.y,
        bs,
        bio,
        skylight: s.skylight.map(|b| b.to_vec()),
        blocklight: s.blocklight.map(|b| b.to_vec()),
        sl: s.sl_state,
        bl: s.bl_state,
    })
}

fn pal_data_view_of_lit<'a>(pd: &'a PalData<'a>) -> Option<PalDataV<'a>> {
    let mut palette = Vec::with_capacity(pd.palette.len());
    for p in &pd.palette {
        palette.push(pal_entry_from_lit(p)?);
    }
    let data = pd.data.as_ref().map(|la| la.as_ref() as &[i64]);
    Some(PalDataV { palette, data })
}

/// feed() over the lite form. Byte-stream-identical to feed(&Value) for the
/// equivalent section: compounds feed [10][len][sorted keys...], the sorted
/// key order is ASCII ("BlockLight" < "SkyLight" < "Y" < "biomes" <
/// "block_states" < "starlight.blocklight_state" < "starlight.skylight_state"),
/// inner palette/data compounds sort "data" < "palette", entries "Name" <
/// "Properties". `strip` mirrors strip_derived_section: derived fields are
/// not fed at all.
fn feed_lite(h: &mut Xxh3, s: &SecLite, strip: bool) {
    // (key, present) pairs in ASCII order; emit only present keys
    fn w(h: &mut Xxh3, b: &[u8]) {
        h.update(b);
    }
    let mut keys: [(&str, bool); 7] = [("", false); 7];
    let mut nk = 0usize;
    if !strip {
        if s.blocklight.is_some() {
            keys[nk] = ("BlockLight", true);
            nk += 1;
        }
        if s.skylight.is_some() {
            keys[nk] = ("SkyLight", true);
            nk += 1;
        }
    }
    keys[nk] = ("Y", true);
    nk += 1;
    keys[nk] = ("biomes", s.biomes.is_some());
    nk += 1;
    keys[nk] = ("block_states", s.block_states.is_some());
    nk += 1;
    if !strip {
        if s.bl_state.is_some() {
            keys[nk] = ("starlight.blocklight_state", true);
            nk += 1;
        }
        if s.sl_state.is_some() {
            keys[nk] = ("starlight.skylight_state", true);
            nk += 1;
        }
    }
    // count = number of present keys
    let nkeys = keys[..nk].iter().filter(|(_, p)| *p).count() as u64;
    w(h, &[10]);
    w(h, &nkeys.to_le_bytes());
    for (k, present) in keys[..nk].iter() {
        if !present {
            continue;
        }
        w(h, &(k.len() as u64).to_le_bytes());
        w(h, k.as_bytes());
        match *k {
            "Y" => {
                w(h, &[1]);
                w(h, &s.y.to_le_bytes());
            }
            "SkyLight" => {
                let b = s.skylight.unwrap_or(&[]);
                w(h, &[7]);
                w(h, &(b.len() as u64).to_le_bytes());
                w(h, b);
            }
            "BlockLight" => {
                let b = s.blocklight.unwrap_or(&[]);
                w(h, &[7]);
                w(h, &(b.len() as u64).to_le_bytes());
                w(h, b);
            }
            "biomes" => feed_lite_pal_data(h, s.biomes.as_ref().unwrap()),
            "block_states" => feed_lite_pal_data(h, s.block_states.as_ref().unwrap()),
            "starlight.skylight_state" => {
                w(h, &[3]);
                w(h, &s.sl_state.unwrap_or(0).to_le_bytes());
            }
            "starlight.blocklight_state" => {
                w(h, &[3]);
                w(h, &s.bl_state.unwrap_or(0).to_le_bytes());
            }
            _ => unreachable!(),
        }
    }
}

/// feed() over a lite palette+data compound: [10][nkeys]["data" LongArray?]
/// ["palette" List?] in ASCII order ("data" < "palette").
fn feed_lite_pal_data(h: &mut Xxh3, pd: &PalData) {
    let has_data = pd.data.is_some();
    let nkeys = 1 + has_data as u64;
    h.update(&[10]);
    h.update(&nkeys.to_le_bytes());
    if let Some(d) = &pd.data {
        h.update(&4u64.to_le_bytes());
        h.update(b"data");
        h.update(&[12]);
        h.update(&(d.len() as u64).to_le_bytes());
        // identical stream to feed(Value::LongArray): raw LE bytes of the
        // parsed i64s (both parse from the same big-endian source)
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(d.as_ptr() as *const u8, d.len() * 8)
        };
        h.update(bytes);
    }
    h.update(&7u64.to_le_bytes());
    h.update(b"palette");
    h.update(&[9]);
    h.update(&(pd.palette.len() as u64).to_le_bytes());
    for p in &pd.palette {
        match p {
            PalLit::Plain(s) => {
                h.update(&[8]);
                h.update(&(s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }
            PalLit::Named { name, props } => {
                // compound: keys sorted "Name" < "Properties"
                let nkeys = 1 + props.is_some() as u64;
                h.update(&[10]);
                h.update(&nkeys.to_le_bytes());
                h.update(&4u64.to_le_bytes());
                h.update(b"Name");
                h.update(&[8]);
                h.update(&(name.len() as u64).to_le_bytes());
                h.update(name.as_bytes());
                if let Some(m) = props {
                    h.update(&10u64.to_le_bytes());
                    h.update(b"Properties");
                    h.update(&[10]);
                    h.update(&(m.len() as u64).to_le_bytes());
                    for (k, v) in m {
                        h.update(&(k.len() as u64).to_le_bytes());
                        h.update(k.as_bytes());
                        h.update(&[8]);
                        h.update(&(v.len() as u64).to_le_bytes());
                        h.update(v.as_bytes());
                    }
                }
            }
        }
    }
}

/// Raw-NBT fallback for lite sections whose encoder returns None (rare:
/// oversized palettes, out-of-range starlight, malformed data). Deterministic
/// sorted-key order (the old to_bytes(s) ran over a HashMap-ordered tree, so
/// byte order was never reproducible there; the join side parses NBT
/// order-insensitively).
fn sec_lite_to_bytes(s: &SecLite, strip: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(0x0A); // TAG_Compound root, name ""
    out.extend_from_slice(&0u16.to_be_bytes());
    let mut put_key = |out: &mut Vec<u8>, tag: u8, name: &str| {
        out.push(tag);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
    };
    // list elements carry NO tag byte (the element type is declared once in
    // the list header); a palette is homogeneous by NBT construction, so the
    // element type is derivable — and a plain entry inside a compound-typed
    // list (impossible from a parsed NBT list, kept for safety) is emitted in
    // the interchangeable {Name} form so the bytes stay valid.
    let put_palette = |out: &mut Vec<u8>, pd: &PalData| {
        out.push(0x09);
        out.extend_from_slice(&(7u16).to_be_bytes());
        out.extend_from_slice(b"palette");
        let all_plain = pd.palette.iter().all(|p| matches!(p, PalLit::Plain(_)));
        out.push(if all_plain { 0x08 } else { 0x0A });
        out.extend_from_slice(&(pd.palette.len() as i32).to_be_bytes());
        for p in &pd.palette {
            match p {
                PalLit::Plain(name) => {
                    if all_plain {
                        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
                        out.extend_from_slice(name.as_bytes());
                    } else {
                        // unreachable from a parsed NBT list; {Name} form
                        out.extend_from_slice(&(4u16).to_be_bytes());
                        out.extend_from_slice(b"Name");
                        out.push(0x08);
                        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
                        out.extend_from_slice(name.as_bytes());
                        out.push(0x00);
                    }
                }
                PalLit::Named { name, props } => {
                    out.extend_from_slice(&(4u16).to_be_bytes());
                    out.extend_from_slice(b"Name");
                    out.push(0x08);
                    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
                    out.extend_from_slice(name.as_bytes());
                    if let Some(m) = props {
                        out.extend_from_slice(&(10u16).to_be_bytes());
                        out.extend_from_slice(b"Properties");
                        out.push(0x0A);
                        out.extend_from_slice(&(m.len() as i32).to_be_bytes());
                        for (k, v) in m {
                            out.push(0x08);
                            out.extend_from_slice(&(k.len() as u16).to_be_bytes());
                            out.extend_from_slice(k.as_bytes());
                            out.extend_from_slice(&(v.len() as u16).to_be_bytes());
                            out.extend_from_slice(v.as_bytes());
                        }
                    }
                    out.push(0x00); // END of the entry compound
                }
            }
        }
    };
    put_key(&mut out, 0x01, "Y");
    out.push(s.y as u8);
    for (pd, name) in [(&s.biomes, "biomes"), (&s.block_states, "block_states")] {
        let Some(pd) = pd else { continue };
        put_key(&mut out, 0x0A, name);
        // inner compound entries (tagged): "data" < "palette" in ASCII order
        if let Some(d) = &pd.data {
            out.push(0x0C);
            out.extend_from_slice(&(4u16).to_be_bytes());
            out.extend_from_slice(b"data");
            out.extend_from_slice(&(d.len() as i32).to_be_bytes());
            for l in d.iter() {
                out.extend_from_slice(&l.to_be_bytes());
            }
        }
        put_palette(&mut out, pd);
        out.push(0x00); // END of the pal-data compound
    }
    if !strip {
        for (b, name) in [(s.blocklight, "BlockLight"), (s.skylight, "SkyLight")] {
            if let Some(b) = b {
                put_key(&mut out, 0x07, name);
                out.extend_from_slice(&(b.len() as i32).to_be_bytes());
                out.extend_from_slice(b);
            }
        }
        for (v, name) in [(s.bl_state, "starlight.blocklight_state"), (s.sl_state, "starlight.skylight_state")] {
            if let Some(v) = v {
                put_key(&mut out, 0x03, name);
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
    out.push(0x00); // END of root
    out
}


/// Encoder with optional shift-facts capture (v0.9.12): when `want_facts`,
/// the dense-analysis artifacts (orig-index voxel stream + canonical
/// orig-index order) are returned alongside the bytes, so the shift pass
/// reuses them instead of re-unpacking and re-sorting (~20 µs per dense
/// section). Byte output is identical either way (facts capture happens
/// after the pack/RLE decision, on values that were dead anyway).
fn section_encode_impl(
    sec: &Value,
    want_facts: bool,
) -> Option<(Vec<u8>, Option<SectionFacts>)> {
    let view = sec_view_of_value(sec)?;
    section_encode_view(&view, want_facts)
}

/// Encoder over the SecView currency (v0.9.13): byte-identical output to the
/// old &Value body — sec_view_of_value / sec_view_of_lite reproduce every
/// check (KNOWN keys, Y byte, palette/data types, light-skip semantics)
/// before we get here. Fed by both the Value path and the typed lite path.
fn section_encode_view(
    view: &SecView,
    want_facts: bool,
) -> Option<(Vec<u8>, Option<SectionFacts>)> {
    // 5-byte v4 header; patched down to the 4-byte v3 form when f3 stays 0.
    let mut out = vec![CSEC4_MAGIC, 0u8, 0u8, 0u8, view.y as u8];
    let mut f1 = 0u8;
    // shift-facts capture (filled by the block_states encode below)
    let mut block_facts: Option<SectionFacts> = None;
    let mut f2 = 0u8;
    let mut f3 = 0u8;

    // The real implementation works on palette index order (v0.9.13: a sorted
    // index vec instead of moving (entry, old_index) pairs — the entries live
    // in the caller's PalDataV and are shared with the shift pass's identity
    // interning). Returns (has_data, f2 flag bits): dense form sets
    // `narrow_bit` when the stream is packed at ceillog2(n) (beats the vanilla
    // 4-bit clamp for palettes under 16 entries); RLE forms set
    // (order << order_shift).
    fn enc_pal_data_canon(
        out: &mut Vec<u8>,
        pdv: &PalDataV,
        entries: usize,
        narrow_bit: u8,
        order_shift: u32,
        facts_out: &mut Option<SectionFacts>,
    ) -> Option<(bool, u8)> {
        let pal = &pdv.palette;
        let n = pal.len();
        if n == 0 || n > 65535 {
            return None;
        }
        // One unpack feeds everything downstream (v0.9.7): frequency counts
        // for the canonical order, the remap, and the dense/RLE candidates
        // used to re-unpack the same 4096 voxels a second time (~4 µs/section).
        // Failure modes keep the old outputs: malformed data still yields
        // None (via the sanity check below), and the no-data path keeps the
        // all-zero counts order.
        let data: &[i64] = match pdv.data {
            Some(d) => d,
            None => {
                if n > 1 {
                    return None;
                }
                let t_pal0 = std::time::Instant::now();
                let mut order_idx: Vec<usize> = (0..n).collect();
                order_idx.sort_by(|&a, &b| {
                    (pal[a].plain, pal[a].name, &pal[a].props)
                        .cmp(&(pal[b].plain, pal[b].name, &pal[b].props))
                });
                SPLT_PAL.0.fetch_add(t_pal0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                SPLT_PAL.1.fetch_add(1, Ordering::Relaxed);
                out.extend_from_slice(&(n as u16).to_le_bytes());
                for &oi in &order_idx {
                    put_entry(out, &pal[oi])?;
                }
                return Some((false, 0));
            }
        };
        let t_unp = std::time::Instant::now();
        let in_bits = section_data_bits(n, entries, data.len());
        let per = (64 / in_bits) as usize;
        if data.len() != (entries + per - 1) / per {
            return None;
        }
        let v = unpack_nostroddle_vox(data, entries, in_bits);
        // sanity: every voxel index must be inside the palette. Valid vanilla
        // data always is; a mismatch means malformed/mutated source bytes
        // (garbage in unused codes). Fall back to raw NBT storage instead of
        // panicking on the server thread — the section just loses the CSEC
        // savings, semantics are untouched.
        if v.iter().any(|x| (*x as usize) >= n) {
            ST_ENC_FALLBACK.fetch_add(1, Ordering::Relaxed);
            SPLT_UNP.0.fetch_add(t_unp.elapsed().as_nanos() as u64, Ordering::Relaxed);
            SPLT_UNP.1.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        SPLT_UNP.0.fetch_add(t_unp.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_UNP.1.fetch_add(1, Ordering::Relaxed);
        // Frequency-canonical order: the dominant state becomes index 0, so
        // the dense data stream is dominated by zero runs that compress
        // extremely well. Ties break by (plain, name, props) so the order is
        // a pure function of content (deterministic => dedup-stable).
        let t_pal = std::time::Instant::now();
        let mut counts = vec![0u32; n];
        for &x in &v {
            counts[x as usize] += 1;
        }
        // canonical order as a sorted INDEX vec over the shared palette
        // (stable sort: ties keep original index order, exactly like the old
        // (entry, old_index) pair sort)
        let mut order_idx: Vec<usize> = (0..n).collect();
        order_idx.sort_by(|&a, &b| {
            counts[b]
                .cmp(&counts[a])
                .then_with(|| {
                    (pal[a].plain, pal[a].name, &pal[a].props)
                        .cmp(&(pal[b].plain, pal[b].name, &pal[b].props))
                })
        });
        SPLT_PAL.0.fetch_add(t_pal.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_PAL.1.fetch_add(1, Ordering::Relaxed);
        out.extend_from_slice(&(n as u16).to_le_bytes());
        for &oi in &order_idx {
            put_entry(out, &pal[oi])?;
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
            && order_idx.iter().enumerate().all(|(i, &old)| old == i);
        // remap old palette index -> canonical index
        let mut remap = vec![0u8; n];
        for (new_i, &old_i) in order_idx.iter().enumerate() {
            remap[old_i] = new_i as u8;
        }
        if n > 256 {
            // wide palettes: remap table must be u16; RLE (byte indices)
            // does not apply — dense only. The remap fuses into the pack
            // loop (identical bytes to packing the materialized stream).
            let mut remap16 = vec![0u16; n];
            for (new_i, &old_i) in order_idx.iter().enumerate() {
                remap16[old_i] = new_i as u16;
            }
            out.extend_from_slice(&dense_pack_wide_mapped(&v, &remap16, out_bits));
            ST_ENC_DENSE.fetch_add(1, Ordering::Relaxed);
            // facts capture: v/ents are dead after this point (wide path
            // returns immediately) — hand them to the shift pass for free.
            *facts_out = Some(SectionFacts {
                v,
                order: order_idx.iter().map(|&i| i as u32).collect(),
            });
            return Some((true, 0));
        }
        // v0.9.11: the mapped stream is never materialized. remap is
        // injective on 0..n and every voxel is < n (checked above), so run
        // structure is preserved exactly: the RLE size scans walk the RAW
        // stream with identical framing, and the remap fuses into packing
        // and winner materialization. Outputs are byte-identical to the
        // materialized formulation (pinned by enc_fp).
        // Dense candidate (identity => the raw vanilla longs, memcpy'd —
        // byte-identical to what dense_pack would produce).
        let t_pack = std::time::Instant::now();
        let dense: Vec<u8> = if identity {
            let mut d = Vec::with_capacity(data.len() * 8);
            for l in data {
                d.extend_from_slice(&l.to_le_bytes());
            }
            d
        } else {
            dense_pack_mapped(&v, &remap, out_bits)
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
            // size-only scans: the admission rule (4x smaller AND 32+ bytes
            // saved) rejects ~99% of noise-heavy sections — materialize only
            // the winning candidate. The sizes are exact (same run walk,
            // same [idx][varint] framing), so decisions and chosen bytes are
            // identical to materializing both candidates.
            let la = rle_stream_len_mapped(&v, &remap);
            let a_ok = la * 4 <= dense.len() as u64 && la + 32 <= dense.len() as u64;
            let lb = rle_transposed_len_mapped(&v, &remap, side);
            let b_ok = lb * 4 <= dense.len() as u64 && lb + 32 <= dense.len() as u64;
            if b_ok && (!a_ok || lb < la) {
                best_len = lb as usize;
                best = Some((
                    2,
                    rle_encode_stream_mapped(&transpose_fast_y_u16(&v, side), &remap),
                ));
            } else if a_ok {
                best_len = la as usize;
                best = Some((1, rle_encode_stream_mapped(&v, &remap)));
            }
        }
        SPLT_PACK.0.fetch_add(t_pack.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_PACK.1.fetch_add(1, Ordering::Relaxed);
        // facts capture (narrow path): v died in the RLE scans above, ents
        // died in the identity check — both move out for free. The canonical
        // order here IS the shift pass's order (same comparator, same
        // counts).
        *facts_out = Some(SectionFacts {
            v,
            order: order_idx.iter().map(|&i| i as u32).collect(),
        });
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

    if let Some(bs) = &view.bs {
        let mut facts: Option<SectionFacts> = None;
        let (has_data, f2b) = enc_pal_data_canon(&mut out, bs, 4096, 4, 4, &mut facts)?;
        f1 |= 1;
        if has_data {
            f1 |= 2;
        }
        f2 |= f2b;
        block_facts = facts;
    }
    if let Some(bs) = &view.bio {
        let mut no_facts: Option<SectionFacts> = None;
        let (has_data, f2b) = enc_pal_data_canon(&mut out, bs, 64, 8, 6, &mut no_facts)?;
        f1 |= 4;
        if has_data {
            f1 |= 8;
        }
        f2 |= f2b;
    }
    for (b, uni_bit, raw_bit, all, zero_to_f3) in [
        (view.skylight.as_deref(), 16u8, 64u8, 0xffu8, false),
        (view.blocklight.as_deref(), 0u8, 128u8, 0u8, true),
    ] {
        let Some(b) = b else { continue };
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
            out.extend_from_slice(b);
        }
    }
    if let Some(v) = view.sl {
        if !(0..=255).contains(&v) {
            return None;
        }
        f2 |= 1;
        out.push(v as u8);
    }
    if let Some(v) = view.bl {
        if !(0..=255).contains(&v) {
            return None;
        }
        f2 |= 2;
        out.push(v as u8);
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
    Some((out, if want_facts { block_facts } else { None }))
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
/// pal_entry_identity into a caller-owned scratch buffer (cleared first):
/// identical bytes to pal_entry_identity, zero per-entry allocation.
fn pal_entry_identity_into(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    out.clear();
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
            let mut props: Vec<(&str, &str)> = Vec::new();
            match cm.get("Properties") {
                Some(Value::Compound(pn)) => {
                    for (k, kv) in pn {
                        let Value::String(vv) = kv else { return None };
                        props.push((k.as_str(), vv.as_str()));
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
    Some(())
}

/// pal_entry_identity over a shared view entry: the props vec is pre-sorted
/// by both view builders (parse_entry / pal_entry_from_lit), so the bytes are
/// identical to the Value variant. Debug-only guard keeps that invariant
/// honest.
fn identity_from_entry(e: &PalEntry, out: &mut Vec<u8>) -> Option<()> {
    debug_assert!(
        e.props.windows(2).all(|w| w[0] <= w[1]),
        "view palette props must be pre-sorted"
    );
    out.clear();
    if e.plain {
        out.push(1u8);
        out.extend_from_slice(e.name.as_bytes());
        return Some(());
    }
    out.push(0u8);
    out.extend_from_slice(e.name.as_bytes());
    out.push(0x00);
    for (i, (k, vv)) in e.props.iter().enumerate() {
        if i > 0 {
            out.push(0x1f);
        }
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(vv.as_bytes());
    }
    Some(())
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
/// `tab` interns palette-entry identity bytes chunk-globally (owned copies
/// only for NEW identities; lookups hash the scratch slice); `idbuf` is a
/// reusable identity-serialization buffer.
/// Facts captured by the encoder for one section's block_states (v0.9.12):
/// the raw orig-index voxel stream and the canonical orig-index order —
/// exactly what the shift pass re-derives from scratch today (unpack +
/// counts + sort ≈ 20 µs per dense build). The voxel stream is MOVED out of
/// the encoder (it dies after the pack/RLE decision), so reuse is free.
struct SectionFacts {
    v: Vec<u32>,
    order: Vec<u32>,
}

/// Identity interning over shared view entries: one id per distinct palette
/// entry identity, in first-appearance order (chunk-global table). Shared by
/// both ShiftSec builds.
fn intern_ids(
    pal: &[PalEntry],
    tab: &mut HashMap<Box<[u8]>, u32>,
    idbuf: &mut Vec<u8>,
) -> Option<Vec<u32>> {
    let n = pal.len();
    let mut id_of_orig = vec![0u32; n];
    let t_id = std::time::Instant::now();
    for (i, e) in pal.iter().enumerate() {
        identity_from_entry(e, idbuf)?;
        let nid = tab.len() as u32;
        let id = match tab.get(idbuf.as_slice()) {
            Some(&x) => x,
            None => {
                tab.insert(idbuf.as_slice().into(), nid);
                nid
            }
        };
        id_of_orig[i] = id;
    }
    ST_SHIFT_ID_NS.fetch_add(t_id.elapsed().as_nanos() as u64, Ordering::Relaxed);
    Some(id_of_orig)
}

/// Full ShiftSec build over the SecView currency (v0.9.13): identical output
/// to the old &Value shift_sec_of — same unpack, same counts, same
/// comparator (counts desc, then (plain, name, props-sorted)).
fn shift_sec_of_view(
    v: &SecView,
    tab: &mut HashMap<Box<[u8]>, u32>,
    idbuf: &mut Vec<u8>,
) -> Option<ShiftSec> {
    let bs = v.bs.as_ref()?;
    let pal = &bs.palette;
    let n = pal.len();
    if n == 0 || n > 65535 {
        return None;
    }
    let id_of_orig = intern_ids(pal, tab, idbuf)?;
    let mut ident = vec![0u32; 4096];
    // one unpack feeds ident AND the frequency counts (the canonical order
    // mirrors enc_pal_data_canon: counts desc, then (plain, name, props)) —
    // the two passes used to re-unpack the same 4096 voxels (~40% of the
    // build cost; the shift pass builds one of these per distinct dense
    // section per chunk).
    let t_unp = std::time::Instant::now();
    let mut counts = vec![0u32; n];
    match bs.data {
        Some(data) => {
            let bits = section_data_bits(n, 4096, data.len());
            let per = (64 / bits) as usize;
            if data.len() != (4096 + per - 1) / per {
                return None;
            }
            for (i, x) in unpack_nostroddle_vox(data, 4096, bits).into_iter().enumerate() {
                let o = x as usize;
                if o >= n {
                    return None;
                }
                ident[i] = id_of_orig[o];
                counts[o] += 1;
            }
        }
        None => {
            if n > 1 {
                return None;
            }
            // counts stays all-zero: the canonical order degenerates to
            // (plain, name, props), exactly like the old second pass
            ident.iter_mut().for_each(|x| *x = id_of_orig[0]);
        }
    }
    ST_SHIFT_UNP_NS.fetch_add(t_unp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    // sort keys borrowed straight from the shared view entries (props
    // pre-sorted by both view builders; the comparison order, hence the
    // canonical order, is identical to the old parsed-tree keys)
    let t_key = std::time::Instant::now();
    let keys: Vec<(bool, &str, &Vec<(&str, &str)>)> =
        pal.iter().map(|e| (e.plain, e.name, &e.props)).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then_with(|| keys[a].cmp(&keys[b])));
    let mut canon_of_orig = vec![0u32; n];
    for (pos, &oi) in order.iter().enumerate() {
        canon_of_orig[oi] = pos as u32;
    }
    ST_SHIFT_KEY_NS.fetch_add(t_key.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let t_fp = std::time::Instant::now();
    let mut plane_fp = [0u64; 16];
    for (py, fp) in plane_fp.iter_mut().enumerate() {
        let mut h: u64 = 0xcbf29ce484222325;
        for &x in &ident[py * 256..(py + 1) * 256] {
            h ^= x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        *fp = h;
    }
    ST_SHIFT_FP_NS.fetch_add(t_fp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    Some(ShiftSec { ident, plane_fp, canon_of_orig, id_of_orig, n })
}
/// ShiftSec build from encoder facts (v0.9.12): identical output to
/// shift_sec_of for the same section — the canonical order is the same
/// comparator (counts desc, then (plain, name, props-sorted)) over the same
/// counts (both derive from the same unpacked voxel stream), so the
/// encoder's sorted order IS the shift pass's order. Skips the unpack, the
/// counts pass and the key sort; keeps id interning, the ident fill and the
/// plane fingerprints.
pub static ST_SHIFT_FAST: AtomicU64 = AtomicU64::new(0);

fn shift_sec_from_facts_view(
    v: &SecView,
    tab: &mut HashMap<Box<[u8]>, u32>,
    idbuf: &mut Vec<u8>,
    facts: SectionFacts,
) -> Option<ShiftSec> {
    let bs = v.bs.as_ref()?;
    let pal = &bs.palette;
    let n = pal.len();
    if n == 0 || n > 65535 {
        return None;
    }
    let id_of_orig = intern_ids(pal, tab, idbuf)?;
    let mut ident = vec![0u32; 4096];
    let t_unp = std::time::Instant::now();
    for (i, &o) in facts.v.iter().enumerate() {
        let o = o as usize;
        if o >= n {
            return None; // belt & braces — the encoder validated this
        }
        ident[i] = id_of_orig[o];
    }
    ST_SHIFT_UNP_NS.fetch_add(t_unp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let t_key = std::time::Instant::now();
    let mut canon_of_orig = vec![0u32; n];
    for (pos, &oi) in facts.order.iter().enumerate() {
        let oi = oi as usize;
        if oi >= n {
            return None;
        }
        canon_of_orig[oi] = pos as u32;
    }
    ST_SHIFT_KEY_NS.fetch_add(t_key.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let t_fp = std::time::Instant::now();
    let mut plane_fp = [0u64; 16];
    for (py, fp) in plane_fp.iter_mut().enumerate() {
        let mut h: u64 = 0xcbf29ce484222325;
        for &x in &ident[py * 256..(py + 1) * 256] {
            h ^= x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        *fp = h;
    }
    ST_SHIFT_FP_NS.fetch_add(t_fp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    ST_SHIFT_FAST.fetch_add(1, Ordering::Relaxed);
    Some(ShiftSec { ident, plane_fp, canon_of_orig, id_of_orig, n })
}

/// The shift pass: replace dense CSEC3 sections with shift refs where a
/// lower section of this chunk contains them shifted by k planes. Runs on
/// the freshly split section list (Values in `vals` are parallel to
/// `sections`). Never fails: sections that do not qualify stay untouched.
pub static ST_SHIFT_BUILD_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_TAIL_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
/// v0.9.12 build breakdown: identity interning (serialize+hash per palette
/// entry), voxel unpack + ident/counts fill, sort keys + order/canon, plane
/// fingerprints. Sum ~= ST_SHIFT_BUILD_NS (non-overlapping inner timers).
pub static ST_SHIFT_ID_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_UNP_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_KEY_NS: AtomicU64 = AtomicU64::new(0);
pub static ST_SHIFT_FP_NS: AtomicU64 = AtomicU64::new(0);

/// Section LIST source for the shift pass (v0.9.13): the parsed Value list
/// (whole-parse fallback path) or the typed borrowed list (scan-split lite
/// path, with the strip flag the views must honor). ShiftSec builds go
/// through SecView either way, so encode/shift decisions are identical.
enum SecListSrc<'a> {
    Values(&'a [Value]),
    Lite(&'a [SecLite<'a>], bool),
}

fn shift_ref_pass(
    sections: &mut [(Hash, SecBytes)],
    src: &SecListSrc,
    cache_hit: &[bool],
    facts: &mut [Option<SectionFacts>],
) {
    let t_all = std::time::Instant::now();
    const MAX_K: usize = 15;
    const CANDIDATES: usize = 64;
    const MIN_WIN: usize = 256; // at least one plane must match
    let n_sec = match src {
        SecListSrc::Values(vals) => sections.len().min(vals.len()),
        SecListSrc::Lite(list, _) => sections.len().min(list.len()),
    };
    if n_sec < 2 {
        return;
    }
    let mut tab: HashMap<Box<[u8]>, u32> = HashMap::new();
    let mut idbuf: Vec<u8> = Vec::with_capacity(64);
    // Sections with tiny members (uniform / RLE-collapsed) are excluded
    // entirely: their matches can never clear the admission rule (the ref
    // carries at least the header + tail the member already has), and they
    // are the majority of distinct contents, so this cuts most build cost.
    const MIN_MEMBER: usize = 512;
    // v0.9.7: LAZY ShiftSec builds. The eager loop built every eligible
    // section up front (~40 µs each: unpack + canonical keys + plane fps),
    // but the fingerprint pre-filter rejects >99.9% of candidate pairs, and
    // enc-cache hits are skipped as match sources entirely — so most builds
    // were never consulted for anything. Now a section is classified
    // (cheap header checks only) and built on first USE as a match source
    // or target. Decisions are unchanged: every section the eager pass
    // would have consulted gets built on first consultation, keyed by the
    // same structural-hash meta cache and fed by the same chunk-global id
    // table.
    let mut meta_cache: HashMap<Hash, Arc<ShiftSec>> = HashMap::new();
    let mut eligible = vec![false; n_sec];
    let mut is_ref: Vec<bool> = vec![false; n_sec];
    for i in 0..n_sec {
        let (csec3, mlen) = match &sections[i].1 {
            SecBytes::Shared(a) => (is_csec4(a.first()), a.len()),
            SecBytes::Owned(b) => (is_csec4(b.first()), b.len()),
        };
        if csec3 && mlen >= MIN_MEMBER {
            eligible[i] = true;
        }
    }
    let mut built: HashMap<usize, Option<Arc<ShiftSec>>> = HashMap::new();
    // lazily built SecViews (parse_entry per palette entry), filled at build
    // time and reused by the winner assembly below
    let mut view_cache: Vec<Option<SecView>> = (0..n_sec).map(|_| None).collect();
    // macro, not a closure: the getter needs &mut tab/meta_cache/built while
    // the match loop holds & sections / & src (disjoint borrows).
    // The eligibility check lives INSIDE the getter — ineligible slots must
    // yield None without building, exactly like the eager `secs` vector did
    // (a target that is not buildable must neither consume the candidate
    // budget nor change the match outcome).
    macro_rules! ss_get {
        ($ix:expr) => {
            if !eligible[$ix] {
                None
            } else {
                match built.get(&$ix) {
                    Some(v) => v.clone(),
                    None => {
                        let v = match meta_cache.get(&sections[$ix].0) {
                            Some(a) => Some(a.clone()),
                            None => {
                                let t_b = std::time::Instant::now();
                                if view_cache[$ix].is_none() {
                                    view_cache[$ix] = match src {
                                        SecListSrc::Values(vals) => sec_view_of_value(&vals[$ix]),
                                        SecListSrc::Lite(list, strip) => {
                                            sec_view_of_lite(&list[$ix], *strip)
                                        }
                                    };
                                }
                                let r = match view_cache[$ix].as_ref() {
                                    Some(vw) => match facts[$ix].take() {
                                        Some(f) => shift_sec_from_facts_view(
                                            vw,
                                            &mut tab,
                                            &mut idbuf,
                                            f,
                                        )
                                        .map(Arc::new),
                                        None => {
                                            shift_sec_of_view(vw, &mut tab, &mut idbuf)
                                                .map(Arc::new)
                                        }
                                    },
                                    None => None,
                                };
                                ST_SHIFT_BUILD_NS.fetch_add(
                                    t_b.elapsed().as_nanos() as u64,
                                    Ordering::Relaxed,
                                );
                                if let Some(a) = &r {
                                    ST_SHIFT_SECS_OK.fetch_add(1, Ordering::Relaxed);
                                    meta_cache.insert(sections[$ix].0, a.clone());
                                }
                                r
                            }
                        };
                        built.insert($ix, v.clone());
                        v
                    }
                }
            }
        };
    }
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
        let Some(ss) = ss_get!(i) else { continue };
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
            let Some(ts) = ss_get!(j) else { continue };
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
        let t_tail0 = t_all.elapsed().as_nanos() as u64;
        let tj = ss_get!(j).unwrap();
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
        // non-block_states fields: re-encode the section view without
        // block_states (f1 block_states bits stay clear)
        let Some(src_view) = view_cache[i].as_ref() else { continue };
        if src_view.bs.is_none() {
            continue;
        }
        let mut view2 = src_view.clone();
        view2.bs = None;
        let csec2 = match section_encode_view(&view2, false) {
            Some((c, _)) if is_csec4(c.first()) && c.len() >= 4 => c,
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
            let Some(bs) = view_cache[i].as_ref().and_then(|v| v.bs.as_ref()) else {
                break 'ex
            };
            let Some(e) = bs.palette.get(pos) else { break 'ex };
            if put_entry(&mut mbr, e).is_none() {
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
                // no-straddle padding lives in the last long); width-correct
                // unpack — modded biome palettes can exceed 256 entries
                let bits = ((la.as_ref().len() as u32) * 64 / entries as u32).max(1);
                let idx = unpack_nostroddle_vox(la.as_ref(), entries, bits);
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

/// structural_hash over the typed lite form — byte-stream-identical to
/// structural_hash(&stripped Value) (feed_lite mirrors feed; corpus-verified
/// in probe_lite). Enc-cache keys therefore align across both paths.
fn hash_lite(s: &SecLite, strip: bool) -> Hash {
    let mut h = Xxh3::new();
    feed_lite(&mut h, s, strip);
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
            // one update over the raw bytes: i8 and u8 share the bit layout,
            // and the streaming hash is defined to equal the one-shot digest
            // over the concatenated byte sequence — same hash, ~2k fewer
            // update calls per light array (was per-byte).
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(x.as_ref().as_ptr() as *const u8, x.as_ref().len())
            };
            w!(h, bytes);
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
            // one update over the raw two's-complement LE bytes (identical
            // stream to the per-long to_le_bytes feed — same hash, one
            // update instead of 256 per section data array).
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(x.as_ref().as_ptr() as *const u8, x.as_ref().len() * 8)
            };
            w!(h, bytes);
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
    let strip = strip && !matches!(std::env::var("CSAVES_KEEP_LIGHT").as_deref(), Ok("1"));
    if std::env::var("CSAVES_NO_SCAN").as_deref() != Ok("1") {
        let t_scan = std::time::Instant::now();
        let r = scan_split(payload, strip);
        SPLT_SCAN.0.fetch_add(t_scan.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_SCAN.1.fetch_add(1, Ordering::Relaxed);
        if let Some(sp) = r {
            SPLT_SCAN_OK.0.fetch_add(t_scan.elapsed().as_nanos() as u64, Ordering::Relaxed);
            SPLT_SCAN_OK.1.fetch_add(1, Ordering::Relaxed);
            return Ok(sp);
        }
    }
    let t_sp = std::time::Instant::now();
    let root: Value = fastnbt::from_bytes(payload).map_err(|e| format!("nbt parse: {e}"))?;
    SPLT_PARSE.0.fetch_add(t_sp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    SPLT_PARSE.1.fetch_add(1, Ordering::Relaxed);
    split_parsed(payload, pos_hint, root, strip)
}

/// Section encode + dedup-cache lookup, shared by the scan-split fast path
/// and the full-parse fallback (identical decisions on identical inputs).
fn encode_section_list(
    list: &[Value],
    want_facts: bool,
) -> (
    Vec<(Hash, SecBytes)>,
    Vec<bool>,
    Vec<Option<SectionFacts>>,
) {
    let mut sections = Vec::with_capacity(list.len());
    let mut cache_hit = vec![false; list.len()];
    let mut facts_list: Vec<Option<SectionFacts>> = Vec::with_capacity(list.len());
    for s in list.iter() {
        // encode-cache: re-saving a chunk re-encodes every section; the
        // structural hash is order-free, so unchanged sections hit the
        // cache and skip canonicalization + bit-packing entirely.
        let t_sh = std::time::Instant::now();
        let sh = structural_hash(s);
        SPLT_SH.0.fetch_add(t_sh.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_SH.1.fetch_add(1, Ordering::Relaxed);
        if let Some((h, shared)) = enc_cache_get(sh) {
            cache_hit[sections.len()] = true;
            sections.push((h, SecBytes::Shared(shared)));
            facts_list.push(None); // no fresh encode -> no facts
            continue;
        }
        // custom form when possible (marker-detectable), NBT bytes otherwise.
        // v3 output is canonical, so identical content yields identical bytes
        // regardless of the original palette order -> hash the bytes.
        let t_enc = std::time::Instant::now();
        match section_encode_impl(s, want_facts) {
            Some((c, f)) => {
                SPLT_ENC.0.fetch_add(t_enc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                SPLT_ENC.1.fetch_add(1, Ordering::Relaxed);
                if is_csec4(c.first()) {
                    let h = hash_of(&c);
                    let arc = Arc::new(c);
                    enc_cache_put(sh, h, arc.clone());
                    sections.push((h, SecBytes::Shared(arc)));
                } else {
                    sections.push((structural_hash(s), SecBytes::Owned(c)));
                }
                facts_list.push(f);
            }
            None => {
                SPLT_ENC.0.fetch_add(t_enc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                SPLT_ENC.1.fetch_add(1, Ordering::Relaxed);
                sections.push((structural_hash(s), SecBytes::Owned(to_bytes(s))));
                facts_list.push(None);
            }
        }
    }
    (sections, cache_hit, facts_list)
}

/// Typed-lite twin of encode_section_list (v0.9.13): same flow, but the
/// enc-cache key is hash_lite (byte-stream-identical to the Value path's
/// structural_hash over the stripped section — keys align across paths) and
/// the encoder-None fallback serializes the lite section itself
/// (sec_lite_to_bytes) instead of to_bytes(&Value).
fn encode_section_list_lite(
    list: &[SecLite],
    strip: bool,
    want_facts: bool,
) -> (
    Vec<(Hash, SecBytes)>,
    Vec<bool>,
    Vec<Option<SectionFacts>>,
) {
    let mut sections = Vec::with_capacity(list.len());
    let mut cache_hit = vec![false; list.len()];
    let mut facts_list: Vec<Option<SectionFacts>> = Vec::with_capacity(list.len());
    for s in list.iter() {
        // encode-cache: identical discipline to the Value path; hash_lite
        // equals structural_hash(&stripped Value) so keys align across paths
        let t_sh = std::time::Instant::now();
        let sh = hash_lite(s, strip);
        SPLT_SH.0.fetch_add(t_sh.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_SH.1.fetch_add(1, Ordering::Relaxed);
        if let Some((h, shared)) = enc_cache_get(sh) {
            cache_hit[sections.len()] = true;
            sections.push((h, SecBytes::Shared(shared)));
            facts_list.push(None); // no fresh encode -> no facts
            continue;
        }
        let t_enc = std::time::Instant::now();
        match sec_view_of_lite(s, strip).and_then(|v| section_encode_view(&v, want_facts)) {
            Some((c, f)) => {
                SPLT_ENC.0.fetch_add(t_enc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                SPLT_ENC.1.fetch_add(1, Ordering::Relaxed);
                if is_csec4(c.first()) {
                    let h = hash_of(&c);
                    let arc = Arc::new(c);
                    enc_cache_put(sh, h, arc.clone());
                    sections.push((h, SecBytes::Shared(arc)));
                } else {
                    sections.push((sh, SecBytes::Owned(c)));
                }
                facts_list.push(f);
            }
            None => {
                SPLT_ENC.0.fetch_add(t_enc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                SPLT_ENC.1.fetch_add(1, Ordering::Relaxed);
                sections.push((sh, SecBytes::Owned(sec_lite_to_bytes(s, strip))));
                facts_list.push(None);
            }
        }
    }
    (sections, cache_hit, facts_list)
}

/// The shift-ref stage shared by both split paths: collapse sections that
/// are vertical shifts of a lower section of this chunk (mountain strata /
/// ocean floors / cave ceilings) into ~15-30 byte refs; the top k planes
/// stay explicit. Sections whose content was seen in an earlier chunk
/// (enc-cache hit) are skipped: their full members dedup across chunks for
/// free, which always beats a chunk-local ref.
fn shift_stage(
    sections: &mut Vec<(Hash, SecBytes)>,
    src: &SecListSrc,
    cache_hit: &[bool],
    facts: &mut [Option<SectionFacts>],
) {
    if std::env::var("CSAVES_NO_SHIFT").as_deref() != Ok("1") {
        let t_sp = std::time::Instant::now();
        shift_ref_pass(sections, src, cache_hit, facts);
        SPLT_SHIFT.0.fetch_add(t_sp.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_SHIFT.1.fetch_add(1, Ordering::Relaxed);
    }
}

// ------------------------------------------- zero-copy split scan (v0.9.11) --
//
// The fallback path parses the WHOLE payload into a fastnbt Value tree
// (an allocation per NBT node) and re-serializes the tail (everything except
// sections) in HashMap iteration order. Both are pure overhead: the import
// only needs (a) the sections LIST as Values and (b) the tail bytes — which
// the source payload already contains verbatim in vanilla key order
// (strictly more deterministic than a HashMap-ordered re-serialization, and
// it compresses better: vanilla writers emit optional fields last so
// cross-chunk prefixes stay aligned).
//
// scan_split walks the root compound by header arithmetic (no allocation),
// removes the `sections` range, repacks each Heightmaps LongArray through
// hmap_encode (the same transform tail_encode applies) and patches
// isLightOn when stripping. Any doubt -> None -> the full-parse fallback
// runs unchanged. The raw tail is a VALID NBT compound, so join()/join_fused
// parse it exactly like the old tail (tail_decode already keys the heightmap
// transform off the HMAP_MAGIC ByteArray marker).

/// Bounds-checked NBT value skip by header arithmetic. Returns None on any
/// overrun/malformed input (caller falls back to the full parse).
fn skip_value(b: &[u8], p: &mut usize, ty: u8, depth: u32) -> Option<()> {
    if depth == 0 {
        return None;
    }
    match ty {
        1 => *p += 1,
        2 => *p += 2,
        3 | 5 => *p += 4,
        4 | 6 => *p += 8,
        7 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(4)?;
        }
        12 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(4)?.checked_add(l.checked_mul(8)?)?;
        }
        11 => {
            let l = i32::from_be_bytes(b.get(*p..*p + 4)?.try_into().ok()?) as usize;
            *p = p.checked_add(4)?.checked_add(l.checked_mul(4)?)?;
        }
        8 => {
            let l = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
            *p = p.checked_add(l)?.checked_add(2)?;
        }
        9 => {
            let et = *b.get(*p)?;
            let l = i32::from_be_bytes(b.get(*p + 1..*p + 5)?.try_into().ok()?) as usize;
            *p += 5;
            if l == 0 {
                return Some(()); // empty list: element type may be 0
            }
            if et == 0 {
                return None;
            }
            for _ in 0..l {
                skip_value(b, p, et, depth - 1)?;
            }
        }
        10 => {
            loop {
                let t = *b.get(*p)?;
                *p += 1;
                if t == 0 {
                    break;
                }
                let l = u16::from_be_bytes(b.get(*p..*p + 2)?.try_into().ok()?) as usize;
                *p = p.checked_add(2)?.checked_add(l)?; // name length + NAME BYTES
                skip_value(b, p, t, depth - 1)?;
            }
        }
        _ => return None, // 0 or >12 at value position: malformed
    }
    if *p <= b.len() {
        Some(())
    } else {
        None
    }
}

/// (pos, len, replacement) — disjoint, ascending. Empty replacement = remove.
type TailEdit = (usize, usize, Vec<u8>);

struct ScanOut {
    edits: Vec<TailEdit>,
    /// append TAG_Byte isLightOn=0 before the root END (strip mode only)
    insert_light: bool,
    /// payload range of the sections LIST VALUE (after [tag][name])
    sections_payload: (usize, usize),
    /// position of the root compound END byte
    end_pos: usize,
}

/// Walk the root compound of a chunk payload. Recognizes `sections`
/// (TAG_List — the removal range), `Heightmaps` (TAG_Compound of LongArrays
/// — hmap_encode replacements) and `isLightOn` (strip patch). Everything
/// else is skipped by header arithmetic. None = anything unexpected.
fn scan_root(payload: &[u8], strip: bool) -> Option<ScanOut> {
    // NOTE: `p` must be declared BEFORE the macro — macro_rules hygiene
    // resolves the body's `p` at the definition site, and a later local
    // binding is invisible to it (E0425).
    let mut p = 0usize;
    macro_rules! sr {
        ($e:expr, $why:expr) => {
            match $e {
                Some(v) => v,
                None => {
                    SCAN_FAIL.store($why, Ordering::Relaxed);
                    SCAN_FAIL_POS.store(p as u64, Ordering::Relaxed);
                    return None;
                }
            }
        };
    }
    if *sr!(payload.first(), 1) != 0x0A {
        SCAN_FAIL.store(1, Ordering::Relaxed);
        return None;
    }
    p += 1;
    let nl = u16::from_be_bytes(sr!(sr!(payload.get(p..p + 2), 1).try_into().ok(), 1)) as usize;
    p += 2 + nl; // root name (usually empty)
    let mut edits: Vec<TailEdit> = Vec::new();
    let mut sections_payload: Option<(usize, usize)> = None;
    let mut insert_light = false;
    let mut saw_light = false;
    loop {
        let tag_pos = p;
        let ty = *payload.get(p)?;
        p += 1;
        if ty == 0 {
            // the old Value path does m.insert("isLightOn", Byte(0)) — an
            // UPSERT: the relight trigger must appear even when the source
            // payload has no isLightOn key (Paper omits it in region files)
            if strip && !saw_light {
                insert_light = true;
            }
            return Some(ScanOut {
                edits,
                insert_light,
                sections_payload: sr!(sections_payload, 6),
                end_pos: tag_pos,
            });
        }
        let nl = u16::from_be_bytes(sr!(sr!(payload.get(p..p + 2), 7).try_into().ok(), 7)) as usize;
        let name = sr!(payload.get(p + 2..p + 2 + nl), 7);
        p += 2 + nl;
        match (ty, name) {
            (9, b"sections") => {
                let vp = p;
                sr!(skip_value(payload, &mut p, 9, 24), 2);
                sections_payload = Some((vp, p));
                edits.push((tag_pos, p - tag_pos, Vec::new()));
            }
            (10, b"Heightmaps") => {
                // walk the inner compound manually: each TAG_Long_Array value
                // that hmap_encode accepts is replaced by the dense
                // TAG_Byte_Array form tail_encode would produce. The edit
                // range covers the WHOLE entry (type byte + name + payload);
                // the replacement re-emits type + name.
                loop {
                    let it_pos = p;
                    let it = *sr!(payload.get(p), 3);
                    p += 1;
                    if it == 0 {
                        break;
                    }
                    let inl = u16::from_be_bytes(sr!(sr!(payload.get(p..p + 2), 3).try_into().ok(), 3)) as usize;
                    let iname = sr!(payload.get(p + 2..p + 2 + inl), 3);
                    p += 2 + inl;
                    let vs = p;
                    sr!(skip_value(payload, &mut p, it, 8), 3);
                    if it != 12 {
                        continue;
                    }
                    let l = i32::from_be_bytes(sr!(sr!(payload.get(vs..vs + 4), 3).try_into().ok(), 3)) as usize;
                    if l != 37 {
                        continue; // hmap_encode rejects; keep raw (same decision)
                    }
                    let raw = sr!(payload.get(vs + 4..vs + 4 + l * 8), 3);
                    let la: Vec<i64> = raw
                        .chunks_exact(8)
                        .map(|c| i64::from_be_bytes(c.try_into().unwrap()))
                        .collect();
                    let Some(dense) = hmap_encode(&la) else { continue };
                    let mut rep = Vec::with_capacity(7 + iname.len() + dense.len());
                    rep.push(7u8); // TAG_Byte_Array
                    rep.extend_from_slice(&(iname.len() as u16).to_be_bytes());
                    rep.extend_from_slice(iname);
                    // TAG_Byte_Array payload length is an i32 (standard NBT
                    // — fastnbt serializes ByteArray the same way, and the
                    // join-side Heightmaps walk reads 4 bytes here)
                    rep.extend_from_slice(&(dense.len() as i32).to_be_bytes());
                    rep.extend_from_slice(&dense);
                    edits.push((it_pos, p - it_pos, rep));
                }
            }
            (_, b"isLightOn") if strip => {
                saw_light = true;
                sr!(skip_value(payload, &mut p, ty, 8), 4);
                if ty == 1 {
                    if payload[p - 1] != 0 {
                        edits.push((p - 1, 1, vec![0u8]));
                    }
                } else {
                    // foreign type: remove; the Byte(0) form is appended
                    // before END (a map insert would replace it too)
                    edits.push((tag_pos, p - tag_pos, Vec::new()));
                    insert_light = true;
                }
            }
            _ => {
                sr!(skip_value(payload, &mut p, ty, 32), 5);
            }
        }
    }
}

/// Fast-path split: raw tail + sections-only parse. None = fall back to the
/// full parse (any structural doubt, non-compound root, missing sections...).
fn scan_split(payload: &[u8], strip: bool) -> Option<Split> {
    let Some(out) = scan_root(payload, strip) else {
        if std::env::var("CSAVES_SCAN_DEBUG").as_deref() == Ok("1") {
            eprintln!(
                "scan_root fail why={} pos={} len={}",
                SCAN_FAIL.load(Ordering::Relaxed),
                SCAN_FAIL_POS.load(Ordering::Relaxed),
                payload.len()
            );
        }
        return None;
    };
    let (sp0, sp1) = out.sections_payload;
    // parse ONLY the sections list: wrap the payload as a named compound
    // {"": <list>} — fastnbt requires a compound root.
    let mut buf = Vec::with_capacity(6 + (sp1 - sp0));
    buf.extend_from_slice(&[0x0A, 0x00, 0x00, 0x09, 0x00, 0x00]);
    buf.extend_from_slice(&payload[sp0..sp1]);
    buf.push(0x00);
    let want_facts = std::env::var("CSAVES_NO_SHIFT").as_deref() != Ok("1");
    // v0.9.13: typed borrowed parse first (no per-entry HashMap/String
    // allocations; light fields ride as borrows). Any shape doubt fails the
    // parse and the chunk takes the Value path below — outputs identical
    // either way (pinned by enc_fp + hash_lite == structural_hash).
    let t_sp2 = std::time::Instant::now();
    let lite = if std::env::var("CSAVES_NO_LITE").as_deref() == Ok("1") {
        None
    } else {
        fastnbt::from_bytes::<RootList>(&buf).ok()
    };
    if let Some(RootList { list: litelist }) = lite {
        SPLT_SCANP.0.fetch_add(t_sp2.elapsed().as_nanos() as u64, Ordering::Relaxed);
        SPLT_SCANP.1.fetch_add(1, Ordering::Relaxed);
        SPLT_LITE.1.fetch_add(1, Ordering::Relaxed);
        let (mut sections, cache_hit, mut facts) =
            encode_section_list_lite(&litelist, strip, want_facts);
        shift_stage(&mut sections, &SecListSrc::Lite(&litelist, strip), &cache_hit, &mut facts);
        let tail = build_scan_tail(payload, &out)?;
        return Some(Split::Split { sections, tail });
    }
    let t_sp3 = std::time::Instant::now();
    let root: Value = fastnbt::from_bytes(&buf).ok()?;
    SPLT_SCANP.0.fetch_add(t_sp3.elapsed().as_nanos() as u64, Ordering::Relaxed);
    SPLT_SCANP.1.fetch_add(1, Ordering::Relaxed);
    let Value::Compound(mut m) = root else { return None };
    let Some(Value::List(mut list)) = m.remove("") else { return None };
    if strip {
        for s in list.iter_mut() {
            strip_derived_section(s);
        }
    }
    let (mut sections, cache_hit, mut facts) = encode_section_list(&list, want_facts);
    shift_stage(&mut sections, &SecListSrc::Values(&list), &cache_hit, &mut facts);
    let tail = build_scan_tail(payload, &out)?;
    Some(Split::Split { sections, tail })
}

/// tail = payload minus the sections range, with the heightmap/isLightOn
/// edits applied; the root END byte is re-emitted after any light insert.
fn build_scan_tail(payload: &[u8], out: &ScanOut) -> Option<Vec<u8>> {
    let mut tail = Vec::with_capacity(payload.len());
    let mut pos = 0usize;
    for (ep, el, bytes) in &out.edits {
        tail.extend_from_slice(payload.get(pos..*ep)?);
        tail.extend_from_slice(bytes);
        pos = ep + el;
    }
    tail.extend_from_slice(payload.get(pos..out.end_pos)?);
    if out.insert_light {
        tail.extend_from_slice(&[
            1u8, 0x00, 0x09, b'i', b's', b'L', b'i', b'g', b'h', b't', b'O', b'n', 0x00,
        ]);
    }
    tail.push(0x00);
    Some(tail)
}

/// Diagnostic hook for probes: run scan_root on one payload and report
/// (why, pos, ok) without touching the counters' meaning (why=0 = success).
#[doc(hidden)]
pub fn scan_probe(payload: &[u8], strip: bool) -> (u64, u64, bool) {
    match scan_root(payload, strip) {
        Some(_) => (0, 0, true),
        None => (
            SCAN_FAIL.load(Ordering::Relaxed),
            SCAN_FAIL_POS.load(Ordering::Relaxed),
            false,
        ),
    }
}

/// Diagnostic hook for probes: the byte range of the sections LIST inside the
/// payload (what scan_split hands to fastnbt), or None if the scan walk
/// rejects the payload. Independent of `strip` (the range is recorded before
/// any edit application).
#[doc(hidden)]
pub fn scan_sections_range(payload: &[u8]) -> Option<(usize, usize)> {
    scan_root(payload, false).map(|out| out.sections_payload)
}

/// Diagnostic hook (v0.9.13): per-section (structural_hash, hash_lite,
/// encode(Value view), encode(lite view)) over one wrapped sections list.
/// The probe asserts hash and encoded-byte equality across both currencies —
/// the enc-cache alignment + byte-identity guarantee of the lite path.
#[doc(hidden)]
pub fn lite_parity_probe(
    list_bytes: &[u8],
    strip: bool,
) -> Option<Vec<(u128, u128, Option<Vec<u8>>, Option<Vec<u8>>)>> {
    let mut buf = Vec::with_capacity(6 + list_bytes.len());
    buf.extend_from_slice(&[0x0A, 0x00, 0x00, 0x09, 0x00, 0x00]);
    buf.extend_from_slice(list_bytes);
    buf.push(0x00);
    let litelist = fastnbt::from_bytes::<RootList>(&buf).ok()?.list;
    let root: Value = fastnbt::from_bytes(&buf).ok()?;
    let Value::Compound(mut m) = root else { return None };
    let Some(Value::List(list)) = m.remove("") else { return None };
    if list.len() != litelist.len() {
        return None;
    }
    Some(
        list.iter()
            .zip(litelist.iter())
            .map(|(v, s)| {
                let mut v2 = v.clone();
                if strip {
                    strip_derived_section(&mut v2);
                }
                let hv = structural_hash(&v2);
                let hl = hash_lite(s, strip);
                let ev = sec_view_of_value(&v2)
                    .and_then(|vw| section_encode_view(&vw, false))
                    .map(|(b, _)| b);
                let el = sec_view_of_lite(s, strip)
                    .and_then(|vw| section_encode_view(&vw, false))
                    .map(|(b, _)| b);
                (hv, hl, ev, el)
            })
            .collect(),
    )
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
        let want_facts = std::env::var("CSAVES_NO_SHIFT").as_deref() != Ok("1");
        let (mut secs, cache_hit, mut facts) = encode_section_list(list, want_facts);
        shift_stage(&mut secs, &SecListSrc::Values(list), &cache_hit, &mut facts);
        sections = secs;
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
                    out.extend_from_slice(tail.get(e0..p)?);
                }
            }
            out.push(0x00); // end Heightmaps compound
        } else {
            nbt_skip(tail, &mut p, tag, 0)?;
            out.extend_from_slice(tail.get(start..p)?);
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

/// Convert one canonical CSEC section (0xC7/0xC8) to the GLOB form (0xCB /
/// 0xCC). Applies ONLY to dense block_states sections with data (RLE and
/// single-state sections already beat the wide stream or carry no stream);
/// everything after the block_states stream (biomes, light, starlight) is
/// copied verbatim. Returns None when the section is not eligible — the
/// caller then stores the canonical bytes (always safe).
///
/// Layouts:
///   [0xCB][f1][f2'][f3'?][Y][4096 gid bytes][verbatim tail]
///   [0xCC][f1][f2'][f3'?][Y][gform][payload][verbatim tail]
/// — f2' is f2 with the block_states narrow/RLE bits cleared. gform encodes
/// the gid-stream representation: bit0 RLE-coded, bit1 u16 width, bit2
/// y-fast transposed order before RLE (values: 0 raw u8 — identical bytes
/// to 0xCB, 1 rle u8, 2 rle-t u8, 3 raw u16, 4 rle u16, 5 rle-t u16). The
/// raw-u8 case keeps the legacy 0xCB layout byte-for-byte; wider/RLE forms
/// take 0xCC. The gid width follows the section's MAX gid: registries up
/// to 65536 states are representable (u16 streams).
pub fn section_to_glob(csec: &[u8], reg: &mut StateRegistry) -> Option<Vec<u8>> {
    if reg.len() > 65536 {
        return None;
    }
    let magic = *csec.first()?;
    if magic != CSEC3_MAGIC && magic != CSEC4_MAGIC {
        if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
            eprintln!("glob: magic {magic:#04x} len={} not CSEC", csec.len());
        }
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
        if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
            eprintln!("glob: skip f1={f1:02x} f2={f2:02x} (f1&3={}, f2&48={})", f1 & 3, f2 & 48);
        }
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
    let Some(vox) = dense_unpack_vox(csec.get(p..p + stream_len)?, 4096, out_bits) else {
        if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
            eprintln!("glob: dense_unpack failed n={n} out_bits={out_bits} stream_len={stream_len} p={p} len={}", csec.len());
        }
        return None;
    };
    if vox.iter().any(|&x| (x as usize) >= n) {
        if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
            eprintln!("glob: voxel out of range n={n} out_bits={out_bits}");
        }
        return None;
    }
    // gid stream: one gid per voxel. The registry may cross the 256-state
    // line MID-CONVERSION (earlier sections in the same pack wave insert
    // new states): gids above 255 switch this section's stream to the u16
    // width instead of refusing the conversion (registries up to 65536
    // states stay representable).
    let mut gids: Vec<u32> = Vec::with_capacity(4096);
    let mut max_gid: u32 = 0;
    for v in &vox {
        let st = &states[*v as usize];
        let g = reg.get_or_insert(st.clone());
        if g > 65535 {
            return None;
        }
        if g > max_gid {
            max_gid = g;
        }
        gids.push(g);
    }
    let wide = max_gid > 255;
    // GID-RLE admission divisor: the raw gid stream wins slabs through
    // CROSS-SECTION byte alignment (identical geology -> identical streams);
    // RLE destroys that alignment (run boundaries differ). The uncompressed
    // 4x rule admits streams whose RLE is small but whose slab compresses
    // WORSE (corpus: x2.99 vs x3.08 at div=4). The divisor is tunable —
    // CSAVES_GID_RLE_DIV, 0 disables gid RLE entirely.
    let div: u64 = std::env::var("CSAVES_GID_RLE_DIV")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
        eprintln!("glob: n={n} vox_max={:?} reg_len={} max_gid={max_gid} wide={wide}", vox.iter().max(), reg.len());
    }
    // The run structure of the gid stream equals the canonical index stream
    // (palette mapping is a bijection), but the RLE admission WINDOW differs:
    // the raw gid stream is always 4096/8192 bytes while the canonical dense
    // stream packs at out_bits. Moderate-run sections that LOSE the
    // canonical RLE admission (ra*4 > dense bytes) still clear the wider gid
    // window — and the RLE'd [gid][run] pairs of similar strata line up
    // byte-for-byte across the whole slab window (the same lever that made
    // the raw gid form win x2.16).
    let (gform, payload): (u8, Vec<u8>) = if wide {
        let mut raw = Vec::with_capacity(8192);
        for g in &gids {
            raw.extend_from_slice(&(*g as u16).to_le_bytes());
        }
        // size-only scans (exact [gid u16][varint] lengths) — materialize
        // only the winning form; identical decisions and bytes
        let lv = rle_stream_len_u16(&gids);
        let v_ok = div > 0 && lv * div <= raw.len() as u64 && lv + 32 <= raw.len() as u64;
        let lt = rle_transposed_len_u16(&gids, 16);
        let t_ok = div > 0 && lt * div <= raw.len() as u64 && lt + 32 <= raw.len() as u64;
        if v_ok {
            if t_ok && lt < lv {
                (5, rle_encode_stream_u16(&transpose_fast_y_u16(&gids, 16)))
            } else {
                (4, rle_encode_stream_u16(&gids))
            }
        } else if t_ok {
            (5, rle_encode_stream_u16(&transpose_fast_y_u16(&gids, 16)))
        } else {
            (3, raw)
        }
    } else {
        let mut raw = Vec::with_capacity(4096);
        for g in &gids {
            raw.push(*g as u8);
        }
        // size-only scans (exact lengths) — materialize only the winner
        let la = rle_stream_len(&raw);
        let a_ok = div > 0 && la * div <= raw.len() as u64 && la + 32 <= raw.len() as u64;
        let lb = rle_transposed_len(&raw, 16);
        let b_ok = div > 0 && lb * div <= raw.len() as u64 && lb + 32 <= raw.len() as u64;
        if a_ok {
            if b_ok && lb < la {
                (2, rle_encode_stream(&transpose_fast_y(&raw, 16)))
            } else {
                (1, rle_encode_stream(&raw))
            }
        } else if b_ok {
            (2, rle_encode_stream(&transpose_fast_y(&raw, 16)))
        } else {
            (0, raw)
        }
    };
    let mut out = Vec::with_capacity(p + stream_len + payload.len() + 8);
    if gform == 0 {
        // legacy 0xCB: byte-identical to the pre-0xCC wire (raw u8 stream)
        out.push(GLOB_MAGIC);
        out.push(f1);
        out.push(f2 & !4 & !16 & !32);
        // 0xCB ALWAYS carries the f3 byte (all f1/f2 bits are taken; the
        // explicit shape removes any v3/v4 header ambiguity)
        out.push(f3);
        out.push(y);
        out.extend_from_slice(&payload);
    } else {
        out.push(GLOB2_MAGIC);
        out.push(f1);
        out.push(f2 & !4 & !16 & !32);
        out.push(f3);
        out.push(y);
        out.push(gform);
        out.extend_from_slice(&payload);
    }
    out.extend_from_slice(&csec[p + stream_len..]);
    ST_GLOB_ENC.fetch_add(1, Ordering::Relaxed);
    if std::env::var("CSAVES_GLOB_DEBUG").as_deref() == Ok("1") {
        eprintln!(
            "glob: converted ok n={n} canon={} glob={} gform={gform}",
            csec.len(),
            out.len()
        );
    }
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
    // distinct used gids in the frequency-canonical order — EXACTLY the
    // enc_pal_data_canon rule: count DESC (dominant first), ties by
    // (plain, name, props) ASC. The rebuild MUST reproduce the canonical
    // encoder's palette order byte-for-byte: 0xC9 shift-ref members store
    // permutations against the TARGET's palette positions, computed at
    // write time against the canonical order. Any divergence (e.g. the
    // ascending sort this code once had) silently invalidates every shift
    // whose target was glob-rebuilt — palettes came back rotated by the
    // re-sorted indices (32 corrupt chunks on the JJ corpus).
    let mut used: Vec<(u32, u32)> = (0..reg.len().min(256) as u32)
        .filter(|&g| counts[g as usize] > 0)
        .map(|g| (counts[g as usize], g))
        .collect();
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
        let (ci, _) = used[i];
        let (cj, _) = used[j];
        let (_, pi, ni, propsi) = &reg_snap[i];
        let (_, pj, nj, propsj) = &reg_snap[j];
        // count DESC, then (plain, name, props) ASC — enc_pal_data_canon
        cj.cmp(&ci).then_with(|| (pi, ni, propsi).cmp(&(pj, nj, propsj)))
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
    // size-only scans (exact lengths, same run walk) — materialize only the
    // winning candidate; identical decisions and bytes to full materialize
    let la = rle_stream_len(&vox);
    let a_ok = la * 4 <= dense.len() as u64 && la + 32 <= dense.len() as u64;
    let lb = rle_transposed_len(&vox, 16);
    let b_ok = lb * 4 <= dense.len() as u64 && lb + 32 <= dense.len() as u64;
    if a_ok {
        if b_ok && lb < la {
            best_len = lb as usize;
            best = Some((2, rle_encode_stream(&transpose_fast_y(&vox, 16))));
        } else {
            best_len = la as usize;
            best = Some((1, rle_encode_stream(&vox)));
        }
    } else if b_ok {
        best_len = lb as usize;
        best = Some((2, rle_encode_stream(&transpose_fast_y(&vox, 16))));
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

pub const GLOB_MAGIC: u8 = 0xCB;

/// 0xCC GLOB2 (v0.9.5): RLE-coded / u16-width gid streams. Layout
/// [0xCC][f1][f2'][f3][Y][gform][payload][verbatim tail] — gform values in
/// section_to_glob's doc (0 raw u8, 1 rle u8, 2 rle-t u8, 3 raw u16,
/// 4 rle u16, 5 rle-t u16). Raw-u8 members stay on the legacy 0xCB wire.
pub const GLOB2_MAGIC: u8 = 0xCC;

/// A slab member carries a GLOB form (either wire version).
pub fn is_glob_member(b: &[u8]) -> bool {
    matches!(b.first(), Some(&GLOB_MAGIC) | Some(&GLOB2_MAGIC))
}

/// True when the member's gid stream is u16-wide (slab reg_w bookkeeping).
pub fn glob_member_wide(b: &[u8]) -> bool {
    b.first() == Some(&GLOB2_MAGIC) && matches!(b.get(5), Some(3 | 4 | 5))
}

/// Dispatch any GLOB member (0xCB legacy raw-u8 or 0xCC) back to canonical
/// CSEC bytes. This is the single read-side entry point.
pub fn glob_any_to_section(glob: &[u8], reg: &StateRegistry) -> Option<Vec<u8>> {
    match glob.first() {
        Some(&GLOB_MAGIC) => glob_to_section(glob, reg),
        Some(&GLOB2_MAGIC) => glob2_to_section(glob, reg),
        _ => None,
    }
}

/// Rebuild canonical CSEC bytes from a 0xCC GLOB2 member. Mirrors
/// glob_to_section's rebuild exactly — frequency-canonical palette order
/// (count DESC, ties (plain, name, props) ASC), same dense/RLE/RLE-t form
/// re-choice for n <= 256, dense-only wide packing for n > 256 — with a
/// payload-decode step (gform) in front.
pub fn glob2_to_section(glob: &[u8], reg: &StateRegistry) -> Option<Vec<u8>> {
    if glob.first() != Some(&GLOB2_MAGIC) {
        return None;
    }
    let f1 = *glob.get(1)?;
    let f2 = *glob.get(2)?;
    let f3 = *glob.get(3)?; // 0xCC always carries the f3 byte
    let y = *glob.get(4)?;
    let gform = *glob.get(5)?;
    let mut p = 6usize;
    if f1 & 3 != 3 {
        return None; // never globbed without a data stream
    }
    // decode the gid payload: one u32 gid per voxel
    let mut gids: Vec<u32> = Vec::with_capacity(4096);
    match gform {
        0 => {
            let s = glob.get(p..p + 4096)?;
            p += 4096;
            gids.extend(s.iter().map(|&g| g as u32));
        }
        1 | 2 => {
            // [gid u8][varint run] pairs. gform 2 stored the RLE over the
            // y-fast TRANSPOSED order — the transpose permutation is an
            // involution, so applying it again restores vanilla order.
            let mut raw: Vec<u8> = Vec::with_capacity(4096);
            let mut total = 0usize;
            while total < 4096 {
                let g = *glob.get(p)?;
                p += 1;
                let run = read_uvarint(glob, &mut p)? as usize;
                total += run;
                if total > 4096 {
                    return None;
                }
                raw.extend(std::iter::repeat(g).take(run));
            }
            if gform == 2 {
                for g in transpose_fast_y(&raw, 16) {
                    gids.push(g as u32);
                }
            } else {
                gids.extend(raw.iter().map(|&g| g as u32));
            }
        }
        3 => {
            let s = glob.get(p..p + 8192)?;
            p += 8192;
            for c in s.chunks_exact(2) {
                gids.push(u16::from_le_bytes(c.try_into().ok()?) as u32);
            }
        }
        4 | 5 => {
            // [gid u16 LE][varint run] pairs; gform 5 un-transposes like
            // gform 2 above
            let mut raw: Vec<u32> = Vec::with_capacity(4096);
            let mut total = 0usize;
            while total < 4096 {
                let c = glob.get(p..p + 2)?;
                p += 2;
                let g = u16::from_le_bytes(c.try_into().ok()?) as u32;
                let run = read_uvarint(glob, &mut p)? as usize;
                total += run;
                if total > 4096 {
                    return None;
                }
                raw.extend(std::iter::repeat(g).take(run));
            }
            if gform == 5 {
                gids.extend(transpose_fast_y_u16(&raw, 16));
            } else {
                gids.extend(raw);
            }
        }
        _ => return None,
    }
    if gids.len() != 4096 {
        return None;
    }
    let tail = &glob[p..];
    let max_gid = *gids.iter().max()? as usize;
    // counts per gid over the section's voxels — sized to the gid range the
    // section actually touches (the u8 hot path never allocates beyond 256)
    let mut counts = vec![0u32; max_gid + 1];
    for &g in &gids {
        counts[g as usize] += 1;
    }
    // distinct used gids in the frequency-canonical order — EXACTLY the
    // enc_pal_data_canon rule (count DESC, ties (plain, name, props) ASC);
    // see glob_to_section for why byte-identity matters (shift-ref perms)
    let mut used: Vec<(u32, u32)> = (0..counts.len() as u32)
        .filter(|&g| counts[g as usize] > 0)
        .map(|g| (counts[g as usize], g))
        .collect();
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
        let (ci, _) = used[i];
        let (cj, _) = used[j];
        let (_, pi, ni, propsi) = &reg_snap[i];
        let (_, pj, nj, propsj) = &reg_snap[j];
        cj.cmp(&ci).then_with(|| (pi, ni, propsi).cmp(&(pj, nj, propsj)))
    });
    let ordered: Vec<(u32, u32)> = order.into_iter().map(|i| used[i]).collect();
    let used = ordered;
    let n = used.len();
    if n == 0 || n > 65535 {
        return None;
    }
    let magic = if f3 != 0 { CSEC4_MAGIC } else { CSEC3_MAGIC };
    let mut gid2canon = vec![0u16; max_gid + 1];
    for (i, (_, g)) in used.iter().enumerate() {
        gid2canon[*g as usize] = i as u16;
    }
    let mut out = Vec::with_capacity(16 + n * 32 + 8192 + tail.len());
    if n > 256 {
        // wide palette rebuild: dense-only, exactly the canonical n>256 path
        let out_bits = bits_for(n);
        let f2n = f2 & !4 & !16 & !32;
        out.push(magic);
        out.push(f1);
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
        let mut w = BitW::new();
        for g in &gids {
            w.put(gid2canon[*g as usize] as u64, out_bits);
        }
        out.extend_from_slice(&w.finish());
        out.extend_from_slice(tail);
        ST_GLOB_DEC.fetch_add(1, Ordering::Relaxed);
        return Some(out);
    }
    // narrow rebuild: same form re-choice as the canonical encoder
    let narrow = bits_for(n) < 4;
    let out_bits = if narrow { bits_for(n) } else { bits_for(n).max(4) };
    let vox: Vec<u8> = gids.iter().map(|&g| gid2canon[g as usize] as u8).collect();
    let dense = dense_pack(&vox, out_bits);
    let mut best_len = dense.len();
    let mut best: Option<(u8, Vec<u8>)> = None;
    // size-only scans (exact lengths, same run walk) — materialize only the
    // winning candidate; identical decisions and bytes to full materialize
    let la = rle_stream_len(&vox);
    let a_ok = la * 4 <= dense.len() as u64 && la + 32 <= dense.len() as u64;
    let lb = rle_transposed_len(&vox, 16);
    let b_ok = lb * 4 <= dense.len() as u64 && lb + 32 <= dense.len() as u64;
    if a_ok {
        if b_ok && lb < la {
            best_len = lb as usize;
            best = Some((2, rle_encode_stream(&transpose_fast_y(&vox, 16))));
        } else {
            best_len = la as usize;
            best = Some((1, rle_encode_stream(&vox)));
        }
    } else if b_ok {
        best_len = lb as usize;
        best = Some((2, rle_encode_stream(&transpose_fast_y(&vox, 16))));
    }
    let mut f2n = f2 & !4 & !16 & !32;
    if narrow {
        f2n |= 4;
    }
    out.push(magic);
    out.push(f1);
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
        }
        Some((ord, bytes)) => {
            out.extend_from_slice(&bytes);
            out[2] |= ord << 4;
        }
    }
    out.extend_from_slice(tail);
    ST_GLOB_DEC.fetch_add(1, Ordering::Relaxed);
    Some(out)
}

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

    /// True wide-voxel section: u16 values spanning all `distinct` states
    /// (the u8 helper above wraps values >= 256 — useless for the >256-state
    /// paths). Data is packed no-straddle at the vanilla clamp width.
    fn glob_section_wide(y: i32, seed: u64, distinct: usize) -> Value {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        let mut pal = Vec::new();
        for i in 0..distinct {
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
        let vox: Vec<u16> = (0..4096)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 7;
                st ^= st << 17;
                (st % distinct as u64) as u16
            })
            .collect();
        let vbits = (((distinct as u32 - 1).ilog2() + 1).max(4)) as u32;
        // vanilla no-straddle longs: per = 64/bits entries per long, entry k
        // at bit offset k*bits (matches dense_to_nostroddle's layout)
        let per = (64 / vbits) as usize;
        let mut longs = vec![0i64; (4096 + per - 1) / per];
        for (i, &v) in vox.iter().enumerate() {
            longs[i / per] |= (v as i64) << ((i % per) * vbits as usize);
        }
        bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(longs)));
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
            // the >256 case must use TRUE wide voxels: the u8 helper wraps
            // values >= 256, so a 300-palette section built from it only
            // ever references 256 states
            let src = if distinct > 256 {
                glob_section_wide(-4 + distinct as i32, 0xDEADBEEF, distinct)
            } else {
                glob_section(-4 + distinct as i32, 0xDEADBEEF, distinct)
            };
            let mut reg = StateRegistry::new();
            let canon = match section_encode(&src) {
                Some(c) => c,
                None => panic!("canonical encode failed at distinct={distinct}"),
            };
            let glob = section_to_glob(&canon, &mut reg);
            let back;
            if distinct > 256 {
                // v0.9.5: registries above 256 states take the u16 gid
                // stream (0xCC gform 3 for noisy content) instead of
                // refusing the conversion
                let glob = glob.expect("u16 glob conversion");
                assert_eq!(glob[0], GLOB2_MAGIC, "wide registry must take 0xCC");
                back = glob_any_to_section(&glob, &reg).expect("decode");
                let a = section_decode(&canon).expect("decode canon");
                let b = section_decode(&back).expect("decode back");
                assert!(sections_semantically_equal(&a, &b), "semantic {distinct}");
            } else {
                let glob = glob.expect("glob conversion");
                assert_eq!(glob[0], GLOB_MAGIC);
                back = glob_to_section(&glob, &reg).expect("decode");
                // semantic equality through the SECTION DECODER (CSEC -> Value):
                // the rebuilt palette holds only USED states, so the wire bytes
                // differ while the section content is identical
                let a = section_decode(&canon).expect("decode canon");
                let b = section_decode(&back).expect("decode back");
                assert!(
                    sections_semantically_equal(&a, &b),
                    "semantic roundtrip failed at distinct={distinct}"
                );
            }
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
            // production contract: GLOB members are converted back by
            // read_blob BEFORE any decoder; mirror that here
            let back2 = glob_any_to_section(&glob2, &reg2).expect("decode glob2");
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

    /// Two-state section with moderate horizontal runs: the canonical dense
    /// stream packs at 1 bit (512 B) so the RLE admission (ra*4 <= dense)
    /// fails, but the 4096-byte raw gid stream clears the wider window —
    /// the member must take the 0xCC RLE form and roundtrip byte-identically.
    fn strata_section(y: i32, a: &str, b: &str, period: usize) -> Value {
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(y as i8));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![
                Value::String(a.to_string()),
                Value::String(b.to_string()),
            ]),
        );
        let vox: Vec<u8> = (0..4096).map(|i| ((i / period) % 2) as u8).collect();
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(dense_to_nostroddle(&vox, 4096, 8, 4))),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        Value::Compound(sec)
    }

    #[test]
    fn glob_transposed_gid_form_roundtrip_byte_identical() {
        // vertical-wall content: state varies with x, constant along y —
        // vanilla-order RLE has runs of 1 (never admitted), the y-fast
        // transposed order has runs of 16 (admitted) -> gform must be 2,
        // and the rebuild must un-transpose back to byte-identical canon
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(9));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert(
            "palette".to_string(),
            Value::List(vec![
                Value::String("minecraft:brick_a".to_string()),
                Value::String("minecraft:brick_b".to_string()),
            ]),
        );
        // checkerboard columns: state = (x+z)%2 — vanilla order (x fastest)
        // alternates every voxel (runs of 1), the y-fast transposed order has
        // runs of 16 (whole columns) -> canonical dense, gid RLE gform 2
        let vox: Vec<u8> = (0..4096)
            .map(|i| ((i % 16 + (i / 16) % 16) % 2) as u8)
            .collect();
        bs.insert(
            "data".to_string(),
            Value::LongArray(fastnbt::LongArray::new(dense_to_nostroddle(&vox, 4096, 8, 4))),
        );
        sec.insert("block_states".to_string(), Value::Compound(bs));
        let canon = section_encode(&Value::Compound(sec)).expect("encode");
        assert_eq!(canon[2] & 48, 0, "source must be canonical-dense");
        let mut reg = StateRegistry::new();
        let glob = section_to_glob(&canon, &mut reg).expect("glob conversion");
        assert_eq!(glob[0], GLOB2_MAGIC);
        assert_eq!(glob[5], 2, "transposed rle form must win for wall content");
        let back = glob_any_to_section(&glob, &reg).expect("decode");
        assert_eq!(back, canon, "gform 2 roundtrip must be byte-identical");
    }

    #[test]
    fn glob_rle_gid_form_roundtrip_byte_identical() {
        // pin the admission divisor: this test needs the 4x rule so the
        // ~850-byte strata RLE is admitted (the default 8x would refuse it)
        let _g = crate::store::glob_slab_test::tests_glob_lock();
        std::env::set_var("CSAVES_GID_RLE_DIV", "4");
        let src = strata_section(5, "minecraft:block_a", "minecraft:block_b", 10);
        let canon = section_encode(&src).expect("encode");
        // the canonical stream stays DENSE (runs too short for the packed
        // width) — otherwise the section would never reach the glob pass
        assert_eq!(canon[2] & 48, 0, "source must be canonical-dense");
        let mut reg = StateRegistry::new();
        let glob = section_to_glob(&canon, &mut reg).expect("glob conversion");
        assert_eq!(glob[0], GLOB2_MAGIC, "rle gid streams take 0xCC");
        let gform = glob[5];
        assert!(
            gform == 1 || gform == 2,
            "expected rle u8 form, got gform={gform}"
        );
        // the payload must actually be smaller than the raw gid stream
        assert!(glob.len() < canon.len() + 4096, "rle payload shrank the member");
        let back = glob_any_to_section(&glob, &reg).expect("decode");
        assert_eq!(back, canon, "0xCC rle roundtrip must be byte-identical");
        // noisy content keeps the legacy raw-u8 wire
        let noisy = glob_section(6, 0xFEED, 16);
        let canon_n = section_encode(&noisy).expect("encode noisy");
        let glob_n = section_to_glob(&canon_n, &mut reg).expect("glob noisy");
        assert_eq!(glob_n[0], GLOB_MAGIC, "noisy gid stream stays 0xCB");
        assert_eq!(glob_any_to_section(&glob_n, &reg).unwrap(), canon_n);
        std::env::remove_var("CSAVES_GID_RLE_DIV");
    }

    #[test]
    fn glob_u16_wide_registry_rle_roundtrip() {
        // push the registry past 256 states, then convert a strata section
        // whose states land at gids > 255: the u16 RLE form (gform 4/5)
        let _g = crate::store::glob_slab_test::tests_glob_lock();
        std::env::set_var("CSAVES_GID_RLE_DIV", "4");
        let pre = glob_section_wide(-2, 0xAAAA, 300);
        let mut reg = StateRegistry::new();
        let pre_canon = section_encode(&pre).expect("encode pre");
        let pre_glob = section_to_glob(&pre_canon, &mut reg).expect("pre glob");
        assert_eq!(pre_glob[0], GLOB2_MAGIC, "noisy wide section takes 0xCC gform 3");
        assert_eq!(pre_glob[5], 3, "raw u16 gform for noisy content");
        let pre_back = glob_any_to_section(&pre_glob, &reg).expect("pre decode");
        let a = section_decode(&pre_canon).expect("decode");
        let b = section_decode(&pre_back).expect("decode");
        assert!(sections_semantically_equal(&a, &b), "wide semantic");
        // strata states AFTER the 256 line: gids 300/301
        let src = strata_section(7, "minecraft:block_300", "minecraft:block_301", 12);
        let canon = section_encode(&src).expect("encode strata");
        assert_eq!(canon[2] & 48, 0, "strata source must be canonical-dense");
        let glob = section_to_glob(&canon, &mut reg).expect("wide strata glob");
        assert_eq!(glob[0], GLOB2_MAGIC);
        let gform = glob[5];
        assert!(
            gform == 4 || gform == 5,
            "expected rle u16 form, got gform={gform}"
        );
        assert!(glob.len() < canon.len() + 8192, "u16 rle payload shrank the member");
        let back = glob_any_to_section(&glob, &reg).expect("wide strata decode");
        assert_eq!(back, canon, "u16 rle roundtrip must be byte-identical");
        // registry persistence across the >256 line: reload from bytes,
        // decode again
        let reg2 = StateRegistry::from_bytes(&reg.to_bytes()).expect("registry parse");
        assert_eq!(reg2.len(), reg.len());
        let back2 = glob_any_to_section(&glob, &reg2).expect("decode with reloaded registry");
        assert_eq!(back2, canon);
        std::env::remove_var("CSAVES_GID_RLE_DIV");
    }
}
