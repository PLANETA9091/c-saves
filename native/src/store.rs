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
//! 112KB retrained shared dictionaries, hot tier (level 3 — measured 2.1x
//! faster than 12 with identical packed size) on the write path, cold tier
//! (level 19) recompression during background compaction.

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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

const SEG_ROLL: u64 = 64 * 1024 * 1024;
const BUF_CAP: usize = 1 << 20;
const MAX_SECTIONS: usize = 255;
const NO_ID: u32 = u32::MAX;
/// chunks-map len marker: recipe lives inside a compressed kind-9 batch;
/// the real (rec_off, stream_off, len) is in `batch_idx`.
const BATCHED: u32 = u32::MAX;

// ------------------------------------------------------------- config -----

#[derive(Clone, Debug)]
pub struct Cfg {
    /// zstd level on the write path (hot tier).
    pub hot_level: i32,
    /// zstd level for background recompression (cold tier).
    pub cold_level: i32,
    /// zstd level for recipe-batch flushes (write-path critical section;
    /// measured on the JJ corpus: z19 compression of the 64-192 KiB batch
    /// stream ate ~49% of fresh-import wall time in sync flush pauses,
    /// while z6 costs only a few % more container bytes — the pack pass
    /// re-tiers the underlying section/BE bytes anyway).
    pub recipe_level: i32,
    /// Min segment size before compaction considers it.
    pub compact_min: u64,
    /// Slab member cap for whole-mode stores (entity/POI payloads).
    /// Real JJThunder entity chunks average ~40 KiB, so the cap must admit
    /// whole entity chunks as slab members: batched 4 MiB slabs reach the
    /// concatenated z19 floor (x24 vs per-blob x9).
    pub slab_max_member_whole: u32,
    /// Dead-byte ratio that triggers compaction.
    pub compact_ratio: f64,
    /// Hot-tier bytes accumulated since the last pack before an auto-pack
    /// is requested (keeps fresh data slab-packed + cold-tiered without
    /// relying on dead bytes appearing).
    pub auto_pack_bytes: u64,
    /// Shared dictionary size cap.
    pub dict_max: usize,
    /// Samples collected before (re)training the dictionary.
    pub dict_train: usize,
    /// Tails with compressed size >= this become dedup blobs.
    pub tail_blob_min: usize,
    /// Blobs with raw size <= this become slab members at compaction.
    pub slab_max_member: u32,
    /// Target decompressed size of one slab record. Bigger frames = longer
    /// zstd windows; 4 MiB section slabs roughly halve window-boundary losses
    /// vs 2 MiB at unchanged cache budget (8 hot slabs in 32 MiB).
    pub slab_max_raw: usize,
    /// Target decompressed size of one whole-mode (entity/POI) slab record.
    /// Bigger frames = longer zstd windows = closer to the concatenated
    /// floor for highly repetitive entity corpora.
    pub slab_max_raw_whole: usize,
    /// Decompressed-slab read cache budget (bytes).
    pub slab_cache: u64,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            // hot tier = write path. Level 3 measured 2.1x faster than 12 on
            // the real JJThunder corpus (import 34.8 vs 16.8 MB/s, write
            // p99.9 10ms vs 30ms) with IDENTICAL steady-state size — the
            // background pack pass recompresses everything at the cold tier
            // anyway, so the hot level only bounds transient disk usage.
            hot_level: 3,
            cold_level: 19,
            recipe_level: 6,
            compact_min: 64 * 1024,
            compact_ratio: 0.25,
            auto_pack_bytes: 4 * 1024 * 1024,
            slab_max_member_whole: 1024 * 1024,
            dict_max: 112 * 1024,
            dict_train: 200,
            tail_blob_min: 512,
            slab_max_member: 64 * 1024,
            slab_max_raw: 8 * 1024 * 1024,
            slab_max_raw_whole: 4 * 1024 * 1024,
            slab_cache: 32 * 1024 * 1024,
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, dflt: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

/// Exclusive flock guarding one store directory against a second live
/// opener in ANOTHER process (concurrent scan+truncate from a second
/// process destroys the container). Re-entrant within this process via a
/// global registry: reopening the same dir shares the held flock.
fn take_flock(dir: &Path) -> std::io::Result<Option<File>> {
    use std::collections::HashSet;
    use std::os::unix::io::AsRawFd;
    use std::sync::{Arc, Mutex, OnceLock};
    static REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    static HELD: OnceLock<Mutex<Vec<Arc<File>>>> = OnceLock::new();
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut reg = REGISTRY
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if !reg.insert(key.clone()) {
        // already locked by this process
        return Ok(None);
    }
    let path = key.join("lock");
    let f = OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
    const LOCK_EX: i32 = 2;
    let rc = unsafe { libc_flock(f.as_raw_fd(), LOCK_EX) };
    if rc != 0 {
        reg.remove(&key);
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "c-saves: store already opened by another process",
            ));
        }
        return Err(err);
    }
    // keep the fd alive for the process lifetime (flock holds until exit)
    HELD
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Arc::new(f));
    Ok(None)
}

extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, op: i32) -> i32;
}

impl Cfg {
    pub fn from_env() -> Cfg {
        Cfg {
            hot_level: env_parse("CSAVES_HOT_LEVEL", 3),
            cold_level: env_parse("CSAVES_COLD_LEVEL", 19),
            recipe_level: env_parse("CSAVES_RECIPE_LEVEL", 6),
            compact_min: env_parse("CSAVES_COMPACT_MIN_BYTES", 64 * 1024),
            compact_ratio: env_parse("CSAVES_COMPACT_DEAD_RATIO", 0.25),
            auto_pack_bytes: env_parse("CSAVES_AUTO_PACK_BYTES", 4 * 1024 * 1024),
            slab_max_member_whole: env_parse("CSAVES_SLAB_MAX_MEMBER_WHOLE", 1024 * 1024),
            dict_max: env_parse("CSAVES_DICT_MAX_BYTES", 112 * 1024),
            dict_train: env_parse("CSAVES_DICT_TRAIN_BLOBS", 200),
            tail_blob_min: env_parse("CSAVES_TAIL_BLOB_MIN", 512),
            slab_max_member: env_parse("CSAVES_SLAB_MAX_MEMBER", 64 * 1024),
            slab_max_raw: env_parse("CSAVES_SLAB_MAX_RAW", 8 * 1024 * 1024),
            slab_max_raw_whole: env_parse("CSAVES_SLAB_MAX_RAW_WHOLE", 4 * 1024 * 1024),
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
    /// true when the record was written at the cold tier (background pack);
    /// hot-tier records are pack candidates (recompress + slab).
    cold: bool,
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
    /// Lazily-mapped read view for SEALED segments. OnceLock: lock-free after
    /// the first successful map (one Mutex acquire per blob read was measured
    /// as real contention under Moonrise's parallel IO workers).
    mmap: std::sync::OnceLock<Option<Arc<Mmap>>>,
    total: AtomicU64,
    live: AtomicU64,
}

impl Seg {
    fn read_at(&self, off: u64, len: usize, sealed: bool) -> Option<Vec<u8>> {
        if sealed {
            let m = self.mmap.get_or_init(|| unsafe { Mmap::map(&self.file) }.ok().map(Arc::new));
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
                    match id2h.get(&(prev as u32)) {
                        Some(h) => sections.push(*h),
                        None => return None,
                    }
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
            6 => {
                // mode 6: run-length over the mode-2 delta-id stream —
                // (zigzag delta, run count) pairs. Extreme-height worlds have
                // long Y-runs of the same uniform section hash, so ~131 refs
                // collapse to a dozen pairs.
                let n = *b.get(p)? as usize;
                p += 1;
                let mut sections = Vec::with_capacity(n.min(255));
                let mut prev: i64 = 0;
                let mut total = 0usize;
                while total < n {
                    let d = unzigzag(get_u64(b, &mut p)?);
                    let run = get_u64(b, &mut p)? as usize;
                    if run == 0 || total + run > n {
                        return None;
                    }
                    prev += d;
                    let h = match id2h.get(&(prev as u32)) {
                        Some(h) => *h,
                        None => return None,
                    };
                    for _ in 0..run {
                        sections.push(h);
                    }
                    total += run;
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
                // mode 2: zigzag-delta id per section
                let mut m2 = Vec::with_capacity(2 + ids.len() * 2 + 16);
                m2.push(2u8);
                m2.push(self.sections.len() as u8);
                let mut prev: i64 = 0;
                for id in &ids {
                    put_u64(&mut m2, zigzag(*id as i64 - prev));
                    prev = *id as i64;
                }
                push_tail(&mut m2, tail, &id_of);
                // mode 6: run-length encode the same stream (repeated id =>
                // (delta, run) pairs). Smaller wins, deterministic.
                let mut m6 = Vec::with_capacity(2 + ids.len() / 2 + 16);
                m6.push(6u8);
                m6.push(self.sections.len() as u8);
                let mut prev: i64 = 0;
                let mut i = 0usize;
                while i < ids.len() {
                    let d = ids[i] as i64 - prev;
                    let mut run = 1usize;
                    while i + run < ids.len() && ids[i + run] == ids[i] {
                        run += 1;
                    }
                    put_u64(&mut m6, zigzag(d));
                    put_u64(&mut m6, run as u64);
                    prev = ids[i] as i64;
                    i += run;
                }
                push_tail(&mut m6, tail, &id_of);
                if m6.len() < m2.len() {
                    v.extend_from_slice(&m6);
                } else {
                    v.extend_from_slice(&m2);
                }
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

// ------------------------------------------------------- recipe batch -----

/// Pending chunk recipes queued as one kind-9 batch record on flush.
/// The O(1) index + running byte total replace the per-write linear scans
/// that made the save path quadratic under autosave storms.
struct RecipeBatch {
    entries: Vec<(u64, Vec<u8>)>,
    index: HashMap<u64, usize>,
    bytes: usize,
}

impl Default for RecipeBatch {
    fn default() -> Self {
        RecipeBatch { entries: Vec::new(), index: HashMap::new(), bytes: 0 }
    }
}

impl RecipeBatch {
    /// Queue a recipe; a pending entry for the same key is superseded
    /// (last write wins — matches the chunks-map behavior at flush).
    fn push(&mut self, key: u64, bare: Vec<u8>) {
        if self.index.contains_key(&key) {
            self.remove(key);
        }
        self.index.insert(key, self.entries.len());
        self.bytes += 10 + bare.len();
        self.entries.push((key, bare));
    }
    /// Remove one pending entry (swap_remove; the moved entry's index slot
    /// is fixed up). Returns the removed recipe body.
    fn remove(&mut self, key: u64) -> Option<Vec<u8>> {
        let i = *self.index.get(&key)?;
        self.index.remove(&key);
        self.bytes -= 10 + self.entries[i].1.len();
        let last = self.entries.len() - 1;
        let (_, bare) = self.entries.swap_remove(i);
        if i != last {
            let moved = self.entries[i].0;
            self.index.insert(moved, i);
        }
        Some(bare)
    }
    fn get(&self, key: u64) -> Option<&Vec<u8>> {
        self.entries.get(*self.index.get(&key)?).map(|(_, b)| b)
    }
}

// ----------------------------------------------------- stage timers -------

/// Read-path stage timers (nanos + call count). Always on: two relaxed
/// atomic adds per stage are noise next to the IO work itself, and this is
/// the CI "stall point" recorder (dump via read_stage_json).
pub static ST_RECIPE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
/// Unchanged-chunk autosave skips (hash match against the last write).
pub static ST_WRITE_SKIP: AtomicU64 = AtomicU64::new(0);
pub static ST_TAIL: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_SECTLOAD: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_JOIN_FUSED: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_JOIN_LEGACY: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_PARSE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
// write path
pub static WST_SPLIT: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static WST_ENS: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static WST_RECIPE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));

/// Human/CI-readable stage snapshot: ns/call per stage.
pub fn read_stage_json() -> String {
    let fmt = |name: &str, st: &(AtomicU64, AtomicU64)| {
        let (ns, n) = (st.0.load(Ordering::Relaxed), st.1.load(Ordering::Relaxed));
        format!("\"{}\":{{ns:{},n:{},per_call:{}}}", name, ns, n, if n > 0 { ns / n } else { 0 })
    };
    format!(
        "{{{},{},{},{},{},{}}}",
        fmt("recipe", &ST_RECIPE),
        fmt("tail", &ST_TAIL),
        fmt("sectload", &ST_SECTLOAD),
        fmt("join_fused", &ST_JOIN_FUSED),
        fmt("join_legacy", &ST_JOIN_LEGACY),
        fmt("fastnbt_parse", &ST_PARSE),
    )
}

pub fn write_stage_json() -> String {
    let fmt = |name: &str, st: &(AtomicU64, AtomicU64)| {
        let (ns, n) = (st.0.load(Ordering::Relaxed), st.1.load(Ordering::Relaxed));
        format!("\"{}\":{{ns:{},n:{},per_call:{}}}", name, ns, n, if n > 0 { ns / n } else { 0 })
    };
    format!(
        "{{{},{},{}}}",
        fmt("split_parse_encode", &WST_SPLIT),
        fmt("ensure_blobs", &WST_ENS),
        fmt("recipe_batch", &WST_RECIPE),
    )
}

pub fn read_stage_reset() {
    for st in [&ST_RECIPE, &ST_TAIL, &ST_SECTLOAD, &ST_JOIN_FUSED, &ST_JOIN_LEGACY, &ST_PARSE] {
        st.0.store(0, Ordering::Relaxed);
        st.1.store(0, Ordering::Relaxed);
    }
}

// --------------------------------------------------------------- store ----

/// Result of a batched section load: owned decompressed buffers plus per-
/// section (buffer index, start, end) references into them.
struct LoadedSections {
    keep: Vec<Arc<Vec<u8>>>,
    refs: Vec<(usize, u32, u32)>,
}

pub struct Store {
    uid: u64,
    /// Exclusive flock on `<dir>/lock` — refuses a second live opener of the
    /// same store (concurrent scan+truncate from a second process destroys
    /// the container).
    _lock: Option<File>,
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
    /// Pending chunk recipes batched into one kind-7 record (saves the
    /// 8-byte record frame + 2 header bytes per chunk). Read-your-writes is
    /// served from the pending map until the batch is flushed.
    recipe_batch: Mutex<RecipeBatch>,
    /// Index into COMPRESSED kind-9 recipe batches. The chunks map stores
    /// len=BATCHED for these keys; (record body off, stream off, entry len)
    /// lives here. Rebuilt by scan on reopen.
    batch_idx: Mutex<HashMap<u64, (u64, u32, u32)>>,
    writer: Mutex<Option<Writer>>,
    next_seg: AtomicU64,
    next_blob_id: AtomicU64,
    dicts: RwLock<HashMap<u8, Arc<Vec<u8>>>>,
    slab_cache: Mutex<SlabCache>,
    cur_dict: AtomicU8,
    next_dict_ver: AtomicU8,
    dict_samples: Mutex<Vec<Vec<u8>>>,
    dict_enabled: AtomicBool,

    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub dedup_hits: AtomicU64,
    pub disk_written: AtomicU64,
    pub logical_bytes: AtomicU64,
    /// Whole-mode stores (entities/POI) hold raw NBT payloads whose repeated
    /// strings benefit from the shared dictionary in slab streams; section
    /// stores hold canonical CSEC bytes where the dict only dilutes matches.
    whole_mode: AtomicBool,
    /// Set when a pack (slab + cold tier) pass was explicitly requested
    /// (e.g. right after an Anvil import).
    pack_requested: AtomicBool,
    /// disk_bytes() watermark at the last completed pack pass.
    pack_watermark: AtomicU64,
    /// Content hash of the last successful write_chunk per key: Paper
    /// autosaves re-serialize UNCHANGED chunks every cycle; Anvil pays a
    /// full inflate+deflate for each, we return O(1) when the payload hash
    /// matches. Bounded by unique keys ever written (~24 B/chunk).
    last_written: Mutex<HashMap<u64, Hash>>,
    /// Parsed blob-hash sets per batched recipe location, for the
    /// chunk_intact probe: decompressing a 192 KiB batch record per skip
    /// check thrashes the 32 MiB slab cache on large worlds (each miss is a
    /// full z6 decompress); the parsed hash set turns the probe into map
    /// lookups. Cap-cleared (entries are content-derived: blob ids are
    /// monotonic and segments append-only, so a parsed set never goes stale
    /// short of data loss — which the membership check itself detects).
    probe_cache: Mutex<HashMap<(u32, u64, u32, u32), Arc<Vec<Hash>>>>,
}

impl Store {
    pub fn open(dir: &Path, dict_enabled: bool) -> std::io::Result<Arc<Store>> {
        Self::open_cfg(dir, dict_enabled, Cfg::from_env())
    }

    pub fn open_cfg(dir: &Path, dict_enabled: bool, cfg: Cfg) -> std::io::Result<Arc<Store>> {
        fs::create_dir_all(dir)?;
        static NEXT_UID: AtomicU64 = AtomicU64::new(1);
        let lock = take_flock(dir)?;
        let st = Arc::new(Store {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            _lock: lock,
            dir: dir.to_path_buf(),
            cfg,
            segs: RwLock::new(HashMap::new()),
            blobs: RwLock::new(HashMap::new()),
            ids: RwLock::new(HashMap::new()),
            chunks: RwLock::new(HashMap::new()),
            keys_tab: RwLock::new(Vec::new()),
            key_refcnt: RwLock::new(HashMap::new()),
            recipe_batch: Mutex::new(RecipeBatch::default()),
            batch_idx: Mutex::new(HashMap::new()),
            writer: Mutex::new(None),
            slab_cache: Mutex::new(SlabCache::new(32 * 1024 * 1024)),
            next_seg: AtomicU64::new(0),
            next_blob_id: AtomicU64::new(1),
            dicts: RwLock::new(HashMap::new()),
            cur_dict: AtomicU8::new(0),
            next_dict_ver: AtomicU8::new(1),
            dict_samples: Mutex::new(Vec::new()),
            dict_enabled: AtomicBool::new(dict_enabled),
            writes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
            disk_written: AtomicU64::new(0),
            logical_bytes: AtomicU64::new(0),
            whole_mode: AtomicBool::new(false),
            pack_requested: AtomicBool::new(false),
            pack_watermark: AtomicU64::new(0),
            last_written: Mutex::new(HashMap::new()),
            probe_cache: Mutex::new(HashMap::new()),
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
        if !self.dict_enabled.load(Ordering::Relaxed) {
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
        if !self.dict_enabled.load(Ordering::Relaxed) {
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
                mmap: std::sync::OnceLock::new(),
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
            for id in ids.clone() {
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
                                            cold: false,
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
                                            cold: false,
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
                            // SLAB v1: [dict][cl][raw_total][n][comp]; entries
                            // 24B fixed (id u32, len u32, hash 16B)
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
                                                    cold: true,
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
                        8 => {
                            // SLAB v2: same header layout; entries are varint
                            // zigzag id-delta + varint len + 16B hash, raws after.
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
                                    match self.parse_slab_v2(dict_id, &body[12..12 + cl], raw_total, n) {
                                        Some((entries, tab_len)) => {
                                            let mut idx = 0usize;
                                            let mut cursor = tab_len as u32;
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
                                                    cold: true,
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
                        7 => {
                            // legacy uncompressed batch: [7][u32 n]{[u64 key][u16 rl][0xFF][recipe]}
                            if body.len() < 5 {
                                false
                            } else {
                                let n = u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize;
                                let mut p = 5usize;
                                let base = off + 8;
                                let mut ok = true;
                                for _ in 0..n {
                                    if p + 10 > body.len() {
                                        ok = false;
                                        break;
                                    }
                                    let k = u64::from_le_bytes(body[p..p + 8].try_into().unwrap());
                                    p += 8;
                                    let rl =
                                        u16::from_le_bytes(body[p..p + 2].try_into().unwrap()) as usize;
                                    p += 2;
                                    if rl < 2 || p + rl > body.len() || body[p] != 0xFF {
                                        ok = false;
                                        break;
                                    }
                                    self.chunks.write().insert(k, (id, base + p as u64, rl as u32));
                                    p += rl;
                                }
                                ok
                            }
                        }
                        9 => {
                            // compressed batch: [9][u32 n][u32 cl][u32 raw]{stream}
                            // stream = [u64 key]×n ++ [u16 rl]×n ++ concat entries
                            if body.len() < 13 {
                                false
                            } else {
                                let n = u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize;
                                let cl = u32::from_le_bytes(body[5..9].try_into().unwrap()) as usize;
                                let raw_total = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
                                if body.len() < 13 + cl || n == 0 {
                                    false
                                } else {
                                    match self.decompress(0, &body[13..13 + cl], raw_total as u32) {
                                        Some(raw) if raw.len() == raw_total => {
                                            let mut ok = true;
                                            let mut keys = Vec::with_capacity(n);
                                            let mut lens = Vec::with_capacity(n);
                                            let mut p = 0usize;
                                            for _ in 0..n {
                                                if p + 8 > raw.len() {
                                                    ok = false;
                                                    break;
                                                }
                                                keys.push(u64::from_le_bytes(
                                                    raw[p..p + 8].try_into().unwrap(),
                                                ));
                                                p += 8;
                                            }
                                            if ok {
                                                for _ in 0..n {
                                                    if p + 2 > raw.len() {
                                                        ok = false;
                                                        break;
                                                    }
                                                    lens.push(u16::from_le_bytes(
                                                        raw[p..p + 2].try_into().unwrap(),
                                                    ) as usize);
                                                    p += 2;
                                                }
                                            }
                                            if ok {
                                                let base = off + 8;
                                                let mut bidx = self.batch_idx.lock();
                                                let mut ch = self.chunks.write();
                                                for (i, k) in keys.iter().enumerate() {
                                                    let rl = lens[i];
                                                    if rl < 2 || p + rl > raw.len() || raw[p] != 0xFF {
                                                        ok = false;
                                                        break;
                                                    }
                                                    ch.insert(*k, (id, base, BATCHED));
                                                    bidx.insert(*k, (base, p as u32, rl as u32));
                                                    p += rl;
                                                }
                                                if ok && p != raw.len() {
                                                    ok = false;
                                                }
                                            }
                                            ok
                                        }
                                        _ => false,
                                    }
                                }
                            }
                        }
                        3 => {
                            if body.len() < 9 {
                                false
                            } else {
                                let key = u64::from_le_bytes(body[1..9].try_into().unwrap());
                                self.chunks.write().remove(&key);
                                self.batch_idx.lock().remove(&key);
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
        // CRITICAL: resume the segment counter AFTER the highest existing id
        // — otherwise the first post-reopen append recreates seg-00000 and
        // overwrites the container from byte 0 (observed as total data loss
        // of every chunk written before the reopen).
        let max_seg = ids.iter().copied().max().unwrap_or(0);
        self.next_seg
            .store(max_seg as u64 + 1, Ordering::Relaxed);
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
        for (k, seg, off, len) in recipe_entries {
            // BATCHED recipes must resolve through the kind-9 batch index —
            // load_recipe reads `len` raw bytes at `off`, and BATCHED is a
            // sentinel, not a length. Resolving them with load_recipe
            // silently dropped EVERY batched recipe's refcounts at reopen
            // (refs rebuilt as 0), so the first full rewrite cycle
            // superseded recipes down to refs=0 and evicted live blobs that
            // other still-unrewritten recipes referenced (dangling ids,
            // unreadable chunks — exposed by the unchanged-write fast path,
            // masked before it because full rewrites re-created the blobs).
            let rec = if len == BATCHED {
                self.recipe_at(k, seg, off, len)
            } else {
                self.load_recipe(seg, off, len)
            };
            if let Some(rec) = rec {
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
        self.parse_recipe_body(&body)
    }

    /// Key-aware recipe load: resolves BATCHED pointers through the
    /// compressed kind-9 batch index.
    fn recipe_at(&self, key: u64, seg: u32, off: u64, len: u32) -> Option<Recipe> {
        if len != BATCHED {
            return self.load_recipe(seg, off, len);
        }
        let Some((boff, soff, rl)) = self.batch_idx.lock().get(&key).copied() else {
            return None;
        };
        if Some(seg) == self.writer_seg_id() {
            self.flush_writer();
        }
        // decompressed batch bodies live in the slab cache under a tagged key
        let ck = (seg, boff | (1u64 << 63));
        if let Some(cached) = self.slab_cache.lock().get(&ck) {
            let e = cached.get(soff as usize..(soff + rl) as usize)?;
            return self.parse_recipe_body(e);
        }
        let segs = self.segs.read();
        let s = segs.get(&seg)?;
        let sealed = Some(seg) != self.writer_seg_id();
        let hdr = s.read_at(boff, 13, sealed)?;
        if hdr.first() != Some(&9) {
            return None;
        }
        let cl = u32::from_le_bytes(hdr[5..9].try_into().ok()?) as usize;
        let raw_total = u32::from_le_bytes(hdr[9..13].try_into().ok()?) as usize;
        let data = s.read_at(boff + 13, cl, sealed)?;
        let raw = self.decompress(0, &data, raw_total as u32)?;
        let e = raw.get(soff as usize..(soff + rl) as usize)?;
        let out = self.parse_recipe_body(e);
        drop(segs);
        self.slab_cache.lock().put(ck, Arc::new(raw));
        out
    }

    /// Parse a recipe from an on-disk body: either a wrapped CHUNK record
    /// `[2][key][reclen][recipe]` or a bare batched sub-recipe `[0xFF][recipe]`.
    fn parse_recipe_body(&self, body: &[u8]) -> Option<Recipe> {
        let recipe_bytes: &[u8] = if body.first() == Some(&0xFF) {
            &body[1..]
        } else {
            if body.first() != Some(&2) || body.len() < 13 {
                return None;
            }
            let rl = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
            if body.len() < 13 + rl {
                return None;
            }
            &body[13..13 + rl]
        };
        let id2h = self.ids.read();
        let keytab = self.keys_tab.read();
        Recipe::parse(recipe_bytes, &id2h, &keytab)
    }

    /// Drain the pending recipe batch into one COMPRESSED kind-9 record:
    /// `[9][u32 n][u32 cl][u32 raw]{stream}` where stream =
    /// `[u64 key]×n ++ [u16 len]×n ++ concat([0xFF][recipe])]`, zstd-19'd as
    /// a whole (keys/len-tables are varint-dense and recipes share mode
    /// bytes/counts across entries — measured ~2x on the JJThunder corpus).
    /// Each entry is indexed in `chunks` with len=BATCHED; the true
    /// (rec_off, stream_off, len) sits in `batch_idx`.
    fn flush_recipe_batch(&self) {
        const BATCH_BYTES: usize = 192 * 1024;
        let entries: Vec<(u64, Vec<u8>)> = {
            let mut b = self.recipe_batch.lock();
            if b.entries.is_empty() {
                return;
            }
            let e = std::mem::take(&mut b.entries);
            b.index.clear();
            b.bytes = 0;
            e
        };
        // split into batch-sized groups (same 64 KiB accumulation as before)
        let mut groups: Vec<Vec<(u64, Vec<u8>)>> = Vec::new();
        let mut cur: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut cur_bytes = 0usize;
        for (k, bare) in entries {
            let need = 10 + bare.len();
            if !cur.is_empty() && cur_bytes + need > BATCH_BYTES {
                groups.push(std::mem::take(&mut cur));
                cur_bytes = 0;
            }
            cur_bytes += need;
            cur.push((k, bare));
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        for group in groups {
            let n = group.len();
            let mut stream = Vec::with_capacity(BATCH_BYTES + 4096);
            for (k, bare) in &group {
                stream.extend_from_slice(&k.to_le_bytes());
            }
            for (_, bare) in &group {
                stream.extend_from_slice(&(bare.len() as u16).to_le_bytes());
            }
            let mut starts = Vec::with_capacity(n);
            let mut so = (8 * n + 2 * n) as u32;
            for (_, bare) in &group {
                starts.push(so);
                stream.extend_from_slice(bare);
                so += bare.len() as u32;
            }
            let raw_total = stream.len() as u32;
            // z6: the recipe flush runs in the WRITE thread's critical path;
            // z19 there ate ~49% of fresh-import wall time in sync pauses
            // (measured via SPEED stages-import on the JJ corpus). Container
            // cost of z6 vs z19 on these streams is a few %; section/BE
            // payload bytes are re-tiered at the cold level by the pack pass.
            let (dict_id, comp) = self.compress_tier_level(&stream, self.cfg.recipe_level);
            debug_assert_eq!(dict_id, 0);
            let mut body = Vec::with_capacity(13 + comp.len());
            body.push(9u8);
            body.extend_from_slice(&(n as u32).to_le_bytes());
            body.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            body.extend_from_slice(&raw_total.to_le_bytes());
            body.extend_from_slice(&comp);
            let blen = body.len() as u64;
            let Ok((seg, off, _)) = self.append(&body) else {
                // put entries back so nothing is lost
                let mut b = self.recipe_batch.lock();
                for (k, bare) in group {
                    b.push(k, bare);
                }
                return;
            };
            // virtual live accounting: append added 8+blen; the per-entry
            // scheme accounts Σ(8+rl). Adjust by the difference.
            let virt: u64 = group.iter().map(|(_, b)| 8 + b.len() as u64).sum();
            {
                let mut chunks = self.chunks.write();
                let mut bidx = self.batch_idx.lock();
                for (i, (k, bare)) in group.iter().enumerate() {
                    chunks.insert(*k, (seg, off, BATCHED));
                    bidx.insert(*k, (off, starts[i], bare.len() as u32));
                }
            }
            if let Some(sg) = self.segs.read().get(&seg) {
                let phys = 8 + blen;
                if phys >= virt {
                    sg.live.fetch_sub(phys - virt, Ordering::Relaxed);
                } else {
                    sg.live.fetch_add(virt - phys, Ordering::Relaxed);
                }
            }
        }
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
    /// Compress without a shared dictionary. Slab streams (up to 192 KiB of
    /// similar section bytes) measure ~4% SMALLER without the dictionary:
    /// the zstd window already covers the whole slab, so the dict only adds
    /// header cost and dilutes match search.
    fn compress_tier_plain(&self, data: &[u8], cold: bool) -> (u8, Vec<u8>) {
        let level = if cold { self.cfg.cold_level } else { self.cfg.hot_level };
        match zstd::bulk::compress(data, level) {
            Ok(out) => (0, out),
            Err(_) => (0, data.to_vec()),
        }
    }

    /// Explicit-level variant (recipe-batch flush): same no-dictionary path,
    /// caller picks the zstd level.
    fn compress_tier_level(&self, data: &[u8], level: i32) -> (u8, Vec<u8>) {
        match zstd::bulk::compress(data, level) {
            Ok(out) => (0, out),
            Err(_) => (0, data.to_vec()),
        }
    }

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
        DEC_C.with(|c| {
            let mut slot = c.borrow_mut();
            let key = (self.uid, dict_id);
            if slot.as_ref().map(|s| s.key) != Some(key) {
                // rebuild ONLY on (store, dict) change: the per-call dicts
                // read-lock went away (was hit on every decompressed blob)
                let dec = if dict_id == 0 {
                    build_dec(None)
                } else {
                    let dict = self.dicts.read().get(&dict_id)?.clone();
                    build_dec(Some(&dict))
                }
                .ok()?;
                *slot = Some(DecCtx { key, dec });
            }
            slot.as_mut()?.dec.decompress(data, raw_len as usize).ok()
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
            slab: false, cold,
            share: 0, slab_off: 0, slab_raw: 0, slab_comp: 0,
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

    /// SLAB V2 entry table: n × [varint zigzag(id delta)][varint raw len]
    /// [16B hash], then concatenated member raws. Returns the members plus
    /// the byte length of the entry table (= offset of the first raw).
    fn parse_slab_v2(
        &self,
        dict_id: u8,
        comp: &[u8],
        raw_total: u32,
        n: usize,
    ) -> Option<(Vec<(u32, u32, Hash)>, usize)> {
        let raw = self.decompress(dict_id, comp, raw_total)?;
        let mut out = Vec::with_capacity(n);
        let mut p = 0usize;
        let mut prev: i64 = 0;
        for _ in 0..n {
            let d = unzigzag(get_u64(&raw, &mut p)?);
            let bid = (prev + d) as u32;
            prev = prev + d;
            let rl = get_u64(&raw, &mut p)? as u32;
            if raw.len() < p + 16 {
                return None;
            }
            let hi = u64::from_le_bytes(raw[p..p + 8].try_into().ok()?);
            let lo = u64::from_le_bytes(raw[p + 8..p + 16].try_into().ok()?);
            p += 16;
            out.push((bid, rl, ((hi as u128) << 64) | lo as u128));
        }
        let total_raw: u64 = out.iter().map(|(_, rl, _)| *rl as u64).sum();
        if raw.len() != p + total_raw as usize {
            return None;
        }
        Some((out, p))
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
        // Whole-mode payloads ABOVE the field-split threshold are stored as
        // one verbatim blob. Re-serializing through the NBT value model used
        // to sort compound keys alphabetically; vanilla writers emit optional
        // fields (Bukkit.*/Paper.*/Spigot.*) last, so their byte layouts keep
        // cross-entity prefix alignment and compress ~2.4x better (measured
        // on the real JJThunder corpus: x24.6 verbatim vs x10.3 re-serialized).
        // Field split only pays off for tiny payloads (POI stores dedup their
        // small structures across chunks).
        let hash = chunknbt::hash_of(payload);
        {
            // unchanged-payload fast path: Paper's autosave re-serializes
            // every loaded chunk each cycle even when nothing changed; when
            // the payload hash matches the last write, the full
            // parse/encode/append pipeline is redundant (Anvil still pays
            // inflate+deflate for the same bytes). The fast path is
            // SELF-HEALING: it re-reads the stored chunk (a decompress, not
            // a re-encode) and falls through to the full write if anything
            // is unreadable, so a lost/evicted backing blob is repaired
            // from the payload the server just handed us. CSAVES_NO_SKIP=1
            // disables (debug/bisect).
            if std::env::var("CSAVES_NO_SKIP").as_deref() != Ok("1")
                && self.last_written.lock().get(&key) == Some(&hash)
                && self.chunk_intact(key)
            {
                ST_WRITE_SKIP.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }
        let t0w = std::time::Instant::now();
        let recipe = if whole_mode && payload.len() > 1024 {
            self.ensure_blob(hash, payload, false)?;
            Recipe { whole: Some(hash), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false }
        } else {
            let split = chunknbt::split(payload, whole_mode, pos_hint)?;
            WST_SPLIT.0.fetch_add(t0w.elapsed().as_nanos() as u64, Ordering::Relaxed);
            WST_SPLIT.1.fetch_add(1, Ordering::Relaxed);
            let t1w = std::time::Instant::now();
            let r = match split {
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
                        self.ensure_blob(*h, bytes.as_slice(), false)?;
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
            WST_ENS.0.fetch_add(t1w.elapsed().as_nanos() as u64, Ordering::Relaxed);
            WST_ENS.1.fetch_add(1, Ordering::Relaxed);
            r
        };
        // recipe_batch stage = recipe encode + supersede + pending push ONLY
        // (was measured from t0w, i.e. cumulative over split+ensure — the
        // SPEED stages-import JSON used to misattribute ~49% of import wall
        // time to this stage; the real cost lives in split_parse_encode).
        let t2w = std::time::Instant::now();
        WST_RECIPE.1.fetch_add(1, Ordering::Relaxed);

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
        self.bump_recipe_refs(&recipe);

        // supersede refs: a pending batch entry first, then the chunks-map
        // entry (which is only dropped when it was not already superseded by
        // a pending entry for the same key)
        {
            let mut batch = self.recipe_batch.lock();
            if let Some(old_body) = batch.remove(key) {
                if let Some(old_rec) = self.parse_recipe_body(&old_body) {
                    self.drop_recipe_refs(&old_rec);
                }
            } else if let Some((oseg, ooff, olen)) = self.chunks.read().get(&key).copied() {
                if let Some(old_rec) = self.recipe_at(key, oseg, ooff, olen) {
                    self.drop_recipe_refs(&old_rec);
                }
                let rl = self.batched_len(key, olen);
                if let Some(s) = self.segs.read().get(&oseg) {
                    s.live.fetch_sub(8 + rl as u64, Ordering::Relaxed);
                }
                if olen == BATCHED {
                    self.batch_idx.lock().remove(&key);
                }
            }
        }

        // queue the bare recipe into the batch; flushed on sync/seal/compact
        // or when the batch grows past its size cap
        {
            let mut bare = Vec::with_capacity(1 + rec.len());
            bare.push(0xFFu8);
            bare.extend_from_slice(&rec);
            let mut batch = self.recipe_batch.lock();
            batch.push(key, bare);
            let total = batch.bytes;
            drop(batch);
            if total >= 192 * 1024 {
                self.flush_recipe_batch();
            }
        }
        WST_RECIPE.0.fetch_add(t2w.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.last_written.lock().insert(key, hash);
        Ok(())
    }

    /// Cheap integrity probe: does the chunk's recipe resolve AND do all
    /// blobs it references still exist? No decompress, no join — a few map
    /// lookups. Used by the unchanged-write fast path: catches recipes whose
    /// backing blobs were evicted (dangling ids) without paying a full read.
    pub fn chunk_intact(&self, key: u64) -> bool {
        enum Loc {
            Seg(u32, u64, u32),
            Pending(Vec<u8>),
        }
        // pending batch first (read-your-writes — see read_chunk)
        let hashes: Option<Arc<Vec<Hash>>> =
            match self.recipe_batch.lock().get(key).cloned() {
                Some(p) => self.parse_recipe_body(&p).map(|r| Arc::new(r.blob_hashes())),
                None => match self.chunks.read().get(&key).copied() {
                    Some((seg, off, len)) => {
                        if len != BATCHED {
                            self.load_recipe(seg, off, len)
                                .map(|r| Arc::new(r.blob_hashes()))
                        } else {
                            let (boff, soff, rl) =
                                match self.batch_idx.lock().get(&key).copied() {
                                    Some(x) => x,
                                    None => return false,
                                };
                            let pk = (seg, boff, soff, rl);
                            if let Some(set) = self.probe_cache.lock().get(&pk) {
                                Some(Arc::clone(set))
                            } else {
                                let parsed = self.recipe_at(key, seg, off, len)
                                    .map(|r| Arc::new(r.blob_hashes()));
                                if let Some(set) = &parsed {
                                    let mut pc = self.probe_cache.lock();
                                    if pc.len() >= 4096 {
                                        pc.clear();
                                    }
                                    pc.insert(pk, Arc::clone(set));
                                }
                                parsed
                            }
                        }
                    }
                    None => return false,
                },
            };
        let Some(hashes) = hashes else { return false };
        let blobs = self.blobs.read();
        hashes.iter().all(|h| blobs.contains_key(h))
    }

    /// Load one chunk payload (uncompressed NBT, `NbtIo.write` form).
    pub fn read_chunk(&self, key: u64) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        // located in a segment, or still pending in the recipe batch?
        enum Loc {
            Seg(u32, u64, u32),
            Pending(Vec<u8>),
        }
        // READ-YOUR-WRITES: the pending batch wins over the committed
        // chunks-map entry. A rewrite of a BATCHED chunk removes its batch_idx
        // entry at supersede time, so the stale committed pointer is
        // unreadable until the next flush — the pending recipe is the truth.
        let loc = match self.recipe_batch.lock().get(key).cloned() {
            Some(pending) => Loc::Pending(pending),
            None => match self.chunks.read().get(&key).copied() {
                Some((s, o, l)) => Loc::Seg(s, o, l),
                None => return None,
            },
        };
        let dbg = |stage: &str| {
            if std::env::var("CSAVES_READ_DEBUG").map(|v| v == "1").unwrap_or(false) {
                eprintln!("[c-saves] read fail key={key:016x} stage={stage}");
            }
        };
        let fail = |stage: &str| -> Option<Vec<u8>> {
            dbg(stage);
            None
        };
        let t_recipe = std::time::Instant::now();
        let rec = match &loc {
            Loc::Pending(body) => self.parse_recipe_body(body),
            Loc::Seg(seg, off, len) => self.recipe_at(key, *seg, *off, *len),
        };
        {
            let el = t_recipe.elapsed().as_nanos() as u64;
            ST_RECIPE.0.fetch_add(el, Ordering::Relaxed);
            ST_RECIPE.1.fetch_add(1, Ordering::Relaxed);
        }
        let rec = match rec {
            Some(r) => r,
            None => {
                dbg("recipe");
                return None;
            }
        };
        let writer_id = self.writer_seg_id();
        let t_tail = std::time::Instant::now();
        let tail_bytes: Option<Vec<u8>> = match &rec.tail {
            Tail::None => Some(Vec::new()),
            Tail::Inline { dict, comp, raw } => self.decompress(*dict, comp, *raw),
            Tail::Blob(h) => {
                let loc = *self.blobs.read().get(h)?;
                self.read_blob(loc, writer_id)
            }
        };
        {
            let el = t_tail.elapsed().as_nanos() as u64;
            ST_TAIL.0.fetch_add(el, Ordering::Relaxed);
            ST_TAIL.1.fetch_add(1, Ordering::Relaxed);
        }
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
                let t_sec = std::time::Instant::now();
                let blobs = self.blobs.read();
                let mut locs = Vec::with_capacity(rec.sections.len());
                for h in &rec.sections {
                    locs.push(*blobs.get(h)?);
                }
                drop(blobs);
                // batched load: one segs pass, per-slab dedup, borrowed slices
                let loaded = match self.load_sections(&locs, writer_id) {
                    Some(l) => l,
                    None => return fail("section-blob"),
                };
                let slices: Vec<&[u8]> = loaded
                    .refs
                    .iter()
                    .map(|(i, s, e)| &loaded.keep[*i][*s as usize..*e as usize])
                    .collect();
                {
                    let el = t_sec.elapsed().as_nanos() as u64;
                    ST_SECTLOAD.0.fetch_add(el, Ordering::Relaxed);
                    ST_SECTLOAD.1.fetch_add(1, Ordering::Relaxed);
                }
                // shift-ref resolution (v0.7): [0xC9] members expand to
                // canonical CSEC3 against their chunk-local target; keys are
                // re-mixed so the fused emit cache stays content-keyed.
                let resolved = match chunknbt::resolve_shifts(&slices, &rec.sections) {
                    Some(r) => r,
                    None => return fail("shift-resolve"),
                };
                let any_shift = !resolved.is_empty();
                let mut shift_store: Vec<Arc<Vec<u8>>> = Vec::new();
                let mut keys: Vec<chunknbt::Hash> = Vec::with_capacity(rec.sections.len());
                for (i, r) in resolved.iter().enumerate() {
                    match r {
                        Some((nk, b)) => {
                            shift_store.push(b.clone());
                            keys.push(*nk);
                        }
                        None => keys.push(rec.sections[i]),
                    }
                }
                let slices: Vec<&[u8]> = if any_shift {
                    let mut v = Vec::with_capacity(slices.len());
                    let mut si = 0usize;
                    for (i, s) in slices.iter().enumerate() {
                        if resolved[i].is_some() {
                            v.push(shift_store[si].as_slice());
                            si += 1;
                        } else {
                            v.push(s);
                        }
                    }
                    v
                } else {
                    slices
                };
                // fused path: CSEC streams emitted straight to NBT bytes —
                // no per-section Value trees, no HashMap churn; repeat
                // sections splice from the emit cache by content hash
                let sources: Vec<chunknbt::SecSrc> = keys
                    .iter()
                    .zip(slices.iter())
                    .map(|(h, s)| chunknbt::SecSrc { h: *h, b: s })
                    .collect();
                let t_join = std::time::Instant::now();
                let fused = chunknbt::join_fused(&sources, &tail);
                {
                    let el = t_join.elapsed().as_nanos() as u64;
                    ST_JOIN_FUSED.0.fetch_add(el, Ordering::Relaxed);
                    ST_JOIN_FUSED.1.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(v) = fused {
                    return Some(v);
                }
                let t_leg = std::time::Instant::now();
                let sec_bytes: Vec<Vec<u8>> = slices.iter().map(|s| s.to_vec()).collect();
                let r = match chunknbt::join(&sec_bytes, &tail) {
                    Ok(v) => Some(v),
                    Err(e) => fail(&format!("join: {e}")),
                };
                {
                    let el = t_leg.elapsed().as_nanos() as u64;
                    ST_JOIN_LEGACY.0.fetch_add(el, Ordering::Relaxed);
                    ST_JOIN_LEGACY.1.fetch_add(1, Ordering::Relaxed);
                }
                r
            }
        }
    }

    /// Batched section load: one `segs` lock pass, per-slab dedup (a chunk's
    /// 131 sections usually live in one or two slabs), slab-cache fill.
    /// Returns owned buffers + (buffer idx, start, end) refs per section.
    fn load_sections(
        &self,
        locs: &[BlobLoc],
        writer_id: Option<u32>,
    ) -> Option<LoadedSections> {
        if locs.iter().any(|l| writer_id == Some(l.seg)) {
            self.flush_writer();
        }
        let mut keep: Vec<Arc<Vec<u8>>> = Vec::with_capacity(8);
        let mut refs = Vec::with_capacity(locs.len());
        let mut slab_map: HashMap<(u32, u64), usize> = HashMap::new();
        let mut fresh_slabs: Vec<((u32, u64), Arc<Vec<u8>>)> = Vec::new();
        // one cache-lock pass for all hits (131 lock/unlock pairs per chunk
        // were measurable against the actual work)
        {
            let cache = self.slab_cache.lock();
            for loc in locs {
                if loc.slab {
                    let skey = (loc.seg, loc.off);
                    if slab_map.contains_key(&skey) {
                        continue;
                    }
                    if let Some(cached) = cache.get(&skey) {
                        let idx = keep.len();
                        keep.push(cached);
                        slab_map.insert(skey, idx);
                    }
                }
            }
        }
        let segs = self.segs.read();
        for loc in locs {
            if loc.slab {
                let skey = (loc.seg, loc.off);
                if let Some(&idx) = slab_map.get(&skey) {
                    // cached or already loaded in this pass
                    if keep[idx].len() as u64 >= loc.slab_off as u64 + loc.raw_len as u64 {
                        refs.push((idx, loc.slab_off, loc.slab_off + loc.raw_len));
                        continue;
                    }
                    // entry exists but is an unsealed-writer placeholder: fall
                    // through to the full load below
                }
                let seg = segs.get(&loc.seg)?;
                let sealed = Some(loc.seg) != writer_id;
                let hdr = seg.read_at(loc.off, 12, sealed)?;
                let kind = *hdr.first()?;
                if kind != 5 && kind != 8 {
                    return None;
                }
                let cl = u32::from_le_bytes(hdr[2..6].try_into().ok()?) as usize;
                let raw_total = u32::from_le_bytes(hdr[6..10].try_into().ok()?);
                let data = seg.read_at(loc.off + 12, cl, sealed)?;
                let raw = Arc::new(self.decompress(loc.dict_id, &data, raw_total)?);
                if (loc.slab_off + loc.raw_len) as usize > raw.len() {
                    return None;
                }
                fresh_slabs.push((skey, raw.clone()));
                let idx = match slab_map.get(&skey) {
                    Some(&i) if keep[i].len() as u64 >= loc.slab_off as u64 + loc.raw_len as u64 => i,
                    _ => {
                        let idx = keep.len();
                        keep.push(raw);
                        slab_map.insert(skey, idx);
                        idx
                    }
                };
                refs.push((idx, loc.slab_off, loc.slab_off + loc.raw_len));
            } else {
                let seg = segs.get(&loc.seg)?;
                let sealed = Some(loc.seg) != writer_id;
                let data = seg.read_at(loc.off + loc.header_len(), loc.comp_len as usize, sealed)?;
                let raw = Arc::new(self.decompress(loc.dict_id, &data, loc.raw_len)?);
                let idx = keep.len();
                keep.push(raw);
                refs.push((idx, 0, loc.raw_len));
            }
        }
        drop(segs);
        if !fresh_slabs.is_empty() {
            let mut sc = self.slab_cache.lock();
            for (k, v) in fresh_slabs {
                sc.put(k, v);
            }
        }
        Some(LoadedSections { keep, refs })
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
            let kind = *hdr.first()?;
            if kind != 5 && kind != 8 {
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
        self.recipe_at(key, seg, off, len)
    }

    /// Virtual per-entry length for live accounting (BATCHED pointers
    /// resolve through batch_idx).
    fn batched_len(&self, key: u64, len: u32) -> u32 {
        if len == BATCHED {
            self.batch_idx
                .lock()
                .get(&key)
                .map(|(_, _, rl)| *rl)
                .unwrap_or(0)
        } else {
            len
        }
    }

    pub fn has_chunk(&self, key: u64) -> bool {
        self.chunks.read().contains_key(&key)
    }

    pub fn chunk_count(&self) -> usize {
        let pending = self.recipe_batch.lock();
        let chunks = self.chunks.read();
        chunks.len()
            + pending.entries.iter().filter(|(k, _)| !chunks.contains_key(k)).count()
    }

    pub fn keys(&self) -> Vec<u64> {
        let pending = self.recipe_batch.lock();
        let chunks = self.chunks.read();
        let mut v: Vec<u64> = chunks.keys().copied().collect();
        for (k, _) in pending.entries.iter() {
            if !chunks.contains_key(k) {
                v.push(*k);
            }
        }
        v
    }

    pub fn delete_chunk(&self, key: u64) -> Result<(), String> {
        self.flush_recipe_batch();
        self.last_written.lock().remove(&key);
        let old = self.chunks.write().remove(&key);
        if let Some((seg, off, len)) = old {
            if let Some(rec) = self.recipe_at(key, seg, off, len) {
                self.drop_recipe_refs(&rec);
            }
            let rl = self.batched_len(key, len);
            if let Some(s) = self.segs.read().get(&seg) {
                s.live.fetch_sub(8 + rl as u64, Ordering::Relaxed);
            }
            if len == BATCHED {
                self.batch_idx.lock().remove(&key);
            }
        }
        let mut body = vec![3u8];
        body.extend_from_slice(&key.to_le_bytes());
        self.append(&body).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// fsync all segments + flush buffers (end of a save cycle).
    pub fn sync(&self) {
        self.flush_recipe_batch();
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
        self.flush_recipe_batch();
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
        // recipes: live chunk records (BATCHED pointers resolve via batch_idx)
        let mut rec_bytes = 0u64;
        {
            let chunks = self.chunks.read();
            for (k, (_s, _o, l)) in chunks.iter() {
                let rl = if *l == BATCHED {
                    self.batch_idx.lock().get(k).map(|(_, _, r)| *r as u64).unwrap_or(0)
                } else {
                    *l as u64
                };
                rec_bytes += 8 + rl;
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
        s.push_str(&format!(r#""write_skip":{},"#, ST_WRITE_SKIP.load(Ordering::Relaxed)));
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
        let slab_raw_cap = if self.whole_mode.load(Ordering::Relaxed) {
            self.cfg.slab_max_raw_whole
        } else {
            self.cfg.slab_max_raw
        };
        let mut groups: Vec<Vec<(Hash, u32, BlobLoc, Vec<u8>)>> = Vec::new();
        let mut cur: Vec<(Hash, u32, BlobLoc, Vec<u8>)> = Vec::new();
        let mut cur_raw = 24usize; // entry table floor
        for c in cands.drain(..) {
            let need = 24 + c.3.len();
            if !cur.is_empty() && cur_raw + need > slab_raw_cap {
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
            // SLAB V2 (kind 8): entry table = varint zigzag(id delta) ++
            // varint(raw len) ++ 16B hash per member, then concatenated raws.
            // v1 spent 8 fixed bytes/member on id+len; with ids assigned in
            // walk order the deltas are ~1 byte, lens ~1-2 bytes.
            let mut tab = Vec::with_capacity(n * 4);
            let mut rawcat = Vec::with_capacity(group.iter().map(|c| c.3.len()).sum::<usize>());
            let mut prev_id: i64 = 0;
            for (h, id, _loc, raw) in &group {
                put_u64(&mut tab, zigzag(*id as i64 - prev_id));
                prev_id = *id as i64;
                put_u64(&mut tab, raw.len() as u64);
                tab.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                tab.extend_from_slice(&(*h as u64).to_le_bytes());
                rawcat.extend_from_slice(raw);
            }
            let tab_len = tab.len();
            let mut stream = tab;
            stream.extend_from_slice(&rawcat);
            let raw_total = stream.len() as u32;
            if std::env::var("CSAVES_SLAB_DEBUG").map(|v| v == "1").unwrap_or(false) {
                eprintln!(
                    "[c-saves] slab: n={n} tab={tab_len} rawcat={} members_raw_min={} members_raw_max={}",
                    rawcat.len(),
                    group.iter().map(|c| c.3.len()).min().unwrap_or(0),
                    group.iter().map(|c| c.3.len()).max().unwrap_or(0)
                );
                if let Ok(d) = std::env::var("CSAVES_SLAB_DUMP") {
                    let i = self.next_seg.load(Ordering::Relaxed);
                    let p = std::path::PathBuf::from(d).join(format!("slabdump-{i}-{}.bin", group[0].0 as u64));
                    let _ = std::fs::write(p, &stream);
                }
            }
            // Both store kinds compress plain: section slabs measured -4%
            // without the dict, and whole-mode stores no longer train one.
            let (dict_id, comp) = self.compress_tier_plain(&stream, true);
            let mut body = Vec::with_capacity(12 + comp.len());
            body.push(8u8);
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
            let mut cursor = tab_len as u32;
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
                let Some(rec) = self.recipe_at(*key, *seg, *off, *len) else { continue };
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
            // route through the recipe batch (same accounting as compaction
            // moves); the chunks map flips at flush
            {
                let mut bare = Vec::with_capacity(1 + enc.len());
                bare.push(0xFFu8);
                bare.extend_from_slice(&enc);
                self.recipe_batch.lock().push(key, bare);
            }
            self.bump_recipe_refs(&new_rec);
            if let Some(old_rec) = self.load_recipe(seg, off, len) {
                self.drop_recipe_refs(&old_rec);
            }
            if let Some(s) = self.segs.read().get(&seg) {
                s.live.fetch_sub(8 + len as u64, Ordering::Relaxed);
            }
            rewritten += 1;
            let total = self.recipe_batch.lock().bytes;
            if total >= 192 * 1024 {
                self.flush_recipe_batch();
            }
        }
        self.flush_recipe_batch();
        rewritten > 0
    }

    /// Compact at most one segment: move live data (recompressing blobs at
    /// the cold tier), then drop the segment. Returns true if work was done.
    pub fn compact_step(&self) -> bool {
        self.train_dict_if_needed();
        self.flush_recipe_batch();
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
        if let Some((worst_id, ratio)) = worst {
            if ratio >= self.cfg.compact_ratio {
                return self.pack_segment(worst_id, true);
            }
        }
        self.pack_fallback(writer_id)
    }

    /// Mark this store as whole-mode (entity/POI payloads): slab streams are
    /// then compressed WITH the shared dictionary (measured win on raw-NBT
    /// payloads; section stores measure a win WITHOUT it).
    pub fn set_whole_mode(&self) {
        self.whole_mode.store(true, Ordering::Relaxed);
        // Entity/POI stores never persist a shared dictionary: batched 4 MiB
        // slabs self-compress to the concatenated floor, and a trained dict
        // (up to 112 KiB on disk) measured a net LOSS on the real JJThunder
        // entity corpus (dict saved ~38 KiB of compressed bytes but cost 114 KiB).
        self.dict_enabled.store(false, Ordering::Relaxed);
    }

    /// Request a pack pass (slab-packing + cold-tier recompression) without
    /// waiting for dead bytes. Used after Anvil imports and for fresh data
    /// accumulation (auto-watermark).
    pub fn request_pack(&self) {
        self.pack_requested.store(true, Ordering::Relaxed);
    }

    /// Pack pass without dead bytes: picks the sealed non-writer segment
    /// holding the most hot-tier bytes and re-packs only its hot blobs
    /// (small -> kind-5 slabs, big -> cold-tier BLOB2). Cold and slab data
    /// is left in place, so repeated passes never re-compress packed bytes.
    fn pack_fallback(&self, writer_id: u32) -> bool {
        let mut do_pack = self.pack_requested.swap(false, Ordering::Relaxed);
        if !do_pack && self.cfg.auto_pack_bytes > 0 {
            let disk = self.disk_bytes();
            if disk.saturating_sub(self.pack_watermark.load(Ordering::Relaxed))
                >= self.cfg.auto_pack_bytes
            {
                do_pack = true;
            }
        }
        if !do_pack {
            return self.optimize_fields_step();
        }
        let mut hot: HashMap<u32, u64> = HashMap::new();
        {
            let blobs = self.blobs.read();
            for l in blobs.values() {
                if l.slab || l.cold || l.seg == writer_id {
                    continue;
                }
                *hot.entry(l.seg).or_default() += l.body_len();
            }
        }
        let pick = hot.iter().max_by_key(|(_, b)| **b).map(|(s, _)| *s);
        let did = match pick {
            Some(id) => self.pack_segment(id, false),
            None => false,
        };
        self.pack_watermark.store(self.disk_bytes(), Ordering::Relaxed);
        if !did {
            return self.optimize_fields_step();
        }
        true
    }

    /// Move one segment's live data to fresh records. `drop_seg=true` rewrites
    /// everything (recipes included) and removes the segment (dead-ratio
    /// compaction); `drop_seg=false` re-packs only hot blobs and keeps the
    /// segment (continuous pack pass).
    fn pack_segment(&self, worst_id: u32, drop_seg: bool) -> bool {
        self.pack_requested.store(false, Ordering::Relaxed);
        // Locality-ordered victims: walk live recipes in chunk-key order and
        // collect their blobs in section (Y) order, so sections of one chunk
        // become adjacent slab members. Measured -17% slab bytes vs map-order
        // packing (same-chunk sections share palettes and data texture).
        let mut victims: Vec<(Hash, BlobLoc)> = Vec::new();
        {
            let blobs = self.blobs.read();
            let mut seen: std::collections::HashSet<Hash> = std::collections::HashSet::new();
            {
                let chunks = self.chunks.read();
                let mut keys: Vec<u64> = chunks.keys().copied().collect();
                keys.sort_unstable();
                for k in keys {
                    let Some((seg, off, len)) = chunks.get(&k).copied() else { continue };
                    let Some(rec) = self.recipe_at(k, seg, off, len) else { continue };
                    for h in rec.blob_hashes() {
                        if seen.insert(h) {
                            if let Some(l) = blobs.get(&h) {
                                if l.seg == worst_id && (drop_seg || !(l.slab || l.cold)) {
                                    victims.push((h, *l));
                                }
                            }
                        }
                    }
                }
            }
            // hot blobs not covered by any live recipe (orphans) go last
            for (h, l) in blobs.iter() {
                if l.seg == worst_id && !seen.contains(h) && (drop_seg || !(l.slab || l.cold)) {
                    victims.push((*h, *l));
                }
            }
        }
        let mut cands: Vec<(Hash, u32, BlobLoc, Vec<u8>)> = Vec::new();
        let mut cand_bytes = 0usize;
        let mut did = false;
        for (h, loc) in victims {
            if !drop_seg && (loc.slab || loc.cold) {
                continue; // already packed: leave in place
            }
            let Some(data) = self.read_blob(loc, Some(worst_id)) else { continue };
            did = true;
            let member_cap = if self.whole_mode.load(Ordering::Relaxed) {
                self.cfg.slab_max_member_whole
            } else {
                self.cfg.slab_max_member
            };
            if (data.len() as u32) <= member_cap {
                let id = if loc.id != NO_ID {
                    loc.id
                } else {
                    self.next_blob_id.fetch_add(1, Ordering::Relaxed) as u32
                };
                cand_bytes += data.len();
                cands.push((h, id, loc, data));
                let slab_cap = if self.whole_mode.load(Ordering::Relaxed) {
                    self.cfg.slab_max_raw_whole
                } else {
                    self.cfg.slab_max_raw
                };
                if cand_bytes >= slab_cap {
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
                l.cold = true;
                if l.id == NO_ID {
                    l.id = id;
                    self.ids.write().insert(id, h);
                }
            }
        }
        self.flush_slab_cands(&mut cands, worst_id);
        if !drop_seg {
            // the segment may have emptied out entirely
            let empty = self
                .segs
                .read()
                .get(&worst_id)
                .map(|s| s.live.load(Ordering::Relaxed) == 0)
                .unwrap_or(true);
            if empty {
                let old = self.segs.write().remove(&worst_id);
                drop(old);
                let _ = fs::remove_file(self.seg_path(worst_id));
                self.gc_dicts();
            }
            return did;
        }
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
            // normalize to a bare batched recipe
            let bare: Vec<u8> = if len == BATCHED {
                // entry lives inside a compressed kind-9 batch: pull the
                // bare bytes out of the decompressed stream
                let Some((boff, soff, rl)) = self.batch_idx.lock().get(&key).copied() else {
                    eprintln!("[c-saves] LEAK: batch_idx miss key={key:016x} during seg-{worst_id} rewrite");
                    continue;
                };
                if _s != worst_id {
                    continue;
                }
                let is_writer = Some(worst_id) == self.writer_seg_id();
                if is_writer {
                    self.flush_writer();
                }
                let segs = self.segs.read();
                let Some(seg) = segs.get(&worst_id) else { continue };
                let Some(hdr) = seg.read_at(boff, 13, !is_writer) else {
                    eprintln!("[c-saves] LEAK: hdr read fail key={key:016x} boff={boff}");
                    continue;
                };
                let cl = u32::from_le_bytes(hdr[5..9].try_into().unwrap()) as usize;
                let raw_total = u32::from_le_bytes(hdr[9..13].try_into().unwrap());
                let Some(data) = seg.read_at(boff + 13, cl, !is_writer) else {
                    eprintln!("[c-saves] LEAK: body read fail key={key:016x} cl={cl}");
                    continue;
                };
                drop(segs);
                let Some(raw) = self.decompress(0, &data, raw_total) else {
                    eprintln!("[c-saves] LEAK: batch decompress fail key={key:016x}");
                    continue;
                };
                let Some(e) = raw.get(soff as usize..(soff + rl) as usize) else {
                    eprintln!("[c-saves] LEAK: slice fail key={key:016x} soff={soff} rl={rl} rawlen={}", raw.len());
                    continue;
                };
                e.to_vec()
            } else {
                let segs = self.segs.read();
                let Some(seg) = segs.get(&worst_id) else { continue };
                let Some(body) = seg.read_at(off, len as usize, false) else { continue };
                drop(segs);
                if body.first() == Some(&0xFF) {
                    body
                } else if body.first() == Some(&2) && body.len() >= 13 {
                    let rl = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
                    if body.len() < 13 + rl {
                        continue;
                    }
                    let mut b = Vec::with_capacity(1 + rl);
                    b.push(0xFFu8);
                    b.extend_from_slice(&body[13..13 + rl]);
                    b
                } else {
                    continue;
                }
            };
            // capture the OLD virtual length and batch location BEFORE the
            // re-queue: an in-loop flush may already give this key a fresh
            // batch in the writer segment, and the cleanup below must only
            // retire the OLD index entry (pointing into the dying segment).
            let old_rl = self.batched_len(key, len);
            {
                let mut batch = self.recipe_batch.lock();
                batch.push(key, bare);
                let total = batch.bytes;
                drop(batch);
                if total >= 192 * 1024 {
                    self.flush_recipe_batch();
                }
            }
            if let Some(old) = self.segs.read().get(&worst_id) {
                old.live.fetch_sub(8 + old_rl as u64, Ordering::Relaxed);
            }
            if len == BATCHED {
                let mut b = self.batch_idx.lock();
                // retire only the OLD index entry (pointing into the dying
                // segment): an in-loop flush may already have given this key
                // a fresh batch in the writer segment — that entry must stay.
                if b.get(&key).map(|(bo, _, _)| *bo == off).unwrap_or(false) {
                    b.remove(&key);
                }
            }
        }
        self.flush_recipe_batch();
        // preserve interned keys recorded in this segment (kind-6 records
        // are not referenced by recipes but the key table must survive)
        self.preserve_keys(worst_id);
        // drop the segment
        let old = self.segs.write().remove(&worst_id);
        drop(old);
        let _ = fs::remove_file(self.seg_path(worst_id));
        self.gc_dicts();
        eprintln!("[c-saves] packed seg-{worst_id:05} (full rewrite, cold-tier recompressed)");
        true
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;

    pub(crate) fn key(cx: i32, cz: i32) -> u64 {
        ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
    }

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
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

    pub(crate) fn bench_cfg() -> Cfg {
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
    fn shared_sections_pack_rewrite_roundtrip() {
        // Regression guard for the ref-loss the unchanged-write fast path
        // exposed on the speed bench: many chunks share section blobs;
        // import -> pack -> full rewrite rounds must keep EVERY chunk
        // readable with identical content.
        let dir = temp_dir("shared-pack-rewrite");
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let n = 40usize;
        let payloads: Vec<Vec<u8>> = (0..n).map(|i| chunk_payload((i % 8) as i32, (i / 8) as i32, (i % 5) as u64)).collect();
        for (i, p) in payloads.iter().enumerate() {
            s.write_chunk(i as u64, p, false).unwrap();
        }
        s.sync();
        s.seal();
        s.request_pack();
        loop {
            if !s.compact_step() {
                break;
            }
        }
        s.sync();
        // reopen (fresh caches, recipes + refs rebuilt from disk)
        drop(s);
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "post-pack-reopen chunk {i}");
        }
        // rewrite rounds (the speed-bench G model)
        for _ in 0..3 {
            for (i, p) in payloads.iter().enumerate() {
                s.write_chunk(i as u64, p, false).unwrap();
            }
        }
        s.sync();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "post-rewrite chunk {i}");
        }
    }

    #[test]
    fn unchanged_write_skip_and_invalidation() {
        let dir = temp_dir("skip-test");
        let s = Store::open_cfg(&dir, false, Cfg::default()).unwrap();
        let p = chunk_payload(7, 42, 1);
        s.write_chunk(1, &p, false).unwrap();
        let before = ST_WRITE_SKIP.load(Ordering::Relaxed);
        // identical re-save (autosave of an unchanged chunk): skipped
        s.write_chunk(1, &p, false).unwrap();
        assert_eq!(
            ST_WRITE_SKIP.load(Ordering::Relaxed),
            before + 1,
            "identical re-write must hit the skip path"
        );
        assert!(semantic(&s.read_chunk(1).unwrap(), &p));
        // changed content: full path again
        let p2 = chunk_payload(7, 43, 2);
        s.write_chunk(1, &p2, false).unwrap();
        assert_eq!(ST_WRITE_SKIP.load(Ordering::Relaxed), before + 1);
        assert!(semantic(&s.read_chunk(1).unwrap(), &p2));
        // delete invalidates the cache: same content re-writes fully
        s.delete_chunk(1).unwrap();
        assert!(s.read_chunk(1).is_none());
        s.write_chunk(1, &p, false).unwrap();
        assert_eq!(ST_WRITE_SKIP.load(Ordering::Relaxed), before + 1);
        assert!(semantic(&s.read_chunk(1).unwrap(), &p));
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

    /// JJThunder-style extreme-height chunk: 131 sections dominated by long
    /// Y-runs of identical uniform sections. Exercises recipe mode 6 (RLE
    /// over the delta-id stream) through write, read, reopen-scan and read.
    #[test]
    fn recipe_mode6_rle_tall_chunk_roundtrip() {
        let dir = temp_dir("mode6");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();

        let uniform_section = |y: i8, block: &str| -> Value {
            let mut sec: std::collections::HashMap<String, Value> = Default::default();
            sec.insert("Y".to_string(), Value::Byte(y));
            let mut bs: std::collections::HashMap<String, Value> = Default::default();
            bs.insert(
                "palette".to_string(),
                Value::List(vec![Value::String(block.to_string())]),
            );
            sec.insert("block_states".to_string(), Value::Compound(bs));
            Value::Compound(sec)
        };
        let mut root: std::collections::HashMap<String, Value> = Default::default();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        let mut sec_list = Vec::new();
        // y -32..-29: stone boundary sections with data; -28..71: air runs
        for y in -32..-28i32 {
            let mut sec: std::collections::HashMap<String, Value> = Default::default();
            sec.insert("Y".to_string(), Value::Byte(y as i8));
            let mut bs: std::collections::HashMap<String, Value> = Default::default();
            // unequal counts keep the frequency-canonical palette order equal
            // to the source order, so raw NBT equality holds after decode.
            // Pack at the vanilla-clamped 4-bit width: the decoder normalizes
            // narrow streams back to max(4, bits) — the input must already be
            // in that shape for raw equality.
            let entries: Vec<u8> = (0..4096).map(|i| if i < 1024 { 1u8 } else { 0u8 }).collect();
            bs.insert(
                "palette".to_string(),
                Value::List(vec![
                    Value::String("minecraft:stone".to_string()),
                    Value::String("minecraft:deepslate".to_string()),
                ]),
            );
            bs.insert(
                "data".to_string(),
                Value::LongArray(fastnbt::LongArray::new(crate::chunknbt::pack_nostroddle(
                    &entries, 4,
                ))),
            );
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        for y in -28..100i32 {
            let block = if y < 60 { "minecraft:air" } else { "minecraft:cave_air" };
            sec_list.push(uniform_section(y as i8, block));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        let payload = fastnbt::to_bytes(&Value::Compound(root)).unwrap();

        st.write_chunk(key(9, -9), &payload, false).unwrap();
        let got = st.read_chunk(key(9, -9)).expect("read");
        assert!(semantic(&got, &payload), "mode-6 tall chunk mismatch");
        st.sync();
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st2.chunk_count(), 1, "reopen must scan the mode-6 recipe");
        let got2 = st2.read_chunk(key(9, -9)).expect("read after reopen");
        assert!(semantic(&got2, &payload), "mode-6 tall chunk mismatch after reopen");
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

    /// A legacy kind-7 (uncompressed batch) record written by an older
    /// version must still scan, load and rewrite cleanly.
    #[test]
    fn legacy_kind7_batch_still_loads_and_rewrites() {
        let dir2 = temp_dir("k7legacy2");
        let st2 = Store::open_cfg(&dir2, true, bench_cfg()).unwrap();
        let mk = |i: i32| -> Vec<u8> {
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            root.insert("Position".to_string(), Value::List(vec![
                Value::Int(i), Value::Int(0), Value::Int(i),
            ]));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };
        let p0 = mk(1);
        let p1 = mk(2);
        // materialize the whole-payload blobs the forged recipes reference
        for p in [&p0, &p1] {
            let h = chunknbt::hash_of(p);
            st2.ensure_blob(h, p, false).unwrap();
        }
        // forge a legacy kind-7 record with two mode-0 whole recipes
        let bare = |p: &[u8]| -> Vec<u8> {
            let h = chunknbt::hash_of(p);
            let mut r = vec![0xFFu8, 0u8];
            r.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
            r.extend_from_slice(&(h as u64).to_le_bytes());
            r
        };
        let mut body: Vec<u8> = Vec::new();
        let mut starts: Vec<(u64, usize, usize)> = Vec::new();
        for (kx, p) in [(10i32, &p0), (11, &p1)] {
            let b = bare(p);
            let i = body.len() + 10;
            body.extend_from_slice(&key(kx, 0).to_le_bytes());
            body.extend_from_slice(&(b.len() as u16).to_le_bytes());
            body.extend_from_slice(&b);
            starts.push((key(kx, 0), i, b.len()));
        }
        let mut rec = vec![7u8];
        rec.extend_from_slice(&(starts.len() as u32).to_le_bytes());
        rec.extend_from_slice(&body);
        let (seg, off, _len) = st2.append(&rec).unwrap();
        {
            let mut chunks = st2.chunks.write();
            let base = off + 5;
            for (k, i, l) in &starts {
                chunks.insert(*k, (seg, base + *i as u64, *l as u32));
            }
        }
        st2.sync();
        drop(st2);
        // reopen: scan must index the kind-7 entries; reads resolve
        let st3 = Store::open_cfg(&dir2, true, bench_cfg()).unwrap();
        assert_eq!(st3.chunk_count(), 2, "legacy kind-7 entries must scan");
        for (kx, p) in [(10i32, &p0), (11, &p1)] {
            let got = st3.read_chunk(key(kx, 0)).expect("kind-7 readable");
            let va: Value = fastnbt::from_bytes(&got).unwrap();
            let vb: Value = fastnbt::from_bytes(p).unwrap();
            assert_eq!(va, vb, "kind-7 payload mismatch");
        }
        // full rewrite must move kind-7 recipes into fresh batches without loss
        st3.sync();
        st3.seal();
        for _ in 0..10 {
            if !st3.compact_step() {
                break;
            }
        }
        for (kx, p) in [(10i32, &p0), (11, &p1)] {
            let got = st3.read_chunk(key(kx, 0)).expect("kind-7 readable after rewrite");
            let va: Value = fastnbt::from_bytes(&got).unwrap();
            let vb: Value = fastnbt::from_bytes(p).unwrap();
            assert_eq!(va, vb);
        }
        let _ = fs::remove_dir_all(&dir2);
    }

    /// Full pack rewrite under delete pressure: every survivor must stay
    /// byte-identical across batch re-batching, segment drops and reopen.
    /// (Companion to the batch_idx stale-removal guard in pack_segment:
    /// an in-loop flush_recipe_batch must never retire freshly-created
    /// index entries — caught on the real JJThunder corpus, see the
    /// `bo == off` location check.)
    #[test]
    fn batch_rewrite_under_flush_pressure() {
        let dir = temp_dir("k9pressure");
        let cfg = Cfg { compact_min: 64 * 1024, compact_ratio: 0.10, ..bench_cfg() };
        let st = Store::open_cfg(&dir, true, cfg).unwrap();
        // region-style payload: many sections (shared between chunks) + a
        // small unique tail -> mode-2 recipes with sizable bare bodies
        let payload = |i: u64| -> Vec<u8> {
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            let mut sec_list = Vec::new();
            for y in 0..132i32 {
                let mut sec: Map<String, Value> = Map::new();
                sec.insert("Y".to_string(), Value::Byte(y as i8));
                // sections repeat across chunks so ids dedup and deltas stay tiny
                let mut bs: Map<String, Value> = Map::new();
                bs.insert("palette".to_string(), Value::List(vec![
                    Value::String(format!("minecraft:stone_{}", (i + y as u64) % 8)),
                ]));
                bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(vec![7i64; 37])));
                sec.insert("block_states".to_string(), Value::Compound(bs));
                sec_list.push(Value::Compound(sec));
            }
            root.insert("sections".to_string(), Value::List(sec_list));
            root.insert("Heightmaps".to_string(), Value::Compound({
                let mut hm: Map<String, Value> = Map::new();
                hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(fastnbt::LongArray::new(vec![i as i64; 40])));
                hm
            }));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };
        let n = 4000u64;
        for i in 0..n {
            st.write_chunk(key(i as i32, 3), &payload(i), false).unwrap();
        }
        // delete 40% -> dead bytes -> full rewrites; survivors must survive
        for i in 0..n {
            if i % 5 < 2 {
                st.delete_chunk(key(i as i32, 3)).unwrap();
            }
        }
        st.sync();
        st.seal();
        for _ in 0..50 {
            if !st.compact_step() {
                break;
            }
        }
        let mut live = 0;
        for i in 0..n {
            if i % 5 < 2 {
                continue;
            }
            let got = st.read_chunk(key(i as i32, 3)).expect("survivor readable after pack rewrite");
            let va: Value = fastnbt::from_bytes(&got).unwrap();
            let vb: Value = fastnbt::from_bytes(&payload(i)).unwrap();
            assert_eq!(va, vb, "chunk {i} mismatch after pack rewrite");
            live += 1;
        }
        assert_eq!(live, (n * 3 / 5) as usize);
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st2.chunk_count(), (n * 3 / 5) as usize);
        for i in 0..n {
            if i % 5 < 2 {
                continue;
            }
            st2.read_chunk(key(i as i32, 3)).expect("survivor readable after reopen");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod repro {
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;

    pub(crate) fn key(cx: i32, cz: i32) -> u64 {
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

#[cfg(test)]
mod reopen_regression {
    use super::tests::{bench_cfg, key, temp_dir};
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;

    /// WRITE -> REOPEN -> WRITE MORE -> REOPEN -> verify everything.
    /// Regression for the next_seg reset that overwrote seg-00000.
    #[test]
    fn writes_after_reopen_append_not_overwrite() {
        let dir = temp_dir("reopensave");
        let n = 120usize;
        let payload = |i: usize| -> Vec<u8> {
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            root.insert("Position".to_string(), Value::List(vec![
                Value::Int(i as i32), Value::Int(0), Value::Int(0)]));
            root.insert("Pad".to_string(), Value::String("x".repeat(64 + i % 200)));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };
        // session 1
        {
            let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
            for i in 0..n {
                st.write_chunk(key(i as i32, 0), &payload(i), true).unwrap();
            }
            st.sync();
            st.seal();
        }
        // session 2: reopen + write MORE chunks (new seg must not clobber)
        {
            let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
            assert_eq!(st.chunk_count(), n);
            for i in n..2 * n {
                st.write_chunk(key(i as i32, 0), &payload(i), true).unwrap();
            }
            st.sync();
            st.seal();
        }
        // session 3: everything must survive
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        assert_eq!(st.chunk_count(), 2 * n, "all chunks must survive reopen+write");
        for i in 0..2 * n {
            let got = st.read_chunk(key(i as i32, 0)).expect("chunk survives");
            let a: Value = fastnbt::from_bytes(&got).unwrap();
            let b: Value = fastnbt::from_bytes(&payload(i)).unwrap();
            assert_eq!(a, b, "chunk {i} corrupted by reopen-write cycle");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
