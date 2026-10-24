//! CRS2 container: append-only segments, content-addressed blobs, chunk
//! recipes, tombstones, background compaction, versioned shared zstd
//! dictionaries, two-tier compression.
//!
//! Layout `<world>/c-saves/<kind>/`:
//!   * `seg-<id>.log`     — append-only segment files (64MB roll)
//!   * `dicts/dict-N.bin` — shared zstd dictionaries (versioned)
//!
//! Record framing (little-endian): `[u32 body_len][u32 crc32(body)][body]`
//! Body kinds:
//!   * 1 BLOB-legacy `[u8 dict][u64 hi][u64 lo][u32 raw][u32 cl][comp]`
//!   * 2 CHUNK       `[u64 key][u32 rec_len][recipe..]`
//!   * 3 DEL         `[u64 key]`
//!   * 4 BLOB2       `[u8 dict][u32 id][u64 hi][u64 lo][u32 raw][u32 cl][comp]`
//!
//! Recipe modes:
//!   * 0 whole-by-hash `[0][hi][lo]`                                  (legacy)
//!   * 1 split-by-hash + inline tail `[1][n][n x hi,lo][dict][cl][raw][comp]` (legacy)
//!   * 2 split-by-id    `[2][n][n x zigzag-varint id-delta][tail]`
//!   * 3 whole-by-id    `[3][varint id]`
//!   * 4 split-by-hash  `[4][n][n x hi,lo][tail]`
//! tail kinds (modes 2/4): 0 none | 1 inline `[dict][cl][raw][comp]`
//!                         | 2 blob-by-varint-id | 3 blob-by-hash
//!
//! Size levers: section-level content dedup, tail dedup, id/varint recipes,
//! 112KB retrained shared dictionaries, hot tier (level 12) on the write
//! path, cold tier (level 19) recompression during background compaction.

use crate::chunknbt::{self, Hash, Split};
use crc32fast::Hasher as Crc;
use memmap2::Mmap;
use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

const SEG_ROLL: u64 = 64 * 1024 * 1024;
const BUF_CAP: usize = 1 << 20;
const MAX_SECTIONS: usize = 255;
const NO_ID: u32 = u32::MAX;

// ------------------------------------------------------------- config -----

#[derive(Clone, Debug)]
pub struct Cfg {
    /// zstd level on the write path (hot tier).
    pub hot_level: i32,
    /// zstd level for background recompression (cold tier).
    pub cold_level: i32,
    /// Min segment size before compaction considers it.
    pub compact_min: u64,
    /// Dead-byte ratio that triggers compaction.
    pub compact_ratio: f64,
    /// Shared dictionary size cap.
    pub dict_max: usize,
    /// Samples collected before (re)training the dictionary.
    pub dict_train: usize,
    /// Tails with compressed size >= this become dedup blobs.
    pub tail_blob_min: usize,
    /// Blobs with raw size <= this become slab members at compaction.
    pub slab_max_member: u32,
    /// Target decompressed size of one slab record.
    pub slab_max_raw: usize,
    /// Decompressed-slab read cache budget (bytes).
    pub slab_cache: u64,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            hot_level: 12,
            cold_level: 19,
            compact_min: 4 * 1024 * 1024,
            compact_ratio: 0.25,
            dict_max: 112 * 1024,
            dict_train: 600,
            tail_blob_min: 512,
            slab_max_member: 4096,
            slab_max_raw: 192 * 1024,
            slab_cache: 32 * 1024 * 1024,
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, dflt: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

impl Cfg {
    fn from_env() -> Cfg {
        Cfg {
            hot_level: env_parse("CSAVES_HOT_LEVEL", 12),
            cold_level: env_parse("CSAVES_COLD_LEVEL", 19),
            compact_min: env_parse("CSAVES_COMPACT_MIN_BYTES", 4 * 1024 * 1024),
            compact_ratio: env_parse("CSAVES_COMPACT_DEAD_RATIO", 0.25),
            dict_max: env_parse("CSAVES_DICT_MAX_BYTES", 112 * 1024),
            dict_train: env_parse("CSAVES_DICT_TRAIN_BLOBS", 600),
            tail_blob_min: env_parse("CSAVES_TAIL_BLOB_MIN", 512),
            slab_max_member: env_parse("CSAVES_SLAB_MAX_MEMBER", 4096),
            slab_max_raw: env_parse("CSAVES_SLAB_MAX_RAW", 192 * 1024),
            slab_cache: env_parse("CSAVES_SLAB_CACHE", 32 * 1024 * 1024),
        }
    }
}

// ------------------------------------------------------------- varint -----

fn put_u64(v: &mut Vec<u8>, mut x: u64) {
    loop {
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 {
            v.push(b);
            break;
        }
        v.push(b | 0x80);
    }
}

fn get_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut x = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        x |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(x);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn zigzag(x: i64) -> u64 {
    ((x << 1) ^ (x >> 63)) as u64
}

fn unzigzag(x: u64) -> i64 {
    ((x >> 1) as i64) ^ -((x & 1) as i64)
}

// -------------------------------------------------------------- blobs -----

#[derive(Clone, Copy, Debug)]
struct BlobLoc {
    seg: u32,
    off: u64,
    comp_len: u32,
    raw_len: u32,
    dict_id: u8,
    refs: u32,
    id: u32, // NO_ID for legacy kind-1 records
    // slab membership (kind-5 records): the blob lives inside a packed slab
    slab: bool,
    share: u32,        // exact on-disk byte share incl. framing
    slab_off: u32,     // own offset within the decompressed slab
    slab_raw: u32,     // total decompressed slab size
    slab_comp: u32,    // compressed slab size
}

impl BlobLoc {
    fn header_len(&self) -> u64 {
        if self.id == NO_ID { 26 } else { 30 }
    }
    /// Exact on-disk bytes attributable to this blob, framing included.
    fn body_len(&self) -> u64 {
        if self.slab {
            self.share as u64
        } else {
            8 + self.header_len() + self.comp_len as u64
        }
    }
}

struct Seg {
    file: File,
    mmap: Mutex<Option<Arc<Mmap>>>,
    total: AtomicU64,
    live: AtomicU64,
}

impl Seg {
    fn read_at(&self, off: u64, len: usize, sealed: bool) -> Option<Vec<u8>> {
        if sealed {
            let mut m = self.mmap.lock();
            if m.is_none() {
                *m = unsafe { Mmap::map(&self.file) }.ok().map(Arc::new);
            }
            if let Some(mm) = m.as_ref() {
                let s = mm.get(off as usize..off as usize + len)?;
                return Some(s.to_vec());
            }
        }
        // pread: positional read on a shared fd -- seek()+read() here would
        // race with concurrent IO threads moving the shared file offset and
        // return garbage (observed as infinite spawn-load retries).
        let mut buf = vec![0u8; len];
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(&mut buf, off).ok()?;
        Some(buf)
    }
}

struct Writer {
    file: File,
    seg_id: u32,
    written: u64,
    buf: Vec<u8>,
}

// ------------------------------------------------------------- recipe -----

#[derive(Clone, Debug)]
enum Tail {
    None,
    Inline { dict: u8, comp: Vec<u8>, raw: u32 },
    Blob(Hash),
}

#[derive(Clone, Debug)]
enum FieldVal {
    /// Unnamed-value bytes inlined in the recipe (≤255B).
    Inline { raw: Vec<u8> },
    /// Blob reference (always a BLOB2 with an id).
    Hash(Hash),
}

#[derive(Clone, Debug)]
struct Recipe {
    whole: Option<Hash>,
    sections: Vec<Hash>,
    tail: Tail,
    /// Mode-5 field split (whole-mode payloads): (key id, value).
    fields: Vec<(u16, FieldVal)>,
    /// Mode-5: the Position marker was key-redundant and stripped.
    pos_from_key: bool,
}

fn crc_of(b: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(b);
    c.finalize()
}

impl Recipe {
    /// Parse any recipe mode. `id2h` resolves BLOB2 ids (pass-1 complete);
    /// `keytab` resolves mode-5 interned key ids.
    fn parse(b: &[u8], id2h: &HashMap<u32, Hash>, keytab: &[String]) -> Option<Recipe> {
        const EMPTY: Recipe = Recipe {
            whole: None,
            sections: Vec::new(),
            tail: Tail::None,
            fields: Vec::new(),
            pos_from_key: false,
        };
        let mut p = 0usize;
        let mode = *b.first()?;
        p += 1;
        let rd4 = |p: &mut usize| -> u32 {
            let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let rd8 = |p: &mut usize| -> u64 {
            let v = u64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
            *p += 8;
            v
        };
        let rd_hash = |p: &mut usize| -> Hash { ((rd8(p) as u128) << 64) | rd8(p) as u128 };
        let parse_tail = |p: &mut usize| -> Option<Tail> {
            let kind = *b.get(*p)?;
            *p += 1;
            match kind {
                0 => Some(Tail::None),
                1 => {
                    if *p + 9 > b.len() {
                        return None;
                    }
                    let dict = b[*p];
                    *p += 1;
                    let cl = rd4(p) as usize;
                    let raw = rd4(p);
                    if *p + cl > b.len() {
                        return None;
                    }
                    let comp = b[*p..*p + cl].to_vec();
                    *p += cl;
                    Some(Tail::Inline { dict, comp, raw })
                }
                2 => {
                    let id = get_u64(b, p)? as u32;
                    Some(Tail::Blob(*id2h.get(&id)?))
                }
                3 => Some(Tail::Blob(rd_hash(p))),
                _ => None,
            }
        };
        match mode {
            0 => Some(Recipe { whole: Some(rd_hash(&mut p)), ..EMPTY }),
            1 => {
                let n = *b.get(p)? as usize;
                p += 1;
                let mut sections = Vec::with_capacity(n);
                for _ in 0..n {
                    sections.push(rd_hash(&mut p));
                }
                if p + 9 > b.len() {
                    return None;
                }
                let dict = b[p];
                p += 1;
                let cl = rd4(&mut p) as usize;
                let raw = rd4(&mut p);
                if p + cl > b.len() {
                    return None;
                }
                let comp = b[p..p + cl].to_vec();
                p += cl;
                Some(Recipe {
                    whole: None,
                    sections,
                    tail: Tail::Inline { dict, comp, raw },
                    ..EMPTY
                })
            }
            2 => {
                let n = *b.get(p)? as usize;
                p += 1;
                let mut sections = Vec::with_capacity(n);
                let mut prev: i64 = 0;
                for _ in 0..n {
                    let d = unzigzag(get_u64(b, &mut p)?);
                    prev += d;
                    sections.push(*id2h.get(&(prev as u32))?);
                }
                let tail = parse_tail(&mut p)?;
                Some(Recipe { whole: None, sections, tail, ..EMPTY })
            }
            3 => {
                let id = get_u64(b, &mut p)? as u32;
                Some(Recipe {
                    whole: Some(*id2h.get(&id)?),
                    ..EMPTY
                })
            }
            5 => {
                let flags = *b.get(p)?;
                p += 1;
                let n = *b.get(p)? as usize;
                p += 1;
                let mut fields = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    let kid = get_u64(b, &mut p)? as u16;
                    let kind = *b.get(p)?;
                    p += 1;
                    let val = match kind {
                        0 => {
                            let l = *b.get(p)? as usize;
                            p += 1;
                            let raw = b.get(p..p + l)?.to_vec();
                            p += l;
                            FieldVal::Inline { raw }
                        }
                        1 => {
                            let id = get_u64(b, &mut p)? as u32;
                            FieldVal::Hash(*id2h.get(&id)?)
                        }
                        2 => FieldVal::Hash(rd_hash(&mut p)),
                        _ => return None,
                    };
                    fields.push((kid, val));
                }
                if fields.iter().any(|(k, _)| *k as usize >= keytab.len()) {
                    return None;
                }
                Some(Recipe { fields, pos_from_key: flags & 1 != 0, ..EMPTY })
            }
            4 => {
                let n = *b.get(p)? as usize;
                p += 1;
                let mut sections = Vec::with_capacity(n);
                for _ in 0..n {
                    sections.push(rd_hash(&mut p));
                }
                let tail = parse_tail(&mut p)?;
                Some(Recipe { whole: None, sections, tail, ..EMPTY })
            }
            _ => None,
        }
    }

    fn blob_hashes(&self) -> Vec<Hash> {
        let mut v = Vec::with_capacity(self.sections.len() + self.fields.len() + 1);
        if let Some(h) = self.whole {
            v.push(h);
        }
        v.extend_from_slice(&self.sections);
        for (_, f) in &self.fields {
            if let FieldVal::Hash(h) = f {
                v.push(*h);
            }
        }
        if let Tail::Blob(h) = &self.tail {
            v.push(*h);
        }
        v
    }

    /// Encode choosing the smallest compatible mode.
    /// `id_of` returns the BLOB2 id of a live blob hash (None for legacy).
    fn encode(&self, id_of: impl Fn(Hash) -> Option<u32>) -> Vec<u8> {
        let mut v = Vec::with_capacity(64);
        if let Some(h) = self.whole {
            match id_of(h) {
                Some(id) => {
                    v.push(3u8);
                    put_u64(&mut v, id as u64);
                }
                None => {
                    v.push(0u8);
                    v.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                    v.extend_from_slice(&(h as u64).to_le_bytes());
                }
            }
            return v;
        }
        let ids: Option<Vec<u32>> =
            self.sections.iter().map(|h| id_of(*h)).collect();
        let tail_kind = match &self.tail {
            Tail::None => 0u8,
            Tail::Inline { .. } => 1u8,
            Tail::Blob(h) => match id_of(*h) {
                Some(_) => 2u8,
                None => 3u8,
            },
        };
        let can_id = ids.is_some();
        match (can_id, ids, &self.tail) {
            (true, Some(ids), tail) => {
                v.push(2u8);
                v.push(self.sections.len() as u8);
                let mut prev: i64 = 0;
                for id in &ids {
                    put_u64(&mut v, zigzag(*id as i64 - prev));
                    prev = *id as i64;
                }
                push_tail(&mut v, tail, id_of);
            }
            _ => {
                v.push(4u8);
                v.push(self.sections.len() as u8);
                for h in &self.sections {
                    v.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                    v.extend_from_slice(&((*h as u64).to_le_bytes()));
                }
                push_tail(&mut v, &self.tail, id_of);
            }
        }
        v
    }

    /// Encode mode 5 (field split).
    fn encode_fields(&self, id_of: impl Fn(Hash) -> Option<u32> + Copy) -> Option<Vec<u8>> {
        if self.fields.is_empty() || self.fields.len() > 255 {
            return None;
        }
        let mut v = Vec::with_capacity(48);
        v.push(5u8);
        v.push(if self.pos_from_key { 1u8 } else { 0u8 });
        v.push(self.fields.len() as u8);
        for (kid, val) in &self.fields {
            put_u64(&mut v, *kid as u64);
            match val {
                FieldVal::Inline { raw } => {
                    if raw.len() > 255 {
                        return None;
                    }
                    v.push(0u8);
                    v.push(raw.len() as u8);
                    v.extend_from_slice(raw);
                }
                FieldVal::Hash(h) => match id_of(*h) {
                    Some(id) => {
                        v.push(1u8);
                        put_u64(&mut v, id as u64);
                    }
                    None => {
                        v.push(2u8);
                        v.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                        v.extend_from_slice(&((*h as u64).to_le_bytes()));
                    }
                },
            }
        }
        Some(v)
    }
}

fn push_tail(v: &mut Vec<u8>, tail: &Tail, id_of: impl Fn(Hash) -> Option<u32>) {
    match tail {
        Tail::None => v.push(0u8),
        Tail::Inline { dict, comp, raw } => {
            v.push(1u8);
            v.push(*dict);
            v.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            v.extend_from_slice(&raw.to_le_bytes());
            v.extend_from_slice(comp);
        }
        Tail::Blob(h) => match id_of(*h) {
            Some(id) => {
                v.push(2u8);
                put_u64(v, id as u64);
            }
            None => {
                v.push(3u8);
                v.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                v.extend_from_slice(&((*h as u64).to_le_bytes()));
            }
        },
    }
}

// ------------------------------------------------------- zstd contexts ----

struct CompCtx {
    // (store uid, dict id, level). Dict ids are PER-STORE, so the store uid
    // must be part of the key: multiple stores on one thread must never share
    // a codec loaded with another store's dictionary.
    key: (u64, u8, i32),
    comp: zstd::bulk::Compressor<'static>,
}

struct DecCtx {
    key: (u64, u8),
    dec: zstd::bulk::Decompressor<'static>,
}

thread_local! {
    static HOT_C: RefCell<Option<CompCtx>> = const { RefCell::new(None) };
    static COLD_C: RefCell<Option<CompCtx>> = const { RefCell::new(None) };
    static DEC_C: RefCell<Option<DecCtx>> = const { RefCell::new(None) };
}

fn build_comp(level: i32, dict: Option<&[u8]>) -> std::io::Result<zstd::bulk::Compressor<'static>> {
    match dict {
        Some(d) => {
            let mut c = zstd::bulk::Compressor::new(level)?;
            c.set_dictionary(level, d)?;
            Ok(c)
        }
        None => zstd::bulk::Compressor::new(level),
    }
}

fn build_dec(dict: Option<&[u8]>) -> std::io::Result<zstd::bulk::Decompressor<'static>> {
    let mut d = zstd::bulk::Decompressor::new()?;
    let _ = d.set_parameter(zstd_safe::DParameter::WindowLogMax(27));
    if let Some(dd) = dict {
        d.set_dictionary(dd)?;
    }
    Ok(d)
}

// ------------------------------------------------------------ slab cache --

/// Bounded FIFO cache of decompressed slab records (read path).
struct SlabCache {
    map: HashMap<(u32, u64), Arc<Vec<u8>>>,
    order: VecDeque<((u32, u64), u64)>, // key + approx size
    total: u64,
    cap: u64,
}

impl SlabCache {
    fn new(cap: u64) -> Self {
        SlabCache { map: HashMap::new(), order: VecDeque::new(), total: 0, cap }
    }
    fn get(&self, k: &(u32, u64)) -> Option<Arc<Vec<u8>>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: (u32, u64), v: Arc<Vec<u8>>) {
        let sz = v.len() as u64;
        if sz > self.cap {
            return;
        }
        if self.map.insert(k, v).is_none() {
            self.order.push_front((k, sz));
            self.total += sz;
            while self.total > self.cap {
                if let Some((ek, esz)) = self.order.pop_back() {
                    if self.map.remove(&ek).is_some() {
                        self.total -= esz;
                    }
                } else {
                    break;
                }
            }
        }
    }
}

// --------------------------------------------------------------- store ----

pub struct Store {
    uid: u64,
    dir: PathBuf,
    cfg: Cfg,
    segs: RwLock<HashMap<u32, Arc<Seg>>>,
    blobs: RwLock<HashMap<Hash, BlobLoc>>,
    ids: RwLock<HashMap<u32, Hash>>,
    chunks: RwLock<HashMap<u64, (u32, u64, u32)>>,
    /// Interned recipe key names (mode 5); persisted as kind-6 records.
    keys_tab: RwLock<Vec<String>>,
    /// On-disk kind-6 record count per key name (a key whose last record
    /// would die with a compacted segment is re-recorded first).
    key_refcnt: RwLock<HashMap<String, u32>>,
    writer: Mutex<Option<Writer>>,
    next_seg: AtomicU64,
    next_blob_id: AtomicU64,
    dicts: RwLock<HashMap<u8, Arc<Vec<u8>>>>,
    slab_cache: Mutex<SlabCache>,
    cur_dict: AtomicU8,
    next_dict_ver: AtomicU8,
    dict_samples: Mutex<Vec<Vec<u8>>>,
    dict_enabled: bool,

    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub dedup_hits: AtomicU64,
    pub disk_written: AtomicU64,
    pub logical_bytes: AtomicU64,
}

impl Store {
    pub fn open(dir: &Path, dict_enabled: bool) -> std::io::Result<Arc<Store>> {
        Self::open_cfg(dir, dict_enabled, Cfg::from_env())
    }

    pub fn open_cfg(dir: &Path, dict_enabled: bool, cfg: Cfg) -> std::io::Result<Arc<Store>> {
        fs::create_dir_all(dir)?;
        static NEXT_UID: AtomicU64 = AtomicU64::new(1);
        let st = Arc::new(Store {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            dir: dir.to_path_buf(),
            cfg,
            segs: RwLock::new(HashMap::new()),
            blobs: RwLock::new(HashMap::new()),
            ids: RwLock::new(HashMap::new()),
            chunks: RwLock::new(HashMap::new()),
            keys_tab: RwLock::new(Vec::new()),
            key_refcnt: RwLock::new(HashMap::new()),
            writer: Mutex::new(None),
            slab_cache: Mutex::new(SlabCache::new(32 * 1024 * 1024)),
            next_seg: AtomicU64::new(0),
            next_blob_id: AtomicU64::new(1),
            dicts: RwLock::new(HashMap::new()),
            cur_dict: AtomicU8::new(0),
            next_dict_ver: AtomicU8::new(1),
            dict_samples: Mutex::new(Vec::new()),
            dict_enabled,
            writes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
            disk_written: AtomicU64::new(0),
            logical_bytes: AtomicU64::new(0),
        });
        *st.slab_cache.lock() = SlabCache::new(st.cfg.slab_cache);
        st.load_dicts()?;
        st.scan()?;
        Ok(st)
    }

    // ---- dictionaries ----

    fn dict_dir(&self) -> PathBuf {
        self.dir.join("dicts")
    }

    fn load_dicts(&self) -> std::io::Result<()> {
        let ddir = self.dict_dir();
        fs::create_dir_all(&ddir)?;
        // migrate legacy dict.bin -> dicts/dict-1.bin
        let legacy = self.dir.join("dict.bin");
        if legacy.exists() && !ddir.join("dict-1.bin").exists() {
            let _ = fs::rename(&legacy, ddir.join("dict-1.bin"));
        }
        let mut vers = Vec::new();
        for e in fs::read_dir(&ddir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("dict-").and_then(|r| r.strip_suffix(".bin")) {
                if let Ok(v) = rest.parse::<u8>() {
                    vers.push(v);
                }
            }
        }
        vers.sort_unstable();
        let mut dicts = self.dicts.write();
        let mut maxv = 0u8;
        for v in vers {
            if let Ok(d) = fs::read(ddir.join(format!("dict-{v}.bin"))) {
                if d.len() >= 512 {
                    maxv = maxv.max(v);
                    dicts.insert(v, Arc::new(d));
                }
            }
        }
        if maxv > 0 {
            self.cur_dict.store(maxv, Ordering::Relaxed);
            self.next_dict_ver.store(maxv.saturating_add(1), Ordering::Relaxed);
        }
        Ok(())
    }

    fn current_dict(&self) -> Option<(u8, Arc<Vec<u8>>)> {
        let v = self.cur_dict.load(Ordering::Relaxed);
        if v == 0 {
            return None;
        }
        let d = self.dicts.read().get(&v)?.clone();
        Some((v, d))
    }

    /// Collect a raw payload as dictionary-training material.
    fn dict_sample(&self, raw: &[u8]) {
        if !self.dict_enabled {
            return;
        }
        let mut s = self.dict_samples.lock();
        if s.len() < self.cfg.dict_train.max(1) * 2 {
            s.push(raw[..raw.len().min(16 * 1024)].to_vec());
        }
    }

    /// Train a fresh shared dictionary from collected samples (if enough).
    /// Called from the maintenance thread (NOT the write path — training
    /// costs CPU). Returns true when a new dictionary version was installed.
    pub fn train_dict_if_needed(&self) -> bool {
        if !self.dict_enabled {
            return false;
        }
        if self.next_dict_ver.load(Ordering::Relaxed) == 255 {
            return false; // version space exhausted
        }
        let refs: Vec<Vec<u8>> = {
            let mut s = self.dict_samples.lock();
            if s.len() < self.cfg.dict_train {
                return false;
            }
            s.truncate(self.cfg.dict_train);
            std::mem::take(&mut *s)
        };
        let ref_slices: Vec<&[u8]> = refs.iter().map(|v| v.as_slice()).collect();
        let Ok(d) = zstd::dict::from_samples(&ref_slices, self.cfg.dict_max) else {
            return false;
        };
        if d.len() < 512 {
            return false;
        }
        let v = self.next_dict_ver.fetch_add(1, Ordering::Relaxed);
        let ddir = self.dict_dir();
        let _ = fs::create_dir_all(&ddir);
        if fs::write(ddir.join(format!("dict-{v}.bin")), &d).is_err() {
            self.next_dict_ver.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        self.dicts.write().insert(v, Arc::new(d));
        self.cur_dict.store(v, Ordering::Relaxed);
        eprintln!("[c-saves] trained shared dictionary v{v} ({} bytes)", {
            let d = self.dicts.read();
            d.get(&v).map(|x| x.len()).unwrap_or(0)
        });
        true
    }

    fn gc_dicts(&self) {
        // drop dictionary versions no blob references anymore
        let used: std::collections::HashSet<u8> =
            self.blobs.read().values().map(|l| l.dict_id).collect();
        let cur = self.cur_dict.load(Ordering::Relaxed);
        let mut dicts = self.dicts.write();
        let dead: Vec<u8> = dicts
            .keys()
            .copied()
            .filter(|v| *v != cur && !used.contains(v))
            .collect();
        for v in dead {
            dicts.remove(&v);
            let _ = fs::remove_file(self.dict_dir().join(format!("dict-{v}.bin")));
        }
    }

    // ---- segments ----

    fn seg_path(&self, id: u32) -> PathBuf {
        self.dir.join(format!("seg-{id:05}.log"))
    }

    fn new_seg(&self, id: u32) -> std::io::Result<File> {
        let path = self.seg_path(id);
        let wfile = OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        let rfile = OpenOptions::new().read(true).open(&path)?;
        self.segs.write().insert(
            id,
            Arc::new(Seg {
                file: rfile,
                mmap: Mutex::new(None),
                total: AtomicU64::new(0),
                live: AtomicU64::new(0),
            }),
        );
        Ok(wfile)
    }

    fn scan(&self) -> std::io::Result<()> {
        let mut ids: Vec<u32> = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name();
            let name = name.to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("seg-").and_then(|r| r.strip_suffix(".log")) {
                if let Ok(id) = rest.parse::<u32>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        let mut max_bid = 0u32;
        {
            let mut blobs = self.blobs.write();
            let mut id2h = self.ids.write();
            for id in ids {
                let path = self.seg_path(id);
                let file = OpenOptions::new().read(true).write(true).open(&path)?;
                let total = file.metadata()?.len();
                if total < 8 {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    continue;
                }
                drop(file);
                let _ = self.new_seg(id);
                {
                    let segs = self.segs.read();
                    let seg = segs.get(&id).unwrap();
                    seg.total.store(total, Ordering::Relaxed);
                    seg.live.store(total, Ordering::Relaxed);
                }
                let mut off = 0u64;
                let mut valid_end = 0u64;
                while off + 8 <= total {
                    let segs = self.segs.read();
                    let seg = segs.get(&id).unwrap();
                    let Some(hdr) = seg.read_at(off, 8, false) else { break };
                    drop(segs);
                    if hdr.len() < 8 {
                        break;
                    }
                    let blen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as u64;
                    let crc = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
                    if blen == 0 || off + 8 + blen > total {
                        break;
                    }
                    let segs = self.segs.read();
                    let seg = segs.get(&id).unwrap();
                    let Some(body) = seg.read_at(off + 8, blen as usize, false) else { break };
                    drop(segs);
                    if crc_of(&body) != crc {
                        break;
                    }
                    let ok = match body.first().copied().unwrap_or(0) {
                        1 => {
                            // legacy blob
                            if body.len() < 26 {
                                false
                            } else {
                                let dict_id = body[1];
                                let hi = u64::from_le_bytes(body[2..10].try_into().unwrap());
                                let lo = u64::from_le_bytes(body[10..18].try_into().unwrap());
                                let raw = u32::from_le_bytes(body[18..22].try_into().unwrap());
                                let comp =
                                    u32::from_le_bytes(body[22..26].try_into().unwrap()) as usize;
                                if body.len() < 26 + comp {
                                    false
                                } else {
                                    let h = ((hi as u128) << 64) | lo as u128;
                                    blobs.insert(
                                        h,
                                        BlobLoc {
                                            seg: id,
                                            off: off + 8,
                                            comp_len: comp as u32,
                                            raw_len: raw,
                                            dict_id,
                                            refs: 0,
                                            id: NO_ID,
                                            slab: false,
                                            share: 0,
                                            slab_off: 0,
                                            slab_raw: 0,
                                            slab_comp: 0,
                                        },
                                    );
                                    true
                                }
                            }
                        }
                        4 => {
                            // BLOB2: [dict][id][hi][lo][raw][cl][comp]
                            if body.len() < 30 {
                                false
                            } else {
                                let dict_id = body[1];
                                let bid = u32::from_le_bytes(body[2..6].try_into().unwrap());
                                let hi = u64::from_le_bytes(body[6..14].try_into().unwrap());
                                let lo = u64::from_le_bytes(body[14..22].try_into().unwrap());
                                let raw = u32::from_le_bytes(body[22..26].try_into().unwrap());
                                let comp =
                                    u32::from_le_bytes(body[26..30].try_into().unwrap()) as usize;
                                if body.len() < 30 + comp {
                                    false
                                } else {
                                    let h = ((hi as u128) << 64) | lo as u128;
                                    blobs.insert(
                                        h,
                                        BlobLoc {
                                            seg: id,
                                            off: off + 8,
                                            comp_len: comp as u32,
                                            raw_len: raw,
                                            dict_id,
                                            refs: 0,
                                            id: bid,
                                            slab: false,
                                            share: 0,
                                            slab_off: 0,
                                            slab_raw: 0,
                                            slab_comp: 0,
                                        },
                                    );
                                    id2h.insert(bid, h);
                                    max_bid = max_bid.max(bid);
                                    true
                                }
                            }
                        }
                        5 => {
                            // SLAB: [dict][cl][raw_total][n][comp]
                            if body.len() < 12 {
                                false
                            } else {
                                let dict_id = body[1];
                                let cl = u32::from_le_bytes(body[2..6].try_into().unwrap()) as usize;
                                let raw_total = u32::from_le_bytes(body[6..10].try_into().unwrap());
                                let n = u16::from_le_bytes(body[10..12].try_into().unwrap()) as usize;
                                if body.len() < 12 + cl || n == 0 {
                                    false
                                } else {
                                    let body_start = off + 8;
                                    let share = ((8 + blen) as u64 / n as u64).min(u32::MAX as u64) as u32;
                                    let last_share = (8 + blen) as u64 - share as u64 * (n as u64 - 1) as u64;
                                    match self.parse_slab(dict_id, &body[12..12 + cl], raw_total, n) {
                                        Some(entries) => {
                                            let mut idx = 0usize;
                                            let mut cursor = (n * 24) as u32; // raws start after the entry table
                                            let cnt = entries.len();
                                            for (bid, rl, h) in entries {
                                                idx += 1;
                                                let sh = if idx == cnt { last_share.min(u32::MAX as u64) as u32 } else { share };
                                                blobs.insert(h, BlobLoc {
                                                    seg: id,
                                                    off: body_start,
                                                    comp_len: rl,
                                                    raw_len: rl,
                                                    dict_id,
                                                    refs: 0,
                                                    id: bid,
                                                    slab: true,
                                                    share: sh,
                                                    slab_off: cursor,
                                                    slab_raw: raw_total,
                                                    slab_comp: cl as u32,
                                                });
                                                id2h.insert(bid, h);
                                                max_bid = max_bid.max(bid);
                                                cursor += rl;
                                            }
                                            true
                                        }
                                        None => false,
                                    }
                                }
                            }
                        }
                        2 => {
                            if body.len() < 13 {
                                false
                            } else {
                                let key = u64::from_le_bytes(body[1..9].try_into().unwrap());
                                self.chunks.write().insert(key, (id, off + 8, blen as u32));
                                true
                            }
                        }
                        6 => {
                            // interned recipe key: [6][u16 len][bytes]
                            if body.len() < 3 {
                                false
                            } else {
                                let l = u16::from_le_bytes(body[1..3].try_into().unwrap()) as usize;
                                if body.len() < 3 + l {
                                    false
                                } else {
                                    let k = String::from_utf8_lossy(&body[3..3 + l]).to_string();
                                    *self.key_refcnt.write().entry(k.clone()).or_default() += 1;
                                    self.keys_tab.write().push(k);
                                    true
                                }
                            }
                        }
                        3 => {
                            if body.len() < 9 {
                                false
                            } else {
                                let key = u64::from_le_bytes(body[1..9].try_into().unwrap());
                                self.chunks.write().remove(&key);
                                true
                            }
                        }
                        _ => true, // unknown: skip (forward compat)
                    };
                    if !ok {
                        break;
                    }
                    off += 8 + blen;
                    valid_end = off;
                }
                // truncate torn tail
                if valid_end < total {
                    let segs = self.segs.read();
                    if let Some(seg) = segs.get(&id) {
                        seg.total.store(valid_end, Ordering::Relaxed);
                        seg.live.store(valid_end, Ordering::Relaxed);
                    }
                    drop(segs);
                    let f = OpenOptions::new().write(true).open(&path)?;
                    f.set_len(valid_end)?;
                    f.sync_data()?;
                }
            }
        }
        self.next_blob_id
            .store(max_bid as u64 + 1, Ordering::Relaxed);
        // pass 2: refcounts + live accounting from live recipes
        let recipe_entries: Vec<(u64, u32, u64, u32)> = self
            .chunks
            .read()
            .iter()
            .map(|(k, (s, o, l))| (*k, *s, *o, *l))
            .collect();
        let mut live_by_seg: HashMap<u32, u64> = HashMap::new();
        {
            let blobs = self.blobs.read();
            for loc in blobs.values() {
                *live_by_seg.entry(loc.seg).or_default() += loc.body_len();
            }
        }
        for (_k, seg, off, len) in recipe_entries {
            if let Some(rec) = self.load_recipe(seg, off, len) {
                let mut blobs = self.blobs.write();
                for h in rec.blob_hashes() {
                    if let Some(loc) = blobs.get_mut(&h) {
                        loc.refs += 1;
                    }
                }
                *live_by_seg.entry(seg).or_default() += 8 + len as u64;
            }
        }
        {
            let segs = self.segs.read();
            for (id, seg) in segs.iter() {
                seg.live.store(*live_by_seg.get(id).unwrap_or(&0), Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn load_recipe(&self, seg: u32, off: u64, len: u32) -> Option<Recipe> {
        if Some(seg) == self.writer_seg_id() {
            self.flush_writer();
        }
        let segs = self.segs.read();
        let s = segs.get(&seg)?;
        let sealed = Some(seg) != self.writer_seg_id();
        let body = s.read_at(off, len as usize, sealed)?;
        // body = [2][u64 key][u32 rec_len][recipe..]
        if body.first() != Some(&2) || body.len() < 13 {
            return None;
        }
        let rl = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
        if body.len() < 13 + rl {
            return None;
        }
        let id2h = self.ids.read();
        let keytab = self.keys_tab.read();
        Recipe::parse(&body[13..13 + rl], &id2h, &keytab)
    }

    fn writer_seg_id(&self) -> Option<u32> {
        self.writer.lock().as_ref().map(|w| w.seg_id)
    }

    /// Push the append buffer to the OS. Called before any read of the
    /// CURRENT segment so readers observe all acknowledged writes.
    fn flush_writer(&self) {
        let mut wg = self.writer.lock();
        if let Some(w) = wg.as_mut() {
            if !w.buf.is_empty() {
                let _ = w.file.seek(SeekFrom::Start(w.written - w.buf.len() as u64));
                let _ = w.file.write_all(&w.buf);
                w.buf.clear();
            }
        }
    }

    fn append(&self, body: &[u8]) -> std::io::Result<(u32, u64, u32)> {
        let mut wg = self.writer.lock();
        if wg.is_none() {
            let id = self.next_seg.fetch_add(1, Ordering::SeqCst) as u32;
            let file = self.new_seg(id)?;
            *wg = Some(Writer { file, seg_id: id, written: 0, buf: Vec::with_capacity(BUF_CAP) });
        }
        let w = wg.as_mut().unwrap();
        if w.written > 0 && w.written + body.len() as u64 > SEG_ROLL {
            w.file.seek(SeekFrom::Start(w.written))?;
            if !w.buf.is_empty() {
                w.file.write_all(&w.buf)?;
                w.buf.clear();
            }
            let _ = w.file.sync_data();
            let id = self.next_seg.fetch_add(1, Ordering::SeqCst) as u32;
            let file = self.new_seg(id)?;
            w.file = file;
            w.seg_id = id;
            w.written = 0;
        }
        let mut c = Crc::new();
        c.update(body);
        let crc = c.finalize();
        w.buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        w.buf.extend_from_slice(&crc.to_le_bytes());
        let body_off = w.written + 8;
        w.buf.extend_from_slice(body);
        w.written += 8 + body.len() as u64;
        if w.buf.len() >= BUF_CAP {
            w.file.write_all(&w.buf)?;
            w.buf.clear();
        }
        if let Some(s) = self.segs.read().get(&w.seg_id) {
            s.total.fetch_add(8 + body.len() as u64, Ordering::Relaxed);
            s.live.fetch_add(8 + body.len() as u64, Ordering::Relaxed);
        }
        self.disk_written.fetch_add(8 + body.len() as u64, Ordering::Relaxed);
        Ok((w.seg_id, body_off, body.len() as u32))
    }

    // ---- compression tiers ----

    /// Hot tier: write path. Cold tier: background compaction.
    fn compress_tier(&self, data: &[u8], cold: bool) -> (u8, Vec<u8>) {
        let level = if cold { self.cfg.cold_level } else { self.cfg.hot_level };
        let dict = self.current_dict();
        let dver = dict.as_ref().map(|(v, _)| *v).unwrap_or(0);
        let key = (self.uid, dver, level);
        let dbuf = dict.as_ref().map(|(_, d)| d.as_slice());
        let run = |slot: &RefCell<Option<CompCtx>>| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().map(|s| s.key) != Some(key) {
                *slot = build_comp(level, dbuf).ok().map(|comp| CompCtx { key, comp });
            }
            match slot.as_mut() {
                Some(ctx) => match ctx.comp.compress(data) {
                    Ok(out) => (dver, out),
                    Err(_) => (0, data.to_vec()),
                },
                None => match zstd::bulk::compress(data, level) {
                    Ok(out) => (0, out),
                    Err(_) => (0, data.to_vec()),
                },
            }
        };
        if cold {
            COLD_C.with(|c| run(c))
        } else {
            HOT_C.with(|c| run(c))
        }
    }

    fn decompress(&self, dict_id: u8, data: &[u8], raw_len: u32) -> Option<Vec<u8>> {
        if dict_id == 0 {
            return zstd::bulk::decompress(data, raw_len as usize).ok();
        }
        DEC_C.with(|c| {
            let mut slot = c.borrow_mut();
            let Some(dict) = self.dicts.read().get(&dict_id).cloned() else {
                slot.take();
                return None;
            };
            let key = (self.uid, dict_id);
            if slot.as_ref().map(|s| s.key) != Some(key) {
                *slot = build_dec(Some(&dict)).ok().map(|dec| DecCtx { key, dec });
            }
            match slot.as_mut() {
                Some(ctx) => ctx.dec.decompress(data, raw_len as usize).ok(),
                None => None,
            }
        })
    }

    // ---- blob mgmt ----

    /// Returns (already_existed, blob_id). Refcount is handled by recipes.
    fn ensure_blob(&self, h: Hash, raw: &[u8], cold: bool) -> Result<(bool, u32), String> {
        if let Some(loc) = self.blobs.read().get(&h) {
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            return Ok((true, loc.id));
        }
        let (dict_id, comp) = self.compress_tier(raw, cold);
        let id = self.next_blob_id.fetch_add(1, Ordering::Relaxed) as u32;
        let mut body = Vec::with_capacity(30 + comp.len());
        body.push(4u8);
        body.push(dict_id);
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
        body.extend_from_slice(&(h as u64).to_le_bytes());
        body.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        body.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        body.extend_from_slice(&comp);
        let (seg, off, _len) = self.append(&body).map_err(|e| e.to_string())?;
        self.blobs.write().insert(h, BlobLoc {
            seg, off, comp_len: comp.len() as u32, raw_len: raw.len() as u32,
            dict_id, refs: 0, id,
            slab: false, share: 0, slab_off: 0, slab_raw: 0, slab_comp: 0,
        });
        self.ids.write().insert(id, h);
        self.dict_sample(raw);
        Ok((false, id))
    }

    fn bump_recipe_refs(&self, rec: &Recipe) {
        let mut blobs = self.blobs.write();
        for h in rec.blob_hashes() {
            if let Some(loc) = blobs.get_mut(&h) {
                loc.refs += 1;
            }
        }
    }

    fn drop_recipe_refs(&self, rec: &Recipe) {
        let mut blobs = self.blobs.write();
        for h in rec.blob_hashes() {
            let evict = {
                let Some(loc) = blobs.get_mut(&h) else { continue };
                loc.refs = loc.refs.saturating_sub(1);
                loc.refs == 0
            };
            if evict {
                if let Some(loc) = blobs.remove(&h) {
                    self.ids.write().remove(&loc.id);
                    if let Some(s) = self.segs.read().get(&loc.seg) {
                        s.live.fetch_sub(loc.body_len(), Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Parse a slab record's compressed stream into (id, raw_len, hash) entries.
    fn parse_slab(
        &self,
        dict_id: u8,
        comp: &[u8],
        raw_total: u32,
        n: usize,
    ) -> Option<Vec<(u32, u32, Hash)>> {
        let raw = self.decompress(dict_id, comp, raw_total)?;
        if raw.len() < n * 24 {
            return None;
        }
        let mut out = Vec::with_capacity(n);
        let mut p = 0usize;
        for _ in 0..n {
            let bid = u32::from_le_bytes(raw[p..p + 4].try_into().ok()?);
            let rl = u32::from_le_bytes(raw[p + 4..p + 8].try_into().ok()?);
            let hi = u64::from_le_bytes(raw[p + 8..p + 16].try_into().ok()?);
            let lo = u64::from_le_bytes(raw[p + 16..p + 24].try_into().ok()?);
            p += 24;
            out.push((bid, rl, ((hi as u128) << 64) | lo as u128));
        }
        let total_raw: u64 = out.iter().map(|(_, rl, _)| *rl as u64).sum();
        if raw.len() != p + total_raw as usize {
            return None;
        }
        Some(out)
    }

    /// Intern a recipe key name; persists it as a kind-6 record so scans
    /// rebuild the table in log order.
    fn intern_key(&self, key: &str) -> Result<u16, String> {
        {
            let t = self.keys_tab.read();
            if let Some(i) = t.iter().position(|k| k == key) {
                return Ok(i as u16);
            }
        }
        let mut body = Vec::with_capacity(3 + key.len());
        body.push(6u8);
        body.extend_from_slice(&(key.len() as u16).to_le_bytes());
        body.extend_from_slice(key.as_bytes());
        self.append(&body).map_err(|e| e.to_string())?;
        *self.key_refcnt.write().entry(key.to_string()).or_default() += 1;
        let mut t = self.keys_tab.write();
        // double-check under write lock (concurrent interning)
        if let Some(i) = t.iter().position(|k| k == key) {
            return Ok(i as u16);
        }
        if t.len() >= u16::MAX as usize {
            return Err("key table full".into());
        }
        t.push(key.to_string());
        Ok((t.len() - 1) as u16)
    }

    // ---- public chunk API ----

    /// Store one chunk payload. `whole_mode` is used for entity/poi payloads
    /// and as a fallback when the chunk has no section structure.
    pub fn write_chunk(&self, key: u64, payload: &[u8], whole_mode: bool) -> Result<(), String> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.logical_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
        let pos_hint = Some(((key >> 32) as i32, (key & 0xffff_ffff) as i32));
        let split = chunknbt::split(payload, whole_mode, pos_hint)?;
        let recipe = match split {
            Split::Whole { hash } => {
                self.ensure_blob(hash, payload, false)?;
                Recipe { whole: Some(hash), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false }
            }
            Split::Fields { parts, pos_from_key } => {
                let mut fields = Vec::with_capacity(parts.len());
                for p in parts {
                    if p.inline {
                        fields.push((self.intern_key(&p.key)?, FieldVal::Inline { raw: p.bytes }));
                    } else {
                        self.ensure_blob(p.hash, &p.bytes, false)?;
                        fields.push((self.intern_key(&p.key)?, FieldVal::Hash(p.hash)));
                    }
                }
                Recipe { whole: None, sections: Vec::new(), tail: Tail::None, fields, pos_from_key }
            }
            Split::Split { sections, tail } => {
                if sections.len() > MAX_SECTIONS {
                    let h = chunknbt::hash_of(payload);
                    self.ensure_blob(h, payload, false)?;
                    Recipe { whole: Some(h), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false }
                } else {
                    let mut hashes = Vec::with_capacity(sections.len());
                    for (h, bytes) in &sections {
                        self.ensure_blob(*h, bytes, false)?;
                        hashes.push(*h);
                    }
                    // tail: dedup large tails as blobs, inline tiny ones
                    let (tdict, tcomp) = self.compress_tier(&tail, false);
                    let tail_ref = if tail.len() >= self.cfg.tail_blob_min {
                        let th = chunknbt::hash_of(&tail);
                        self.ensure_blob(th, &tail, false)?;
                        Tail::Blob(th)
                    } else {
                        Tail::Inline { dict: tdict, comp: tcomp, raw: tail.len() as u32 }
                    };
                    Recipe { whole: None, sections: hashes, tail: tail_ref, fields: Vec::new(), pos_from_key: false }
                }
            }
        };

        let id_of = |h: Hash| -> Option<u32> {
            let blobs = self.blobs.read();
            let l = blobs.get(&h)?;
            if l.id == NO_ID { None } else { Some(l.id) }
        };
        let rec = if recipe.fields.is_empty() {
            recipe.encode(id_of)
        } else {
            recipe
                .encode_fields(id_of)
                .ok_or_else(|| "field recipe too large".to_string())?
        };
        let mut body = Vec::with_capacity(13 + rec.len());
        body.push(2u8);
        body.extend_from_slice(&key.to_le_bytes());
        body.extend_from_slice(&(rec.len() as u32).to_le_bytes());
        body.extend_from_slice(&rec);
        let (seg, off, len) = self.append(&body).map_err(|e| e.to_string())?;

        let old = self.chunks.write().insert(key, (seg, off, len));
        self.bump_recipe_refs(&recipe);
        if let Some((oseg, ooff, olen)) = old {
            if let Some(old_rec) = self.load_recipe(oseg, ooff, olen) {
                self.drop_recipe_refs(&old_rec);
            }
            if let Some(s) = self.segs.read().get(&oseg) {
                s.live.fetch_sub(8 + olen as u64, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Load one chunk payload (uncompressed NBT, `NbtIo.write` form).
    pub fn read_chunk(&self, key: u64) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let (seg, off, len) = *self.chunks.read().get(&key)?;
        let dbg = |stage: &str| {
            if std::env::var("CSAVES_READ_DEBUG").map(|v| v == "1").unwrap_or(false) {
                eprintln!("[c-saves] read fail key={key:016x} seg={seg} off={off} len={len} stage={stage}");
            }
        };
        let fail = |stage: &str| -> Option<Vec<u8>> {
            dbg(stage);
            None
        };
        let rec = match self.load_recipe(seg, off, len) {
            Some(r) => r,
            None => {
                dbg("recipe");
                return None;
            }
        };
        let writer_id = self.writer_seg_id();
        let tail_bytes: Option<Vec<u8>> = match &rec.tail {
            Tail::None => Some(Vec::new()),
            Tail::Inline { dict, comp, raw } => self.decompress(*dict, comp, *raw),
            Tail::Blob(h) => {
                let loc = *self.blobs.read().get(h)?;
                self.read_blob(loc, writer_id)
            }
        };
        match rec.whole {
            Some(h) => {
                let loc = *self.blobs.read().get(&h)?;
                self.read_blob(loc, writer_id).or_else(|| fail("whole-blob"))
            }
            None if !rec.fields.is_empty() => {
                let keytab = self.keys_tab.read();
                let mut parts = Vec::with_capacity(rec.fields.len());
                for (kid, val) in &rec.fields {
                    let key = keytab.get(*kid as usize)?; // parse checked bounds
                    let bytes = match val {
                        FieldVal::Inline { raw } => raw.clone(),
                        FieldVal::Hash(h) => {
                            let loc = *self.blobs.read().get(h)?;
                            self.read_blob(loc, writer_id).or_else(|| fail("field-blob"))?
                        }
                    };
                    parts.push(chunknbt::FieldBytes { key: key.clone(), inline: matches!(val, FieldVal::Inline { .. }), bytes });
                }
                drop(keytab);
                let pos = if rec.pos_from_key {
                    Some(((key >> 32) as i32, (key & 0xffff_ffff) as i32))
                } else {
                    None
                };
                match chunknbt::join_fields(&parts, pos) {
                    Ok(v) => Some(v),
                    Err(e) => fail(&format!("join_fields: {e}")),
                }
            }
            None => {
                let tail = tail_bytes.or_else(|| fail("tail"))?;
                let blobs = self.blobs.read();
                let mut locs = Vec::with_capacity(rec.sections.len());
                for h in &rec.sections {
                    locs.push(*blobs.get(h)?);
                }
                drop(blobs);
                let mut sec_bytes = Vec::with_capacity(locs.len());
                for loc in locs {
                    sec_bytes.push(self.read_blob(loc, writer_id).or_else(|| fail("section-blob"))?);
                }
                match chunknbt::join(&sec_bytes, &tail) {
                    Ok(v) => Some(v),
                    Err(e) => fail(&format!("join: {e}")),
                }
            }
        }
    }

    fn read_blob(&self, loc: BlobLoc, writer_id: Option<u32>) -> Option<Vec<u8>> {
        if writer_id == Some(loc.seg) {
            self.flush_writer();
        }
        let segs = self.segs.read();
        let seg = segs.get(&loc.seg)?;
        let sealed = Some(loc.seg) != writer_id;
        if loc.slab {
            let key = (loc.seg, loc.off);
            if let Some(cached) = self.slab_cache.lock().get(&key) {
                let s = cached.get(loc.slab_off as usize..(loc.slab_off + loc.raw_len) as usize)?;
                return Some(s.to_vec());
            }
            let hdr = seg.read_at(loc.off, 12, sealed)?;
            if hdr.first() != Some(&5) {
                return None;
            }
            let cl = u32::from_le_bytes(hdr[2..6].try_into().ok()?) as usize;
            let raw_total = u32::from_le_bytes(hdr[6..10].try_into().ok()?);
            let data = seg.read_at(loc.off + 12, cl, sealed)?;
            let raw = self.decompress(loc.dict_id, &data, raw_total)?;
            let out = raw
                .get(loc.slab_off as usize..(loc.slab_off + loc.raw_len) as usize)?
                .to_vec();
            drop(segs);
            self.slab_cache.lock().put(key, Arc::new(raw));
            return Some(out);
        }
        let data = seg.read_at(loc.off + loc.header_len(), loc.comp_len as usize, sealed)?;
        self.decompress(loc.dict_id, &data, loc.raw_len)
    }

    pub fn debug_blob_state(&self, key: u64) -> String {
        let Some(rec) = self.load_recipe_for_debug(key) else {
            return format!("key {key:016x}: no recipe");
        };
        let mut out = String::new();
        out.push_str(&format!("whole={:?} nsec={} tail={:?}; ", rec.whole.is_some(), rec.sections.len(), matches!(rec.tail, Tail::Blob(_))));
        for h in rec.blob_hashes() {
            if let Some(l) = self.blobs.read().get(&h) {
                out.push_str(&format!("[h={:016x}.. seg={} off={} raw={} slab={} share={} soff={} sraw={} scomp={} dict={}], ",
                    h as u64, l.seg, l.off, l.raw_len, l.slab, l.share, l.slab_off, l.slab_raw, l.slab_comp, l.dict_id));
            } else {
                out.push_str(&format!("[h={:016x}.. MISSING], ", h as u64));
            }
        }
        out
    }

    fn load_recipe_for_debug(&self, key: u64) -> Option<Recipe> {
        let (seg, off, len) = *self.chunks.read().get(&key)?;
        self.load_recipe(seg, off, len)
    }

    pub fn has_chunk(&self, key: u64) -> bool {
        self.chunks.read().contains_key(&key)
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.read().len()
    }

    pub fn keys(&self) -> Vec<u64> {
        self.chunks.read().keys().copied().collect()
    }

    pub fn delete_chunk(&self, key: u64) -> Result<(), String> {
        let old = self.chunks.write().remove(&key);
        if let Some((seg, off, len)) = old {
            if let Some(rec) = self.load_recipe(seg, off, len) {
                self.drop_recipe_refs(&rec);
            }
            if let Some(s) = self.segs.read().get(&seg) {
                s.live.fetch_sub(8 + len as u64, Ordering::Relaxed);
            }
        }
        let mut body = vec![3u8];
        body.extend_from_slice(&key.to_le_bytes());
        self.append(&body).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// fsync all segments + flush buffers (end of a save cycle).
    pub fn sync(&self) {
        self.flush_writer();
        {
            let wg = self.writer.lock();
            if let Some(w) = wg.as_ref() {
                let _ = w.file.sync_data();
            }
        }
        let segs = self.segs.read();
        for s in segs.values() {
            let _ = s.file.sync_data();
        }
    }

    /// Seal the active segment: flush, fsync, and detach it as the writer so
    /// the next append opens a fresh segment. Sealed segments become immutable
    /// (mmap reads) and eligible for background compaction.
    pub fn seal(&self) {
        self.flush_writer();
        let mut wg = self.writer.lock();
        if let Some(w) = wg.as_ref() {
            let _ = w.file.sync_data();
        }
        *wg = None;
    }

    pub fn disk_bytes(&self) -> u64 {
        self.segs.read().values().map(|s| s.total.load(Ordering::Relaxed)).sum()
    }

    pub fn logical(&self) -> u64 {
        self.logical_bytes.load(Ordering::Relaxed)
    }

    pub fn stats_json(&self) -> String {
        format!(
            r#"{{"chunks":{},"blobs":{},"dedup_hits":{},"writes":{},"reads":{},"disk":{},"logical":{},"dict_v":{}}}"#,
            self.chunk_count(),
            self.blobs.read().len(),
            self.dedup_hits.load(Ordering::Relaxed),
            self.writes.load(Ordering::Relaxed),
            self.reads.load(Ordering::Relaxed),
            self.disk_bytes(),
            self.logical(),
            self.cur_dict.load(Ordering::Relaxed)
        )
    }

    /// Exact on-disk accounting: every byte of the container attributed to
    /// record framing, blob headers, compressed payload, slabs, recipes or
    /// dictionaries. Returns a JSON object.
    pub fn profile_json(&self) -> String {
        let blobs = self.blobs.read();
        let mut rec_frame = 0u64; // 8-byte [len][crc] per record
        let mut blob_hdr = 0u64; // kind-4 headers
        let mut blob_comp = 0u64; // compressed bytes in kind-4
        let mut blob_n = 0u64;
        let mut slab_frame = 0u64; // kind-5 headers (12) + framing, full
        let mut slab_comp = 0u64; // compressed slab streams
        let mut slab_share = 0u64; // sum of member shares (attribution view)
        let mut slab_n = 0u64; // members
        let mut raw_regular = 0u64;
        let mut slab_seen: std::collections::HashSet<(u32, u64)> = std::collections::HashSet::new();
        let mut slab_records: std::collections::HashMap<(u32, u64), (u64, u64, usize)> =
            std::collections::HashMap::new(); // -> (frame, comp, members)
        for l in blobs.values() {
            if l.slab {
                slab_n += 1;
                slab_share += l.share as u64;
                let e = slab_records.entry((l.seg, l.off)).or_insert((0, 0, 0));
                e.2 += 1;
            } else {
                blob_n += 1;
                blob_hdr += l.header_len();
                blob_comp += l.comp_len as u64;
                raw_regular += l.raw_len as u64;
                rec_frame += 8;
            }
        }
        for (&(seg, off), e) in slab_records.iter_mut() {
            // reconstruct the kind-5 record size: hdr 12 + comp; framing 8
            if let Some(s) = self.segs.read().get(&seg) {
                let hdr = s.read_at(off, 12, true);
                if let Some(h) = hdr {
                    let cl = u32::from_le_bytes(h[2..6].try_into().unwrap()) as u64;
                    e.0 = 8 + 12 + cl;
                    e.1 = cl;
                }
            }
            slab_frame += e.0;
            slab_comp += e.1;
        }
        // recipes: live chunk records
        let mut rec_bytes = 0u64;
        {
            let chunks = self.chunks.read();
            for (_k, (_s, _o, l)) in chunks.iter() {
                rec_bytes += 8 + *l as u64;
            }
        }
        // dictionaries on disk
        let mut dict_bytes = 0u64;
        if let Ok(rd) = fs::read_dir(self.dict_dir()) {
            for e in rd.flatten() {
                dict_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        // segment headers that are unaccounted (should be ~0)
        let attributed = rec_frame + blob_hdr + blob_comp + slab_frame + rec_bytes;
        let disk = self.disk_bytes();
        let unattr = disk.saturating_sub(attributed);
        let mut s = String::with_capacity(1024);
        s.push_str("{");
        s.push_str(&format!(r#""disk":{disk},"#));
        s.push_str(&format!(r#""recipes":{{"count":{},"bytes":{rec_bytes}}},"#, self.chunk_count()));
        s.push_str(&format!(
            r#""blobs_regular":{{"count":{blob_n},"headers":{blob_hdr},"comp":{blob_comp},"raw":{raw_regular}}},"#
        ));
        s.push_str(&format!(
            r#""slabs":{{"records_frame":{slab_frame},"comp":{slab_comp},"members":{slab_n},"member_share":{slab_share}}},"#
        ));
        s.push_str(&format!(r#""dicts":{dict_bytes},"#));
        s.push_str(&format!(r#""unattributed":{unattr}"#));
        s.push_str("}");
        s
    }

    /// Per-blob compressed-size histogram bucketed by raw size class
    /// (write-path insight: which payload sizes dominate the container).
    pub fn blob_histogram_json(&self) -> String {
        let buckets: [(u64, &str); 8] = [
            (128, "b<=128"),
            (512, "b<=512"),
            (2048, "b<=2k"),
            (4096, "b<=4k"),
            (16384, "b<=16k"),
            (65536, "b<=64k"),
            (262144, "b<=256k"),
            (u64::MAX, "b>256k"),
        ];
        let mut out = String::from("{");
        let blobs = self.blobs.read();
        let mut raw_tot = vec![0u64; buckets.len()];
        let mut comp_tot = vec![0u64; buckets.len()];
        let mut cnt = vec![0u64; buckets.len()];
        for l in blobs.values() {
            let raw = l.raw_len as u64;
            let i = buckets.iter().position(|(t, _)| raw <= *t).unwrap_or(buckets.len() - 1);
            cnt[i] += 1;
            raw_tot[i] += raw;
            comp_tot[i] += if l.slab { l.share as u64 } else { l.body_len() };
        }
        for (i, (_, name)) in buckets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                r#""{}":{{"n":{},"raw":{},"disk":{}}}"#,
                name, cnt[i], raw_tot[i], comp_tot[i]
            ));
        }
        out.push('}');
        out
    }

    // ---- compaction ----

    /// Pack accumulated small-blob candidates into kind-5 slab record(s) in
    /// the current segment, then point their index entries at the slab.
    fn flush_slab_cands(&self, cands: &mut Vec<(Hash, u32, BlobLoc, Vec<u8>)>, worst_id: u32) {
        if cands.is_empty() {
            return;
        }
        // split into slab-sized groups
        let mut groups: Vec<Vec<(Hash, u32, BlobLoc, Vec<u8>)>> = Vec::new();
        let mut cur: Vec<(Hash, u32, BlobLoc, Vec<u8>)> = Vec::new();
        let mut cur_raw = 24usize; // entry table floor
        for c in cands.drain(..) {
            let need = 24 + c.3.len();
            if !cur.is_empty() && cur_raw + need > self.cfg.slab_max_raw {
                groups.push(std::mem::take(&mut cur));
                cur_raw = 24;
            }
            cur_raw += need;
            cur.push(c);
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        for group in groups {
            let n = group.len();
            let mut stream = Vec::with_capacity(n * 24 + group.iter().map(|c| c.3.len()).sum::<usize>());
            let mut rawcat = Vec::with_capacity(stream.capacity());
            for (h, id, _loc, raw) in &group {
                stream.extend_from_slice(&id.to_le_bytes());
                stream.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                stream.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                stream.extend_from_slice(&(*h as u64).to_le_bytes());
                rawcat.extend_from_slice(raw);
            }
            stream.extend_from_slice(&rawcat);
            let raw_total = stream.len() as u32;
            let (dict_id, comp) = self.compress_tier(&stream, true);
            let mut body = Vec::with_capacity(12 + comp.len());
            body.push(5u8);
            body.push(dict_id);
            body.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            body.extend_from_slice(&raw_total.to_le_bytes());
            body.extend_from_slice(&(n as u16).to_le_bytes());
            body.extend_from_slice(&comp);
            let blen = body.len() as u64;
            let Ok((seg, off, _)) = self.append(&body) else { continue };
            let share = ((8 + blen) as u64 / n as u64).min(u32::MAX as u64) as u32;
            let last_share = (8 + blen) as u64 - share as u64 * (n as u64 - 1) as u64;
            let mut blobs = self.blobs.write();
            let cnt = group.len();
            for (i, (h, _id, old_loc, _raw)) in group.iter().enumerate() {
                if let Some(l) = blobs.get_mut(h) {
                    if let Some(oldseg) = self.segs.read().get(&worst_id) {
                        oldseg.live.fetch_sub(old_loc.body_len(), Ordering::Relaxed);
                    }
                    l.seg = seg;
                    l.off = off;
                    l.dict_id = dict_id;
                    l.slab = true;
                    l.share = if i + 1 == cnt { last_share.min(u32::MAX as u64) as u32 } else { share };
                    l.slab_off = 0; // patched below by cursor
                    l.slab_raw = raw_total;
                    l.slab_comp = comp.len() as u32;
                    l.comp_len = old_loc.raw_len; // own raw size (mirrors raw_len)
                }
            }
            // patch slab_off per member in order (raws start after entry table)
            let mut cursor = (n * 24) as u32;
            for (h, _id, old_loc, _raw) in group.iter() {
                if let Some(l) = blobs.get_mut(h) {
                    l.slab_off = cursor;
                }
                cursor += old_loc.raw_len;
            }
            drop(blobs);
        }
    }

    /// Re-append kind-6 key records of a dying segment so the interned key
    /// table survives compaction (indices of existing keys are stable; only
    /// names missing from the table are re-recorded).
    fn preserve_keys(&self, seg_id: u32) {
        let total = match self.segs.read().get(&seg_id) {
            Some(s) => s.total.load(Ordering::Relaxed),
            None => return,
        };
        let mut off = 0u64;
        while off + 8 <= total {
            let body = {
                let segs = self.segs.read();
                let Some(seg) = segs.get(&seg_id) else { return };
                let Some(hdr) = seg.read_at(off, 8, true) else { return };
                let blen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as u64;
                if blen == 0 || off + 8 + blen > total {
                    return;
                }
                match seg.read_at(off + 8, blen as usize, true) {
                    Some(b) => b,
                    None => return,
                }
            };
            if body.first() == Some(&6) && body.len() >= 3 {
                let l = u16::from_le_bytes(body[1..3].try_into().unwrap()) as usize;
                if body.len() >= 3 + l {
                    let name = String::from_utf8_lossy(&body[3..3 + l]).to_string();
                    let mut refs = self.key_refcnt.write();
                    let e = refs.entry(name.clone()).or_default();
                    if *e <= 1 {
                        // last on-disk record dies with this segment -> re-record
                        *e = 1;
                        drop(refs);
                        let mut rec = Vec::with_capacity(3 + name.len());
                        rec.push(6u8);
                        rec.extend_from_slice(&(name.len() as u16).to_le_bytes());
                        rec.extend_from_slice(name.as_bytes());
                        if self.append(&rec).is_ok() {
                            let mut t = self.keys_tab.write();
                            if !t.iter().any(|k| *k == name) {
                                t.push(name);
                            }
                        }
                    } else {
                        *e -= 1;
                    }
                }
            }
            off += 8 + body.len() as u64;
        }
    }

    /// Rewrite mode-5 (field split) recipes when it shrinks them:
    ///   * inline parts whose exact bytes repeat across live recipes become
    ///     dedup blobs (empty Entities lists, constant scalars, ...);
    ///   * single-ref small blobs become inline raw bytes.
    /// Runs as the fallback work item of compact_step (no dead space needed).
    /// Returns true when any recipe was rewritten.
    fn optimize_fields_step(&self) -> bool {
        let live: Vec<(u64, u32, u64, u32)> = {
            let chunks = self.chunks.read();
            chunks.iter().map(|(k, (s, o, l))| (*k, *s, *o, *l)).collect()
        };
        if live.is_empty() {
            return false;
        }
        let mut counts: HashMap<Hash, u32> = HashMap::new();
        let mut has_demote = false;
        let mut parsed: Vec<(u64, u32, u64, u32, Recipe)> = Vec::new();
        {
            let blobs = self.blobs.read();
            for (key, seg, off, len) in &live {
                let Some(rec) = self.load_recipe(*seg, *off, *len) else { continue };
                if rec.whole.is_some() || !rec.sections.is_empty() || rec.fields.is_empty() {
                    continue;
                }
                for (_, f) in &rec.fields {
                    match f {
                        FieldVal::Inline { raw } => {
                            *counts.entry(chunknbt::hash_of(raw)).or_default() += 1;
                        }
                        FieldVal::Hash(h) => {
                            if let Some(loc) = blobs.get(h) {
                                if loc.refs <= 1 && loc.raw_len <= 64 {
                                    has_demote = true;
                                }
                            }
                        }
                    }
                }
                parsed.push((*key, *seg, *off, *len, rec));
            }
        }
        let promote: std::collections::HashSet<Hash> = counts
            .iter()
            .filter(|(_, c)| **c >= 2)
            .map(|(h, _)| *h)
            .collect();
        if promote.is_empty() && !has_demote {
            return false;
        }
        let mut rewritten = 0usize;
        for (key, seg, off, len, rec) in parsed {
            // skip recipes superseded since capture (concurrent rewrite)
            if *self.chunks.read().get(&key).unwrap_or(&(0, 0, 0)) != (seg, off, len) {
                continue;
            }
            let mut changed = false;
            let mut new_fields = Vec::with_capacity(rec.fields.len());
            for (kid, f) in &rec.fields {
                match f {
                    FieldVal::Inline { raw } => {
                        let h = chunknbt::hash_of(raw);
                        if promote.contains(&h) {
                            let Some(v) = chunknbt::read_unnamed(raw) else {
                                new_fields.push((*kid, f.clone()));
                                continue;
                            };
                            let sh = chunknbt::structural_hash(&v);
                            let wrapped = chunknbt::wrap(&v);
                            match self.ensure_blob(sh, &wrapped, false) {
                                Ok(_) => {
                                    new_fields.push((*kid, FieldVal::Hash(sh)));
                                    changed = true;
                                }
                                Err(_) => new_fields.push((*kid, f.clone())),
                            }
                        } else {
                            new_fields.push((*kid, f.clone()));
                        }
                    }
                    FieldVal::Hash(h) => {
                        let loc = self.blobs.read().get(h).copied();
                        let mut demoted = false;
                        if let Some(loc) = loc {
                            if loc.refs <= 1 && loc.raw_len <= 64 {
                                if let Some(data) = self.read_blob(loc, self.writer_seg_id()) {
                                    if let Some(v) = chunknbt::unwrap_wrapped(&data) {
                                        let mut raw2 = Vec::with_capacity(48);
                                        if chunknbt::write_unnamed(&v, &mut raw2).is_some()
                                            && raw2.len() <= 255
                                        {
                                            new_fields.push((*kid, FieldVal::Inline { raw: raw2 }));
                                            changed = true;
                                            demoted = true;
                                        }
                                    }
                                }
                            }
                        }
                        if !demoted {
                            new_fields.push((*kid, f.clone()));
                        }
                    }
                }
            }
            if !changed {
                continue;
            }
            if *self.chunks.read().get(&key).unwrap_or(&(0, 0, 0)) != (seg, off, len) {
                continue;
            }
            let new_rec = Recipe {
                whole: None,
                sections: Vec::new(),
                tail: Tail::None,
                fields: new_fields,
                pos_from_key: rec.pos_from_key,
            };
            let id_of = |h: Hash| -> Option<u32> {
                let blobs = self.blobs.read();
                blobs.get(&h).and_then(|l| if l.id == NO_ID { None } else { Some(l.id) })
            };
            let Some(enc) = new_rec.encode_fields(id_of) else { continue };
            let mut body = Vec::with_capacity(13 + enc.len());
            body.push(2u8);
            body.extend_from_slice(&key.to_le_bytes());
            body.extend_from_slice(&(enc.len() as u32).to_le_bytes());
            body.extend_from_slice(&enc);
            let Ok((nseg, noff, nlen)) = self.append(&body) else { continue };
            self.chunks.write().insert(key, (nseg, noff, nlen));
            self.bump_recipe_refs(&new_rec);
            if let Some(old_rec) = self.load_recipe(seg, off, len) {
                self.drop_recipe_refs(&old_rec);
            }
            if let Some(s) = self.segs.read().get(&seg) {
                s.live.fetch_sub(8 + len as u64, Ordering::Relaxed);
            }
            rewritten += 1;
        }
        rewritten > 0
    }

    /// Compact at most one segment: move live data (recompressing blobs at
    /// the cold tier), then drop the segment. Returns true if work was done.
    pub fn compact_step(&self) -> bool {
        self.train_dict_if_needed();
        // seal any open writer so previously appended data (including our
        // own past moves and optimize rewrites) is eligible for selection;
        // without this, dead bytes trapped in the writer segment would never
        // be reclaimed (the writer segment is exempt below).
        self.seal();
        let writer_id = self.writer_seg_id().unwrap_or(u32::MAX);
        let worst = {
            let segs = self.segs.read();
            let mut worst: Option<(u32, f64)> = None;
            for (id, s) in segs.iter() {
                if *id == writer_id {
                    continue;
                }
                let total = s.total.load(Ordering::Relaxed);
                let live = s.live.load(Ordering::Relaxed);
                if total > self.cfg.compact_min {
                    let r = 1.0 - live as f64 / total as f64;
                    if worst.map(|w| r > w.1).unwrap_or(true) {
                        worst = Some((*id, r));
                    }
                }
            }
            worst
        };
        let Some((worst_id, ratio)) = worst else { return self.optimize_fields_step(); };
        if ratio < self.cfg.compact_ratio {
            return self.optimize_fields_step();
        }
        // move live blobs: big ones as recompressed BLOB2 (cold tier), small
        // ones batched into kind-5 slab records (one header, one stream)
        let victims: Vec<(Hash, BlobLoc)> = {
            let blobs = self.blobs.read();
            blobs.iter().filter(|(_, l)| l.seg == worst_id).map(|(h, l)| (*h, *l)).collect()
        };
        let mut cands: Vec<(Hash, u32, BlobLoc, Vec<u8>)> = Vec::new();
        let mut cand_bytes = 0usize;
        for (h, loc) in victims {
            let Some(data) = self.read_blob(loc, Some(worst_id)) else { continue };
            if (data.len() as u32) <= self.cfg.slab_max_member {
                let id = if loc.id != NO_ID {
                    loc.id
                } else {
                    self.next_blob_id.fetch_add(1, Ordering::Relaxed) as u32
                };
                cand_bytes += data.len();
                cands.push((h, id, loc, data));
                if cand_bytes >= self.cfg.slab_max_raw {
                    self.flush_slab_cands(&mut cands, worst_id);
                    cand_bytes = 0;
                }
                continue;
            }
            let id = if loc.id != NO_ID {
                loc.id
            } else {
                self.next_blob_id.fetch_add(1, Ordering::Relaxed) as u32
            };
            let (dict_id, comp) = self.compress_tier(&data, true);
            let mut body = Vec::with_capacity(30 + comp.len());
            body.push(4u8);
            body.push(dict_id);
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
            body.extend_from_slice(&(h as u64).to_le_bytes());
            body.extend_from_slice(&loc.raw_len.to_le_bytes());
            body.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            body.extend_from_slice(&comp);
            let Ok((seg, off, _)) = self.append(&body) else { continue };
            let mut blobs = self.blobs.write();
            if let Some(l) = blobs.get_mut(&h) {
                if let Some(old) = self.segs.read().get(&worst_id) {
                    old.live.fetch_sub(l.body_len(), Ordering::Relaxed);
                }
                l.seg = seg;
                l.off = off;
                l.comp_len = comp.len() as u32;
                l.dict_id = dict_id;
                l.slab = false;
                if l.id == NO_ID {
                    l.id = id;
                    self.ids.write().insert(id, h);
                }
            }
        }
        self.flush_slab_cands(&mut cands, worst_id);
        // move live recipes byte-exact
        let recs: Vec<(u64, u32, u64, u32)> = {
            let chunks = self.chunks.read();
            chunks
                .iter()
                .filter(|(_, (s, _, _))| *s == worst_id)
                .map(|(k, (s, o, l))| (*k, *s, *o, *l))
                .collect()
        };
        for (key, _s, off, len) in recs {
            let segs = self.segs.read();
            let Some(seg) = segs.get(&worst_id) else { continue };
            let Some(body) = seg.read_at(off, len as usize, false) else { continue };
            drop(segs);
            if body.first() != Some(&2) {
                continue;
            }
            let Ok((nseg, noff, nlen)) = self.append(&body) else { continue };
            if let Some(old) = self.segs.read().get(&worst_id) {
                old.live.fetch_sub(8 + len as u64, Ordering::Relaxed);
            }
            self.chunks.write().insert(key, (nseg, noff, nlen));
        }
        // preserve interned keys recorded in this segment (kind-6 records
        // are not referenced by recipes but the key table must survive)
        self.preserve_keys(worst_id);
        // drop the segment
        let old = self.segs.write().remove(&worst_id);
        drop(old);
        let _ = fs::remove_file(self.seg_path(worst_id));
        self.gc_dicts();
        eprintln!("[c-saves] compacted seg-{worst_id:05} ({:.0}% dead, cold-tier recompressed)", ratio * 100.0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;

    fn key(cx: i32, cz: i32) -> u64 {
        ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "csaves-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn bench_cfg() -> Cfg {
        Cfg {
            compact_min: 512 * 1024,
            compact_ratio: 0.30,
            dict_train: 40,
            ..Cfg::default()
        }
    }

    /// Semi-realistic chunk: 24 sections, some shared via `variant`,
    /// palette + packed longs, heightmaps, block entities.
    fn chunk_payload(cx: i32, cz: i32, variant: u64) -> Vec<u8> {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert(
            "Position".to_string(),
            Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cz)]),
        );
        let mut hm: Map<String, Value> = Map::new();
        let la: Vec<i64> = (0..37).map(|i| (i as i64) * 7 + variant as i64).collect();
        hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(fastnbt::LongArray::new(la)));
        root.insert("Heightmaps".to_string(), Value::Compound(hm));
        let mut sec_list = Vec::new();
        for y in 0..24i32 {
            let shared = (y % 4) != 0; // 3 of 4 sections identical for same variant
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(y as i8));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = Vec::new();
            pal.push(Value::String("minecraft:stone".to_string()));
            pal.push(Value::String("minecraft:deepslate".to_string()));
            pal.push(Value::String("minecraft:air".to_string()));
            bs.insert("palette".to_string(), Value::List(pal));
            let data: Vec<i64> = (0..64)
                .map(|i| {
                    if shared {
                        (i as i64).wrapping_mul(0x1234_5678_9abc_def0u64 as i64) ^ (variant as i64)
                    } else {
                        (i as i64).wrapping_mul(0x1234_9abc_def0_5678i64) ^ (cx as i64) ^ (cz as i64) ^ (y as i64)
                    }
                })
                .collect();
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        let mut be = Vec::new();
        let mut chest: Map<String, Value> = Map::new();
        chest.insert("id".to_string(), Value::String("minecraft:chest".to_string()));
        chest.insert("x".to_string(), Value::Int(cx * 16 + 5));
        chest.insert("y".to_string(), Value::Int(64));
        chest.insert("z".to_string(), Value::Int(cz * 16 + 5));
        be.push(Value::Compound(chest));
        root.insert("block_entities".to_string(), Value::List(be));
        fastnbt::to_bytes(&Value::Compound(root)).unwrap()
    }

    fn semantic(a: &[u8], b: &[u8]) -> bool {
        let va: Value = fastnbt::from_bytes(a).unwrap();
        let vb: Value = fastnbt::from_bytes(b).unwrap();
        va == vb
    }

    #[test]
    fn write_read_persist_reopen() {
        let dir = temp_dir("persist");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let n = 60;
        let mut payloads = Vec::new();
        for i in 0..n {
            let p = chunk_payload(i - 30, (i * 7) - 30, (i % 5) as u64);
            payloads.push(p.clone());
            st.write_chunk(key(i - 30, (i * 7) - 30), &p, false).unwrap();
        }
        for i in 0..n {
            let got = st.read_chunk(key(i - 30, (i * 7) - 30)).expect("read");
            assert!(semantic(&got, &payloads[i as usize]), "chunk {i} mismatch");
        }
        st.sync();
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st2.chunk_count(), n as usize);
        for i in 0..n {
            let got = st2.read_chunk(key(i - 30, (i * 7) - 30)).expect("read after reopen");
            assert!(semantic(&got, &payloads[i as usize]), "chunk {i} mismatch after reopen");
        }
        let logical: u64 = payloads.iter().map(|p| p.len() as u64).sum();
        let disk = st2.disk_bytes();
        eprintln!("persist: logical {:.1} KB, disk {:.1} KB (ratio {:.2})",
            logical as f64 / 1024.0, disk as f64 / 1024.0, disk as f64 / logical as f64);
        assert!(disk < logical, "container must be smaller than logical payload");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_tombstone_compact() {
        let dir = temp_dir("compact");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let n = 30;
        for round in 0..7u64 {
            for i in 0..n {
                let p = chunk_payload(i - 15, i - 15, round * 100 + i as u64);
                st.write_chunk(key(i - 15, i - 15), &p, false).unwrap();
            }
        }
        assert_eq!(st.chunk_count(), n as usize);
        st.sync();
        let mut compacted = false;
        for _ in 0..40 {
            if st.compact_step() {
                compacted = true;
            } else {
                break;
            }
        }
        eprintln!("compact: ran={compacted} disk={:.1} KB", st.disk_bytes() as f64 / 1024.0);
        for i in 0..n {
            let p = chunk_payload(i - 15, i - 15, 6 * 100 + i as u64);
            let got = st.read_chunk(key(i - 15, i - 15)).expect("read post-compact");
            assert!(semantic(&got, &p), "chunk {i} mismatch post-compact");
        }
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st2.chunk_count(), n as usize);
        for i in 0..n {
            let p = chunk_payload(i - 15, i - 15, 6 * 100 + i as u64);
            let got = st2.read_chunk(key(i - 15, i - 15)).expect("read post-compact reopen");
            assert!(semantic(&got, &p), "chunk {i} mismatch post-compact reopen");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_and_dedup() {
        let dir = temp_dir("dedup");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let p = chunk_payload(1, 1, 9);
        for i in 0..50 {
            st.write_chunk(key(i, 0), &p, true).unwrap();
        }
        assert_eq!(st.chunk_count(), 50);
        let disk_all = st.disk_bytes();
        assert!(disk_all < p.len() as u64 * 25, "identical whole payloads must dedup");
        for i in 0..25 {
            st.delete_chunk(key(i, 0)).unwrap();
        }
        assert_eq!(st.chunk_count(), 25);
        assert!(st.read_chunk(key(30, 0)).is_some());
        assert!(st.read_chunk(key(0, 0)).is_none());
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st2.chunk_count(), 25);
        assert!(st2.read_chunk(key(30, 0)).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recipes_use_id_encoding() {
        let dir = temp_dir("ids");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let p = chunk_payload(3, 4, 1);
        st.write_chunk(key(3, 4), &p, false).unwrap();
        let p2 = chunk_payload(3, 5, 1);
        st.write_chunk(key(3, 5), &p2, false).unwrap();
        // verify id-encoded recipes decode
        let got = st.read_chunk(key(3, 4)).unwrap();
        assert!(semantic(&got, &p));
        let got2 = st.read_chunk(key(3, 5)).unwrap();
        assert!(semantic(&got2, &p2));
        // both chunks share most sections: dedup hits expected
        assert!(st.dedup_hits.load(Ordering::Relaxed) > 0, "sections must dedup");
        // blobs got ids
        assert!(!st.ids.read().is_empty(), "blob ids must be assigned");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_blob_roundtrip_and_dedup() {
        let dir = temp_dir("tail");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        // big heightmap tail (> tail_blob_min compressed) shared across chunks
        let mut base: Map<String, Value> = Map::new();
        base.insert("DataVersion".to_string(), Value::Int(4189));
        let mut hm: Map<String, Value> = Map::new();
        let la: Vec<i64> = (0..37).map(|i| 0x1DEAD_0000i64 + i as i64 * 911).collect();
        hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(fastnbt::LongArray::new(la.clone())));
        base.insert("Heightmaps".to_string(), Value::Compound(hm.clone()));
        let mut sec: Map<String, Value> = Map::new();
        sec.insert("Y".to_string(), Value::Byte(0));
        let mut bs: Map<String, Value> = Map::new();
        bs.insert("palette".to_string(), Value::List(vec![Value::String("minecraft:stone".into())]));
        sec.insert("block_states".to_string(), Value::Compound(bs));
        base.insert("sections".to_string(), Value::List(vec![Value::Compound(sec.clone())]));
        base.insert(
            "block_entities".to_string(),
            Value::List((0..40).map(|i| {
                let mut be: Map<String, Value> = Map::new();
                be.insert("id".to_string(), Value::String(format!("minecraft:sign_{i}")));
                be.insert("text".to_string(), Value::String("hello world this is a long sign text".into()));
                Value::Compound(be)
            }).collect()),
        );
        let payload = fastnbt::to_bytes(&Value::Compound(base)).unwrap();
        for i in 0..8 {
            st.write_chunk(key(i, 100), &payload, false).unwrap();
        }
        for i in 0..8 {
            let got = st.read_chunk(key(i, 100)).unwrap();
            assert!(semantic(&got, &payload));
        }
        // identical tails dedup: tail blobs shared, container stays small
        let disk = st.disk_bytes();
        assert!(disk < payload.len() as u64 * 3, "identical tails must dedup: disk {disk} vs payload {}", payload.len());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slab_batching_roundtrip_and_reopen() {
        let dir = temp_dir("slab");
        let cfg = Cfg { compact_min: 16 * 1024, compact_ratio: 0.10, dict_train: 400, ..bench_cfg() };
        let st = Store::open_cfg(&dir, true, cfg).unwrap();
        // many small whole-mode payloads (entity-chunk-like)
        let payload = |i: u64| -> Vec<u8> {
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            root.insert("Position".to_string(), Value::List(vec![
                Value::Int(i as i32), Value::Int(0), Value::Int((i % 97) as i32),
            ]));
            root.insert("Pad".to_string(), Value::String("e".repeat((i % 40 + 120) as usize)));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };
        let n = 400u64;
        for i in 0..n {
            st.write_chunk(key(i as i32, 7), &payload(i), true).unwrap();
        }
        // delete 40% -> dead bytes -> compaction rewrites the rest into slabs
        for i in 0..n {
            if i % 5 < 2 {
                st.delete_chunk(key(i as i32, 7)).unwrap();
            }
        }
        st.sync();
        st.seal();
        let mut compacted = false;
        for _ in 0..50 {
            if st.compact_step() { compacted = true; } else { break; }
        }
        assert!(compacted, "compaction must run");
        // all survivors readable
        let mut live = 0;
        for i in 0..n {
            if i % 5 < 2 { continue; }
            let got = st.read_chunk(key(i as i32, 7)).expect("slab member readable");
            let vb = payload(i);
            let okv = match (fastnbt::from_bytes::<Value>(&got), fastnbt::from_bytes::<Value>(&vb)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if !okv {
                eprintln!("BAD member {i}: state={}", st.debug_blob_state(key(i as i32, 7)));
                panic!("slab member {i} mismatch");
            }
            live += 1;
        }
        assert_eq!(live, (n * 3 / 5) as usize);
        drop(st);
        let st2 = Store::open_cfg(&dir, true, Cfg { compact_min: 16 * 1024, ..bench_cfg() }).unwrap();
        assert_eq!(st2.chunk_count(), (n * 3 / 5) as usize);
        for i in 0..n {
            if i % 5 < 2 { continue; }
            let got = st2.read_chunk(key(i as i32, 7)).expect("slab member readable after reopen");
            let va: Value = fastnbt::from_bytes(&got).unwrap();
            let vb: Value = fastnbt::from_bytes(&payload(i)).unwrap();
            assert_eq!(va, vb, "slab member {i} mismatch after reopen");
        }
        eprintln!("slab: disk {:.1} KB for {live} chunks", st2.disk_bytes() as f64 / 1024.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_recipe_modes_still_parse() {
        // mode 0: whole by hash
        let mut id2h = HashMap::new();
        let r0 = Recipe::parse(
            &{
                let mut v = vec![0u8];
                v.extend_from_slice(&1u64.to_le_bytes());
                v.extend_from_slice(&2u64.to_le_bytes());
                v
            },
            &id2h,
            &[],
        )
        .unwrap();
        assert_eq!(r0.whole, Some(((1u128) << 64) | 2));
        // mode 2 with ids resolved
        id2h.insert(7u32, 0xABu128);
        let mut v = vec![2u8, 1u8];
        put_u64(&mut v, zigzag(7i64));
        v.push(0u8); // tail none
        let r2 = Recipe::parse(&v, &id2h, &[]).unwrap();
        assert_eq!(r2.sections, vec![0xABu128]);
        // mode 2 with delta encoding: id 7 then id 9
        id2h.insert(9u32, 0xBCu128);
        let mut v = vec![2u8, 2u8];
        put_u64(&mut v, zigzag(7i64));
        put_u64(&mut v, zigzag(2i64));
        v.push(0u8);
        let r2 = Recipe::parse(&v, &id2h, &[]).unwrap();
        assert_eq!(r2.sections.len(), 2);
        // mode 3: whole by id
        let mut v = vec![3u8];
        put_u64(&mut v, 7);
        let r3 = Recipe::parse(&v, &id2h, &[]).unwrap();
        assert_eq!(r3.whole, Some(0xABu128));
    }
}

#[cfg(test)]
mod repro {
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;

    fn key(cx: i32, cz: i32) -> u64 {
        ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
    }

    /// Whole-mode payloads (entity chunks) with unique positions + rewrites.
    #[test]
    fn whole_mode_rewrite_readback() {
        let dir = std::env::temp_dir().join(format!(
            "csaves-repro-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Cfg { compact_min: 256 * 1024, compact_ratio: 0.10, dict_train: 400, ..Cfg::default() };
        let st = Store::open_cfg(&dir, true, cfg).unwrap();

        let payload = |i: u64, r: u64| -> Vec<u8> {
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            root.insert("Position".to_string(), Value::List(vec![
                Value::Int(i as i32), Value::Int(0), Value::Int((i * 7 % 1000) as i32),
            ]));
            root.insert("Round".to_string(), Value::Int(r as i32));
            root.insert("Pad".to_string(), Value::String("x".repeat((i % 50 + 100) as usize)));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };

        let n = 300u64;
        for i in 0..n {
            st.write_chunk(key(i as i32 - 150, (i as i32) * 3 - 150), &payload(i, 0), true).unwrap();
        }
        // rewrite 12.5%
        let mut rewritten: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for r in 1..=3u64 {
            for i in 0..n {
                if (i * 13 + r * 5) % 8 == 0 {
                    let k = key(i as i32 - 150, (i as i32) * 3 - 150);
                    st.write_chunk(k, &payload(i, r), true).unwrap();
                    rewritten.insert(k, r);
                }
            }
        }
        st.sync();
        st.seal();
        st.train_dict_if_needed();
        // read all back
        let mut bad = 0;
        for i in 0..n {
            let k = key(i as i32 - 150, (i as i32) * 3 - 150);
            let r = rewritten.get(&k).copied().unwrap_or(0);
            match st.read_chunk(k) {
                Some(got) => {
                    let want = payload(i, r);
                    let va: Value = fastnbt::from_bytes(&got).unwrap();
                    let vb: Value = fastnbt::from_bytes(&want).unwrap();
                    assert_eq!(va, vb, "chunk {i} content mismatch");
                }
                None => {
                    eprintln!("REPRO: chunk {i} key {k:016x} unreadable; stats={}", st.stats_json());
                    bad += 1;
                }
            }
        }
        assert_eq!(bad, 0, "{bad} unreadable chunks");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
