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
//! 112KB retrained shared dictionaries, hot tier (zstd fast level -3 —
//! import +13% vs level 3 at an identical DRAINED size, v0.9.24) on the
//! write path, cold tier (level 19) recompression during background
//! compaction.

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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

const SEG_ROLL: u64 = 64 * 1024 * 1024;
const BUF_CAP: usize = 1 << 20;
const MAX_SECTIONS: usize = 255;
const NO_ID: u32 = u32::MAX;
/// Import-path hot-slab batching: a write carrying at least this many
/// sections AND this much raw section bytes goes into ONE hot-tier slab
/// frame instead of one zstd frame per section (~130 calls of 0.25-4 KiB
/// measured 16.5% of import wall time; the frame also gives zstd the
/// whole-chunk window). Below the thresholds the entry-table + record
/// overhead would beat the saved calls — the per-blob path stays.
const HOT_SLAB_MIN_SECS: usize = 8;
const HOT_SLAB_MIN_RAW: usize = 8 * 1024;
/// SLAB V2 kind-bit marking a HOT-tier slab (write-path batch frame).
/// scan() reconstructs these members with cold=false so the pack pass can
/// still re-group them into 4-8 MiB cold frames; pack-produced slabs (no
/// bit) rebuild as cold=true and stay in place. Legacy containers written
/// before the bit never carry it — they rebuild exactly as before.
const SLAB_KIND_HOT: u8 = 0x40;
/// v0.9.41: dict_id sentinel marking an LZMA2-compressed slab stream
/// (CSAVES_COLD_CODEC=lzma2). Rides in the same wire byte as the zstd dict
/// id, so every read path dispatches through the one decompress() hook with
/// zero call-site changes (the 0.9.39 raw-sentinel property). 0xFF stays
/// RAW (hot frames); dict ids for BLOBS are assigned 1..=253 from here on —
/// training stops at the 254 line, so a fresh store can never mint dict
/// version 0xFE. A pre-existing store with dict-254 files on disk would
/// misroute those blobs; reaching 254 retrains in one store lifetime is
/// already past the documented exhaustion edge, same reasoning class as
/// the 0xFF reservation.
const SLAB_CODEC_LZMA2: u8 = 0xFE;
/// LZMA2 preset for cold slab streams (research probe point: -24.8% vs
/// zstd-6 on the real corpus). FIXED on both encode and decode so the raw
/// decoder's window always matches — CSAVES_COLD_LEVEL applies to the zstd
/// codec only.
///
/// v0.9.43: the ENCODER adds PRESET_EXTREME (deeper match search: nice_len
/// 273 vs 64). Measured on the real post-dedup member stream (12,130
/// members, one 6.29 MiB group): 940,873 -> 915,488 B = -2.70%, identical
/// bytes for 6e/8e/9e — the win is the search depth, NOT the window (a
/// preset-6 dict already covers the 8 MiB slab cap, and dict 64 MiB
/// measured +0.00%). EXTREME changes only encoder search params, so the
/// DECODE side stays new_preset(6): same 8 MiB window, same per-decode
/// memory, byte-compatible with every 0xFE stream ever written.
const LZMA2_COLD_PRESET: u32 = 6;
/// Encoder-only search-depth flag (lzma PRESET_EXTREME); never applied on
/// decode — the decode window comes from LZMA2_COLD_PRESET alone.
const LZMA2_COLD_EXTREME: u32 = liblzma::stream::PRESET_EXTREME;
/// chunks-map len marker: recipe lives inside a compressed kind-9 batch;
/// the real (rec_off, stream_off, len) is in `batch_idx`.
const BATCHED: u32 = u32::MAX;

/// Bucket hasher for keys that are ALREADY fingerprints: Hash (128-bit
/// content hash) and sequential u32 blob ids. Map correctness never relies
/// on the bucket hash (every probe full-key-compares), so uniformity is all
/// we need — one multiply per 64-bit half beats SipHash's ~25 ns on the
/// ~520 map ops each import chunk does in ensure_blobs.
#[derive(Default, Clone, Copy)]
pub(crate) struct RawHasher(u64);
impl std::hash::Hasher for RawHasher {
    #[inline]
    fn write_u128(&mut self, v: u128) {
        self.0 = (v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ ((v >> 64) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.0 = v as u64;
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0 ^ 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h = (h ^ *b as u64).wrapping_mul(0x1000_0000_01B3);
        }
        self.0 = h;
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
/// BuildHasher pair for RawHasher (Default = zeroed).
#[derive(Default, Clone, Copy)]
pub(crate) struct RawBuild;
impl std::hash::BuildHasher for RawBuild {
    type Hasher = RawHasher;
    #[inline]
    fn build_hasher(&self) -> RawHasher {
        RawHasher(0)
    }
}

/// Per-thread reusable zstd compressor: zstd::bulk::compress creates a fresh
/// CCtx (workspace alloc + param setup) per call; the slab path compresses
/// one ~34 KiB frame per chunk, so the setup is pure overhead. Cached per
/// level — import threads stay on hot_level, the maintenance thread on
/// cold_level. Output is byte-identical to bulk::compress (same code path,
/// only the context lifetime differs).
thread_local! {
    static HOT_COMP: std::cell::RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
        const { std::cell::RefCell::new(None) };
}
// v0.9.41: tests must NOT set CSAVES_COLD_CODEC via set_var — same class
// of race as FORCE_GLOB (0.9.35): the codec resolves into a OnceLock and
// parallel tests would race it. Consulted only under cfg!(test).
pub static COLD_CODEC_TEST: (AtomicBool, AtomicU64) = (AtomicBool::new(false), AtomicU64::new(0));
#[cfg(test)]
pub(crate) fn set_cold_codec_for_tests(v: Option<&str>) {
    match v {
        Some(v) => {
            let id = match v {
                "lzma2" => 1u64,
                _ => 0u64,
            };
            COLD_CODEC_TEST.1.store(id, Ordering::Relaxed);
            COLD_CODEC_TEST.0.store(true, Ordering::Relaxed);
        }
        None => COLD_CODEC_TEST.0.store(false, Ordering::Relaxed),
    }
}

/// Resolved ONCE: cold slab streams go through LZMA2 (preset 6) instead of
/// zstd. v0.9.42: LZMA2 IS THE DEFAULT — the research probe measured -24.8%
/// vs zstd-6 on the real corpus, the real-container bench -20.5% (1.70 ->
/// 1.35 MiB, x0.11 -> x0.08 vs anvil), the cold-read verify wall measured
/// NEUTRAL (25.8 s vs 28.2 s on the 1661-chunk verify — the 20% smaller
/// container buys back the decode), and the pack wall stays background /
/// watermark-throttled by design (8.4 s vs 1.0 s on the bench scale, per-
/// stream parallel). CSAVES_COLD_CODEC=zstd opts back out; the per-record
/// codec stamp (0xFE vs dict_id) makes both containers readable either way.
fn cold_codec_lzma2() -> bool {
    static LZ: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if cfg!(test) && COLD_CODEC_TEST.0.load(Ordering::Relaxed) {
        return COLD_CODEC_TEST.1.load(Ordering::Relaxed) == 1;
    }
    *LZ.get_or_init(|| match std::env::var("CSAVES_COLD_CODEC").as_deref() {
        Ok("zstd") => false,
        _ => true, // default LZMA2 ("lzma2" explicit or unset)
    })
}

/// Raw LZMA2 stream (no xz container — the slab record already carries
/// framing + CRC). Returns None on any error so the caller falls back to
/// the zstd path with the correct per-record codec stamp (never silently
/// mislabel a stream).
fn lzma2_compress(data: &[u8]) -> Option<Vec<u8>> {
    // v0.9.43: 6 | EXTREME — measured -2.70% on the real member stream for
    // 1.66x encode wall on the (parallel, background) pack path; the decode
    // side is untouched (see LZMA2_COLD_PRESET docs).
    let opts = liblzma::stream::LzmaOptions::new_preset(LZMA2_COLD_PRESET | LZMA2_COLD_EXTREME)
        .ok()?;
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_encoder(&f).ok()?;
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    stream.process_vec(&[], &mut out, liblzma::stream::Action::Finish).ok()?;
    Some(out)
}

/// Decode a raw LZMA2 slab stream. The decoder window is pinned to
/// LZMA2_COLD_PRESET's dict on both ends (the raw format does not carry the
/// dict size — see the const docs).
fn lzma2_decompress(data: &[u8], raw_len: usize) -> Option<Vec<u8>> {
    let opts = liblzma::stream::LzmaOptions::new_preset(LZMA2_COLD_PRESET).ok()?;
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut stream = liblzma::stream::Stream::new_raw_decoder(&f).ok()?;
    let mut out = Vec::with_capacity(raw_len);
    stream.process_vec(data, &mut out, liblzma::stream::Action::Run).ok()?;
    Some(out)
}

fn bulk_compress(data: &[u8], level: i32) -> (u8, Vec<u8>) {
    HOT_COMP.with(|cell| {
        let mut slot = cell.borrow_mut();
        let hit = matches!(slot.as_ref(), Some((lv, _)) if *lv == level);
        if hit {
            let (_, comp) = slot.as_mut().unwrap();
            match comp.compress(data) {
                Ok(out) => (0, out),
                Err(_) => (0, data.to_vec()),
            }
        } else {
            match zstd::bulk::Compressor::new(level) {
                Ok(mut comp) => {
                    let r = comp.compress(data);
                    *slot = Some((level, comp));
                    match r {
                        Ok(out) => (0, out),
                        Err(_) => (0, data.to_vec()),
                    }
                }
                Err(_) => (0, data.to_vec()),
            }
        }
    })
}

/// One-shot multi-threaded zstd (pack pass): nbWorkers = workers inside ONE
/// standard zstd frame. The frame bytes differ from the single-threaded
/// encoder (job boundaries + per-job overlap) but are deterministic per
/// (input, level, worker count) and decode with the stock single-threaded
/// decoder — dedup hashes are content-based (uncompressed) and every
/// downstream gate is semantic, so the wire stays fully compatible. The
/// pack wall is the LONGEST single-stream compression (one 12 MB GLOB
/// variant = 4-5 s of serial zstd-19 that no per-stream pool can touch);
/// intra-frame workers divide exactly that pole. Returns None on any
/// error so the caller falls back to the single-threaded bulk path (raw
/// store on its failure — same safety ladder as bulk_compress).
fn mt_compress(data: &[u8], level: i32, workers: u32) -> Option<Vec<u8>> {
    let mut sink = Vec::with_capacity(data.len() / 3 + 1024);
    let mut enc = zstd::stream::Encoder::new(&mut sink, level).ok()?;
    enc.multithread(workers).ok()?;
    // pledged size lets zstd size the job grid up front (deterministic
    // output, no resync passes); a fresh Encoder per call is fine here —
    // the MT path only fires on multi-megabyte streams where the frame
    // itself costs seconds and the pool setup is noise.
    enc.set_pledged_src_size(Some(data.len() as u64)).ok()?;
    std::io::Write::write_all(&mut enc, data).ok()?;
    enc.finish().ok()?;
    Some(sink)
}

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
    /// v0.9.44 pristine-chunk elision (OPT-IN, region stores only): the
    /// FIRST-EVER write of a key becomes a regeneration CONTRACT instead of
    /// stored content — recipe mode 7 carries ONLY the payload hash (zero
    /// blobs, 17 B on disk) and read_chunk returns None, which the shim's
    /// vanilla fallback turns into ReadResult::NO_DATA → Paper regenerates
    /// the chunk from the seed (the exact semantics of a missing chunk).
    /// Safety: any later write of DIFFERENT content flows through the
    /// normal pipeline and supersedes the contract (flip-on-mismatch);
    /// an identical-content re-save hits the pristine fast path (O(1)).
    /// Residual risk (documented, accepted by the operator who opts in):
    /// mutations between chunk generation and its first-ever durable save
    /// are captured IN the contract hash and get reverted on regeneration —
    /// the same semantics the Bedrock Editor chunk-reset / community
    /// chunk-regen flows accept. CSAVES_PRISTINE=1 opts in.
    pub pristine: bool,
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
    /// GLOB→canonical section conversion cache budget (bytes). Conversions
    /// are pure functions of immutable content-addressed member bytes, so
    /// each member decodes once per process instead of once per read.
    pub canon_cache: u64,
    /// Writer segment roll threshold. 64 MiB in production; tests shrink it
    /// to force rolls while the append buffer is guaranteed non-empty (the
    /// exact window of the roll-hole corruption this once caused).
    pub seg_roll: u64,
    /// Pack-pass zstd-MT admission threshold (bytes). Variant streams at
    /// least this big compress with nbWorkers inside ONE zstd frame; the
    /// pack wall is the longest single-stream compression, so this divides
    /// the pole no per-stream pool can touch. 0 disables.
    pub pack_mt_min: usize,
    /// Explicit worker count for pack-MT frames. 0 = auto: the cores
    /// concentrate on the k = cores/2 longest big streams (cores/k workers
    /// each, total budget == cores). < 2 keeps the single-threaded bulk
    /// path.
    pub pack_mt_workers: usize,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            // hot tier = write path. Level 3 measured 2.1x faster than 12 on
            // the real JJThunder corpus (import 34.8 vs 16.8 MB/s, write
            // p99.9 10ms vs 30ms) with IDENTICAL steady-state size — the
            // background pack pass recompresses everything at the cold tier
            // anyway, so the hot level only bounds transient disk usage.
            // v0.9.24: the same argument pushes all the way into the zstd
            // fast levels (-3 = 3x faster-than-1): back-to-back corpus runs
            // measured import 96.5 (L3) -> 109.1 MB/s (L-3), ensure stage
            // 79.2 -> 50.1 us/chunk (-37%), p50 75.6 -> 65.8 us — and the
            // DRAINED container is byte-identical (x3.36/x7.51, verify
            // 946/946), because the pack pass decompresses hot slabs and
            // re-compresses at cold_level from the same content. Fast -5/-7
            // bottom the ensure stage out at the same ~50 us (zstd is no
            // longer the term) without further import gains, so -3 keeps the
            // best speed at the smallest transient size of the winners.
            hot_level: -3,
            // v0.9.27 sweep on the JJ corpus (MT off, warmup-first): cold 19
            // pack 8.39 s comp 7.62 s container 1.66 MiB (x7.51) / cold 15
            // 2.53 s comp 1.77 s 1.86 MiB (x6.70) / cold 12 1.23 s comp
            // 0.52 s 1.88 MiB (x6.62). z12 = 6.8x pack at +13% container on
            // the noisiest corpus; the size gates did not move (survival
            // x4.01 identical, bench_size compact x0.08 identical, jj
            // x3.36 at +0.6%). Pack is the background drain rate competing
            // with tick threads for cores — 6.8x less maintenance CPU is
            // the point. CSAVES_COLD_LEVEL=19 for the size-obsessed.
            // v0.9.29 re-sweep AFTER the palette memo cut the build phase
            // 4x (build no longer hides comp): cold 12 pack 0.79 s comp
            // 538 ms container 1.88 MiB (x6.61) / cold 8 0.56 s comp
            // 297 ms 1.89 MiB (x6.59) / cold 6 0.45 s comp 213 ms 1.94 MiB
            // (x6.42) / cold 4 0.29 s comp 60 ms 2.15 MiB (x5.81). z6 =
            // -43% pack for +3% container with every size gate byte-flat
            // (bench_size compact x0.10 unchanged, jj TOTAL unchanged,
            // verify 946/946); below z6 the wall is the BUILD phase again
            // (z4: comp 60 ms vs build 158 ms), so the extra -16% pack at
            // v0.9.33 dropped cold to z4 (pack 0.30 s vs 0.44 s, -32%) on
            // the strength of the local compact gate (x0.10 -> x0.11 vs
            // 0.15). CI verdict: the JJThunder corpora disagree — z4 pushed
            // survival 3.13x -> 2.50x (floor 3.0) and jj 2.86x -> 2.59x
            // (floor 2.6). z5 interpolates below the survival floor too;
            // z6 is the fastest level that clears both gates with real
            // margin. The local dim corpus is GLOB-flat and hides this —
            // survival/jj section data is where cold level actually bites.
            cold_level: 6,
            recipe_level: 6,
            pristine: false,
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
            canon_cache: 16 * 1024 * 1024,
            seg_roll: 64 * 1024 * 1024,
            pack_mt_min: 1024 * 1024,
            pack_mt_workers: 0,
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
            hot_level: env_parse("CSAVES_HOT_LEVEL", -3),
            cold_level: env_parse("CSAVES_COLD_LEVEL", 6),
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
            canon_cache: env_parse("CSAVES_CANON_CACHE", 16 * 1024 * 1024),
            seg_roll: env_parse("CSAVES_SEG_ROLL", 64 * 1024 * 1024),
            pack_mt_min: env_parse("CSAVES_PACK_MT_MIN", 1024 * 1024),
            pack_mt_workers: env_parse("CSAVES_PACK_MT_WORKERS", 0),
            pristine: std::env::var("CSAVES_PRISTINE")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(false),
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
    /// Set when a full-rewrite pack had to skip unreadable victims: the
    /// segment is then NEVER removed (removal would turn those victims into
    /// permanent dangling references — the 23,380-chunk bigworld loss class).
    /// Compaction stops considering poisoned segments.
    pack_broken: AtomicBool,
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
    /// Mode 7 (v0.9.44): pristine regeneration contract — the payload hash
    /// of a first-ever write under cfg.pristine. ZERO blobs (blob_hashes
    /// stays empty, so chunk_intact is trivially true), read_chunk returns
    /// None (shim → vanilla fallback → NO_DATA → Paper regenerates). Any
    /// different-content write supersedes the contract through the normal
    /// pipeline (flip-on-mismatch). Never coexists with whole/sections/tail
    /// by construction.
    pristine: Option<Hash>,
}

fn crc_of(b: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(b);
    c.finalize()
}

impl Recipe {
    /// Parse any recipe mode. `id2h` resolves BLOB2 ids (pass-1 complete);
    /// `keytab` resolves mode-5 interned key ids.
    fn parse<S: std::hash::BuildHasher>(
        b: &[u8],
        id2h: &HashMap<u32, Hash, S>,
        keytab: &[String],
    ) -> Option<Recipe> {
        const EMPTY: Recipe = Recipe {
            whole: None,
            sections: Vec::new(),
            tail: Tail::None,
            fields: Vec::new(),
            pos_from_key: false,
            pristine: None,
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
            7 => {
                // v0.9.44 pristine regeneration contract: [7][hash 16B].
                // Nothing stored — the seed regenerates the content.
                if b.len() < 17 {
                    return None;
                }
                let h = ((rd8(&mut p) as u128) << 64) | rd8(&mut p) as u128;
                Some(Recipe { pristine: Some(h), ..EMPTY })
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
        // mode 7 first: the contract is always the smallest form (17 B) and
        // must never silently degrade into content storage.
        if let Some(h) = self.pristine {
            v.push(7u8);
            v.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
            v.extend_from_slice(&(h as u64).to_le_bytes());
            return v;
        }
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

/// GLOB→canonical section conversion results, keyed by the member blob's
/// content hash. Stored member bytes are immutable (content-addressed) and
/// the state registry is append-only, so a conversion is a pure function:
/// decode once per member per process, not once per read (39 us x 3.7
/// members x every chunk read was 92% of warm sectload).
struct CanonCache {
    map: HashMap<u128, Arc<Vec<u8>>>,
    order: VecDeque<(u128, u64)>,
    total: u64,
    cap: u64,
}
impl CanonCache {
    fn new(cap: u64) -> Self {
        CanonCache { map: HashMap::new(), order: VecDeque::new(), total: 0, cap }
    }
    fn get(&self, k: &u128) -> Option<Arc<Vec<u8>>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: u128, v: Arc<Vec<u8>>) {
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
static ST_GLOB_CONV_FAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ST_GLOB_SLABS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// pack-phase census (diagnostic): cumulative ns in each pack sub-stage
pub static PK_VICTIMS_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_BUILD_NS: AtomicU64 = AtomicU64::new(0);
// v0.9.33 build sub-census: where the serial Phase A wall goes —
// bglob = the GLOB variant block (registry lock + section_to_glob +
// glob stream build), btab = the entry-table + raw-concatenation build()
// closure calls (canon + glob)
pub static PK_B_GLOB_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_B_TAB_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_COMP_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_APPEND_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_RECIPES_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_READBLOB_NS: AtomicU64 = AtomicU64::new(0);
// zstd-MT sub-census: how many frames went intra-frame multi-threaded,
// their cumulative compress ns and uncompressed bytes
pub static PK_MT_JOBS: AtomicU64 = AtomicU64::new(0);
pub static PK_MT_NS: AtomicU64 = AtomicU64::new(0);
pub static PK_MT_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn pack_census_json() -> String {
    format!(
        "{{victims:{},build:{},comp:{},append:{},recipes:{},readblob:{},mtjobs:{},mtns:{},mtbytes:{},bglob:{},btab:{}}}",
        PK_VICTIMS_NS.load(Ordering::Relaxed),
        PK_BUILD_NS.load(Ordering::Relaxed),
        PK_COMP_NS.load(Ordering::Relaxed),
        PK_APPEND_NS.load(Ordering::Relaxed),
        PK_RECIPES_NS.load(Ordering::Relaxed),
        PK_READBLOB_NS.load(Ordering::Relaxed),
        PK_MT_JOBS.load(Ordering::Relaxed),
        PK_MT_NS.load(Ordering::Relaxed),
        PK_MT_BYTES.load(Ordering::Relaxed),
        PK_B_GLOB_NS.load(Ordering::Relaxed),
        PK_B_TAB_NS.load(Ordering::Relaxed),
    )
}

pub fn pack_census_reset() {
    for st in [&PK_VICTIMS_NS, &PK_BUILD_NS, &PK_COMP_NS, &PK_APPEND_NS, &PK_RECIPES_NS, &PK_READBLOB_NS, &PK_MT_JOBS, &PK_MT_NS, &PK_MT_BYTES, &PK_B_GLOB_NS, &PK_B_TAB_NS] {
        st.store(0, Ordering::Relaxed);
    }
}
/// Unchanged-chunk autosave skips (hash match against the last write).
pub static ST_WRITE_SKIP: AtomicU64 = AtomicU64::new(0);
pub static ST_TAIL: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_SECTLOAD: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_JOIN_FUSED: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_JOIN_LEGACY: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_PARSE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
// read-path diagnostics: slab cache hit/miss (miss = full frame decompress),
// decompress ns+bytes, GLOB member conversion ns+count, shift resolve ns
pub static ST_SLAB_HIT: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_SLAB_MISS: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ST_DECOMP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static ST_GLOB_CONV: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
static ST_SHIFT_RES: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
// write path
pub static WST_SPLIT: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static WST_ENS: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static WST_RECIPE: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
// ensure decomposition (ensx): where the ensure stage ns actually goes
pub static ENSX_MAP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_SLAB: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_TBLOB: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_TINL: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_PERB: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_DSMP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_SBUILD: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_SCOMP: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
pub static ENSX_SAPPND: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));
// MT lock census: cumulative ns spent INSIDE each exclusive section across
// ALL threads. In MT, cumulative-inside ~= serialized wall contributed by
// that section (threads cannot overlap inside an exclusive guard).
pub static MT_APPEND_NS: AtomicU64 = AtomicU64::new(0);
pub static MT_SLABINS_NS: AtomicU64 = AtomicU64::new(0);
pub static MT_BUMP_NS: AtomicU64 = AtomicU64::new(0);
pub static MT_DROP_NS: AtomicU64 = AtomicU64::new(0);
pub static MT_LASTW_NS: AtomicU64 = AtomicU64::new(0);
pub static MT_BLOBSR_NS: AtomicU64 = AtomicU64::new(0);

// v0.9.35: tests must NOT set CSAVES_FORCE_GLOB via std::env::set_var —
// the pack path resolves it into a OnceLock (setenv races the resolution
// and other tests' getenv reads; UB). The override is consulted only under
// cfg!(test), so release builds pay a single always-false branch.
pub static FORCE_GLOB_TEST: (AtomicBool, AtomicU64) = (AtomicBool::new(false), AtomicU64::new(0));
#[cfg(test)]
pub(crate) fn set_force_glob_for_tests(v: Option<bool>) {
    match v {
        Some(v) => {
            FORCE_GLOB_TEST.1.store(v as u64, Ordering::Relaxed);
            FORCE_GLOB_TEST.0.store(true, Ordering::Relaxed);
        }
        None => FORCE_GLOB_TEST.0.store(false, Ordering::Relaxed),
    }
}

pub fn mt_lock_census_json() -> String {
    format!(
        "{{append:{},slabins:{},bump:{},drop:{},lastw:{},blobsr:{}}}",
        MT_APPEND_NS.load(Ordering::Relaxed),
        MT_SLABINS_NS.load(Ordering::Relaxed),
        MT_BUMP_NS.load(Ordering::Relaxed),
        MT_DROP_NS.load(Ordering::Relaxed),
        MT_LASTW_NS.load(Ordering::Relaxed),
        MT_BLOBSR_NS.load(Ordering::Relaxed),
    )
}

pub fn mt_lock_census_reset() {
    for st in [&MT_APPEND_NS, &MT_SLABINS_NS, &MT_BUMP_NS, &MT_DROP_NS, &MT_LASTW_NS, &MT_BLOBSR_NS] {
        st.store(0, Ordering::Relaxed);
    }
}

pub fn write_stage_reset() {
    for st in [&WST_SPLIT, &WST_ENS, &WST_RECIPE, &ENSX_MAP, &ENSX_SLAB, &ENSX_TBLOB, &ENSX_TINL, &ENSX_PERB, &ENSX_DSMP, &ENSX_SBUILD, &ENSX_SCOMP, &ENSX_SAPPND] {
        st.0.store(0, Ordering::Relaxed);
        st.1.store(0, Ordering::Relaxed);
    }
}

/// ensx: ensure-stage decomposition (ns/call per block).
pub fn ensure_stage_json() -> String {
    let fmt = |name: &str, st: &(AtomicU64, AtomicU64)| {
        let (ns, n) = (st.0.load(Ordering::Relaxed), st.1.load(Ordering::Relaxed));
        format!("\"{}\":{{ns:{},n:{},per_call:{}}}", name, ns, n, if n > 0 { ns / n } else { 0 })
    };
    format!(
        "{{{},{},{},{},{},{},{},{},{}}}",
        fmt("slab", &ENSX_SLAB),
        fmt("map", &ENSX_MAP),
        fmt("tail_blob", &ENSX_TBLOB),
        fmt("tail_inline", &ENSX_TINL),
        fmt("per_blob", &ENSX_PERB),
        fmt("dict_sample", &ENSX_DSMP),
        fmt("slab_build", &ENSX_SBUILD),
        fmt("slab_comp", &ENSX_SCOMP),
        fmt("slab_append", &ENSX_SAPPND),
    )
}

/// Human/CI-readable stage snapshot: ns/call per stage.
pub fn read_stage_json() -> String {
    let fmt = |name: &str, st: &(AtomicU64, AtomicU64)| {
        let (ns, n) = (st.0.load(Ordering::Relaxed), st.1.load(Ordering::Relaxed));
        format!("\"{}\":{{ns:{},n:{},per_call:{}}}", name, ns, n, if n > 0 { ns / n } else { 0 })
    };
    format!(
        "{{{},{},{},{},{},{},{},{},{},{},{}}}",
        fmt("recipe", &ST_RECIPE),
        fmt("tail", &ST_TAIL),
        fmt("sectload", &ST_SECTLOAD),
        fmt("join_fused", &ST_JOIN_FUSED),
        fmt("join_legacy", &ST_JOIN_LEGACY),
        fmt("fastnbt_parse", &ST_PARSE),
        fmt("slab_hit", &ST_SLAB_HIT),
        fmt("slab_miss", &ST_SLAB_MISS),
        fmt("decomp", &ST_DECOMP),
        fmt("glob_conv", &ST_GLOB_CONV),
        fmt("shift_res", &ST_SHIFT_RES),
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
    for st in [&ST_RECIPE, &ST_TAIL, &ST_SECTLOAD, &ST_JOIN_FUSED, &ST_JOIN_LEGACY, &ST_PARSE, &ST_SLAB_HIT, &ST_SLAB_MISS, &ST_DECOMP, &ST_GLOB_CONV, &ST_SHIFT_RES] {
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
    blobs: RwLock<HashMap<Hash, BlobLoc, RawBuild>>,
    ids: RwLock<HashMap<u32, Hash, RawBuild>>,
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
    canon_cache: Mutex<CanonCache>,
    cur_dict: AtomicU8,
    next_dict_ver: AtomicU8,
    /// v0.9.30: content-hash bucket representatives for dictionary
    /// training. The old Vec pool froze at the first 2*dict_train
    /// arrivals — which payloads arrive first depends on fs::read_dir
    /// order (bench) or player spawn direction (production), and a
    /// uniform startup region then starves the trainer into a degenerate
    /// dictionary (measured 1662-7773 B installs vs 83-100 KB from the
    /// same corpora with full coverage). Buckets key on the FULL 64-bit
    /// content hash; each keeps its LARGEST member (arrival-independent).
    /// Training iterates bucket representatives in hash order —
    /// deterministic given the sample content set.
    dict_samples: Mutex<HashMap<u64, Vec<u8>>>,
    dict_enabled: AtomicBool,
    /// bucket count at the last successful training; retrain only after
    /// +25% coverage growth (same content would retrain an identical dict)
    dict_last_train_n: AtomicUsize,
    /// new-or-grown buckets since the last training — a generation of
    /// fresh material (>= dict_train/4) also authorizes a retrain
    dict_fresh: AtomicUsize,

    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub dedup_hits: AtomicU64,
    /// per-store write-skip count: the global ST_WRITE_SKIP is shared by
    /// every Store in the process, so parallel tests cannot assert on it
    pub write_skips: AtomicU64,
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
    /// Global block-state registry for the 0xCB GLOB slab form (jj_exp5:
    /// x2.16 on the like-for-like dense content of the vanilla-terrain
    /// bigworld). Append-only, monotonic gids; persisted in states-1.bin
    /// BEFORE any slab that introduced new states lands on disk (a missing
    /// registry entry after a crash would misresolve gids silently).
    glob_reg: Mutex<chunknbt::StateRegistry>,
    reg_dirty: AtomicBool,
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
            blobs: RwLock::new(HashMap::with_hasher(RawBuild)),
            ids: RwLock::new(HashMap::with_hasher(RawBuild)),
            chunks: RwLock::new(HashMap::new()),
            keys_tab: RwLock::new(Vec::new()),
            key_refcnt: RwLock::new(HashMap::new()),
            recipe_batch: Mutex::new(RecipeBatch::default()),
            batch_idx: Mutex::new(HashMap::new()),
            writer: Mutex::new(None),
            slab_cache: Mutex::new(SlabCache::new(32 * 1024 * 1024)),
            canon_cache: Mutex::new(CanonCache::new(16 * 1024 * 1024)),
            next_seg: AtomicU64::new(0),
            next_blob_id: AtomicU64::new(1),
            dicts: RwLock::new(HashMap::new()),
            cur_dict: AtomicU8::new(0),
            next_dict_ver: AtomicU8::new(1),
            dict_samples: Mutex::new(HashMap::new()),
            dict_enabled: AtomicBool::new(dict_enabled),
            dict_last_train_n: AtomicUsize::new(0),
            dict_fresh: AtomicUsize::new(0),
            writes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
            write_skips: AtomicU64::new(0),
            disk_written: AtomicU64::new(0),
            logical_bytes: AtomicU64::new(0),
            whole_mode: AtomicBool::new(false),
            pack_requested: AtomicBool::new(false),
            pack_watermark: AtomicU64::new(0),
            last_written: Mutex::new(HashMap::new()),
            probe_cache: Mutex::new(HashMap::new()),
            glob_reg: Mutex::new(chunknbt::StateRegistry::new()),
            reg_dirty: AtomicBool::new(false),
        });
        *st.slab_cache.lock() = SlabCache::new(st.cfg.slab_cache);
        *st.canon_cache.lock() = CanonCache::new(st.cfg.canon_cache);
        st.load_dicts()?;
        st.load_glob_reg();
        st.scan()?;
        Ok(st)
    }

    // ---- global state registry (0xCB GLOB slabs) ----

    fn states_path(&self) -> PathBuf {
        self.dir.join("states-1.bin")
    }

    fn load_glob_reg(&self) {
        let Ok(bytes) = fs::read(self.states_path()) else { return };
        match chunknbt::StateRegistry::from_bytes(&bytes) {
            Some(r) => *self.glob_reg.lock() = r,
            None => eprintln!("[c-saves] states-1.bin unparseable — starting an empty registry (old GLOB slabs would misresolve; re-import needed)"),
        }
    }

    /// Persist the registry atomically (tmp + rename). Called BEFORE any
    /// slab that introduced new gids is appended, and on sync/seal.
    fn persist_glob_reg(&self) -> std::io::Result<()> {
        let bytes = self.glob_reg.lock().to_bytes();
        let tmp = self.dir.join("states-1.tmp");
        {
            use std::io::Write as _;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_data()?;
        }
        fs::rename(&tmp, self.states_path())
    }

    /// Number of slabs stored in the 0xCB GLOB form (probe/statistic).
    pub fn glob_slab_count() -> u64 {
        ST_GLOB_SLABS.load(Ordering::Relaxed)
    }

    /// GLOB conversion admission: section stores only (whole-mode payloads
    /// are not CSEC sections) and a registry small enough for the 1-byte
    /// gid stream. ON by default: the jj_exp5 entropy measurements put the
    /// global-id form at x2.16 on vanilla-terrain dense streams, and the
    /// natural per-slab size choice measured x2.99 -> x3.08 on the real JJ
    /// corpus. The former natural-choice corruption was the glob_to_section
    /// palette-order divergence (shift-ref permutations against rebuilt
    /// targets) — fixed and CI-regressed by
    /// glob_rebuild_keeps_shiftref_target_order. CSAVES_GLOB=0 opts out
    /// (emergency switch / A-B benches).
    fn glob_enabled(&self) -> bool {
        !self.whole_mode.load(Ordering::Relaxed)
            && std::env::var("CSAVES_GLOB").as_deref() != Ok("0")
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
    /// v0.9.30: content-hash bucket reservoir. Every raw lands in its
    /// full-64-bit-hash bucket; identical content collapses into ONE
    /// bucket, so uniform sections cannot flood the pool and tiny RLE
    /// forms of homogeneous columns are admitted like any other raw (the
    /// old Vec pool counted raw ARRIVALS, which is why homogeneous
    /// regions could starve it). Each
    /// bucket keeps its LARGEST member (arrival-independent), so the
    /// pool converges to a content-determined representative set as the
    /// store fills, instead of freezing on whichever 400 payloads
    /// happened to arrive first (the old behavior trained 1662-7773 B
    /// degenerate dictionaries on this corpus after a sandbox recycle,
    /// vs 83-100 KB from full coverage).
    fn dict_sample(&self, raw: &[u8]) {
        if !self.dict_enabled.load(Ordering::Relaxed) {
            return;
        }
        let n = raw.len().min(16 * 1024);
        // hash the TRUNCATED bytes (what gets stored/trained): the bucket
        // key then depends only on the trained representation, and the
        // train-time hash-order iteration reproduces the offline-measured
        // order (83 KB dict vs 49 KB when hashing the untruncated raw).
        // v0.9.40: xxh3_64 replaces SipHash — DefaultHasher ran ~1.5 GB/s,
        // which was the entire 17.6 us/frame dict_sample cost; xxh3 covers
        // the same truncated bytes at an order of magnitude more. Bucket
        // keys shift (dict content moves within its determinism contract:
        // same content set -> same dict); member raws, enc_fp and the
        // drained container are untouched.
        let h = xxhash_rust::xxh3::xxh3_64(&raw[..n]);
        let mut s = self.dict_samples.lock();
        // memory bound: 16 x dict_train buckets (~13 MB typical, 51 MB
        // worst case at 16 KiB samples). Beyond the cap the set is frozen:
        // any 3200-bucket representative subset is far more coverage than
        // the old 400-arrival pool ever had.
        if s.len() >= self.cfg.dict_train.max(1) * 16 && !s.contains_key(&h) {
            return;
        }
        match s.entry(h) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(raw[..n].to_vec());
                self.dict_fresh.fetch_add(1, Ordering::Relaxed);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if raw.len() > e.get().len() {
                    e.insert(raw[..n].to_vec());
                    self.dict_fresh.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Train a fresh shared dictionary from collected samples (if enough).
    /// Called from the maintenance thread (NOT the write path — training
    /// costs CPU). Returns true when a new dictionary version was installed.
    ///
    /// v0.9.30: trains over ALL bucket representatives in ascending hash
    /// order — deterministic given the content set, no arrival dependence,
    /// no truncation to a lucky subset (offline study on the JJ stream:
    /// subset selections of 200 swung the dictionary 4.6-53 KB for the
    /// same material, while the full 982-sample hash-ordered stream gave
    /// a stable 83 KB). Retrains only after +25% bucket growth, since
    /// identical coverage would reproduce an identical dictionary.
    pub fn train_dict_if_needed(&self) -> bool {
        if !self.dict_enabled.load(Ordering::Relaxed) {
            return false;
        }
        // v0.9.41: stop at the 253 line — 0xFE is the LZMA2 slab sentinel
        // and must never be minted as a dict version (see SLAB_CODEC_LZMA2).
        if self.next_dict_ver.load(Ordering::Relaxed) >= 254 {
            return false; // version space exhausted
        }
        let refs: Vec<Vec<u8>> = {
            let s = self.dict_samples.lock();
            let n = s.len();
            if n < self.cfg.dict_train.max(1) {
                return false;
            }
            let last = self.dict_last_train_n.load(Ordering::Relaxed);
            let fresh = self.dict_fresh.load(Ordering::Relaxed);
            if last > 0
                && fresh * 4 < self.cfg.dict_train.max(1)
                && n * 4 < last * 5
            {
                // neither a fresh generation of material nor +25% total
                // coverage: the same content would retrain the same dict
                return false;
            }
            let mut keyed: Vec<(u64, &Vec<u8>)> =
                s.iter().map(|(k, v)| (*k, v)).collect();
            keyed.sort_by_key(|(k, _)| *k);
            // trainer-input bound: the COVER optimizer's time scales with
            // sample count x bytes (worse than linearly), and it runs
            // inside compact_step — on the pack wall. Cut the hash-ordered
            // (== uniform random w.r.t. content: the diversity the arrival
            // pool could never guarantee) prefix at dict_train/2 samples
            // and 256 KiB — measured 74 ms at 200x259 KiB vs ~25 ms for
            // the old arrival pool, which the drain wall cannot afford.
            // Deterministic given the bucket set.
            let mut acc = 0usize;
            let n_cap = 32.max(self.cfg.dict_train.max(2) / 8);
            let cut = keyed
                .iter()
                .take(n_cap)
                .position(|(_, v)| {
                    acc += v.len();
                    acc >= 256 * 1024
                })
                .map(|p| p + 1)
                .unwrap_or(n_cap);
            keyed.truncate(cut.max(32.min(keyed.len())));
            self.dict_last_train_n.store(n, Ordering::Relaxed);
            keyed.into_iter().map(|(_, v)| v.clone()).collect()
        };
        let ref_slices: Vec<&[u8]> = refs.iter().map(|v| v.as_slice()).collect();
        let in_bytes: usize = ref_slices.iter().map(|v| v.len()).sum();
        let tt = std::time::Instant::now();
        let Ok(d) = zstd::dict::from_samples(&ref_slices, self.cfg.dict_max) else {
            return false;
        };
        let train_ms = tt.elapsed().as_millis();
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
        self.dict_fresh.store(0, Ordering::Relaxed);
        eprintln!(
            "[c-saves] trained shared dictionary v{v} ({} bytes) in {train_ms} ms from {} samples / {in_bytes} B",
            {
                let d = self.dicts.read();
                d.get(&v).map(|x| x.len()).unwrap_or(0)
            },
            ref_slices.len()
        );
        true
    }

    fn gc_dicts(&self) {
        // RAM-cache trim ONLY: dict FILES are never deleted. Compressed data
        // outside the blobs map also references dict versions — kind-2 recipe
        // inline tails (Tail::Inline{dict,..}) — which blob-only accounting
        // cannot see. Deleting the file made every such chunk permanently
        // unreadable (the 11,819-missing bigworld-survival loss). decompress()
        // lazy-loads the file back on a cache miss, so the RAM map is a pure
        // cache and the disk copies (<= ~112 KiB per version) are the truth.
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
        }
    }

    /// Lazy (re)load a dictionary version from its on-disk file into the RAM
    /// cache. Called from decompress() on a cache miss so reads survive any
    /// trim/GC of the RAM map (files are never deleted — see gc_dicts). Also
    /// covers stores reopened after a crash where the open-time loader
    /// skipped a small/short file: the file is the source of truth.
    fn load_dict_from_disk(&self, v: u8) -> Option<Arc<Vec<u8>>> {
        if v == 0 {
            return None;
        }
        let d = fs::read(self.dict_dir().join(format!("dict-{v}.bin"))).ok()?;
        if d.len() < 512 {
            return None;
        }
        let a = Arc::new(d);
        let mut dicts = self.dicts.write();
        dicts.entry(v).or_insert_with(|| Arc::clone(&a));
        Some(a)
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
                pack_broken: AtomicBool::new(false),
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
                                                    cold: body[0] & SLAB_KIND_HOT == 0,
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
                        k if k & 0x3f == 8 => {
                            // SLAB v2: same header layout; entries are varint
                            // zigzag id-delta + varint len + 16B hash, raws after.
                            // GLOB slabs (kind | 0x80) carry one extra header
                            // byte (reg_w); the member raws are 0xCB global-id
                            // sections, converted back by the read paths.
                            if body.len() < 12 {
                                false
                            } else {
                                let dict_id = body[1];
                                let cl = u32::from_le_bytes(body[2..6].try_into().unwrap()) as usize;
                                let raw_total = u32::from_le_bytes(body[6..10].try_into().unwrap());
                                let n = u16::from_le_bytes(body[10..12].try_into().unwrap()) as usize;
                                let ghdr = if body[0] & 0x80 != 0 { 13usize } else { 12usize };
                                if body.len() < ghdr + cl || n == 0 {
                                    false
                                } else {
                                    let body_start = off + 8;
                                    let share = ((8 + blen) as u64 / n as u64).min(u32::MAX as u64) as u32;
                                    let last_share = (8 + blen) as u64 - share as u64 * (n as u64 - 1) as u64;
                                    match self.parse_slab_v2(dict_id, &body[ghdr..ghdr + cl], raw_total, n) {
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
                                                    cold: body[0] & SLAB_KIND_HOT == 0,
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
        let (boff, soff, rl) = self.batch_idx.lock().get(&key).copied()?;
        self.load_batched(seg, boff, soff, rl)
    }

    /// Load a recipe out of a compressed kind-9 batch at an exact location
    /// (v0.9.35: split from recipe_at so callers that captured a location —
    /// supersede ref-retirement — can resolve it WITHOUT re-consulting
    /// batch_idx, which a concurrent flush may have re-pointed in between).
    fn load_batched(&self, seg: u32, boff: u64, soff: u32, rl: u32) -> Option<Recipe> {
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
        let _t = std::time::Instant::now();
        let mut wg = self.writer.lock();
        if wg.is_none() {
            let id = self.next_seg.fetch_add(1, Ordering::SeqCst) as u32;
            let file = self.new_seg(id)?;
            *wg = Some(Writer { file, seg_id: id, written: 0, buf: Vec::with_capacity(BUF_CAP) });
        }
        let w = wg.as_mut().unwrap();
        if w.written > 0 && w.written + body.len() as u64 > self.cfg.seg_roll {
            // The append buffer holds records whose bytes were already counted
            // in `written` but NOT yet on disk; they belong at
            // written - buf.len() (the file position invariant). Rolling with
            // a seek to `written` wrote them a buffer-length TOO FAR, leaving
            // a hole of zeros over the tail records: batch records decoded
            // rawlen=0 (LEAK slice fail) and blobs decompressed to garbage —
            // every pack then skipped them and the segment drop turned them
            // into permanent dangling references (23,380 missing chunks on
            // the 262k-chunk bigworld run; JJ-corpus benches never roll a
            // segment with a non-empty buffer, so they stayed clean).
            w.file
                .seek(SeekFrom::Start(w.written - w.buf.len() as u64))?;
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
        MT_APPEND_NS.fetch_add(_t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok((w.seg_id, body_off, body.len() as u32))
    }

    // ---- compression tiers ----

    /// Explicit-level variant (recipe-batch flush): bulk_compress, no
    /// dictionary; caller picks the zstd level. (The hot-slab caller went
    /// RAW in v0.9.39 — dict_id=0xFF sentinel, see flush_hot_slab_group /
    /// decompress.)
    fn compress_tier_level(&self, data: &[u8], level: i32) -> (u8, Vec<u8>) {
        bulk_compress(data, level)
    }

    fn compress_tier(&self, data: &[u8], cold: bool) -> (u8, Vec<u8>) {
        let level = if cold { self.cfg.cold_level } else { self.cfg.hot_level };
        // v0.9.30: the COLD tier is dict-less, matching the pack path
        // (bulk/mt compress are dict-less too). The shared dictionary
        // earns its keep on the HOT tier — small frequent writes where
        // the cross-payload prior actually bites; on the cold tier the
        // generic dictionary only added framing overhead on content that
        // already compresses well at level 6+ (bench_size compact world
        // measured +6% WITH the dict, byte-identical gate corpus). Blobs
        // carry their own dict_id, so dict-framed cold blobs from older
        // stores still decode exactly as stored.
        let dict = if cold { None } else { self.current_dict() };
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
        // 0xFF sentinel = RAW slab stream (v0.9.39): hot slab frames skip the
        // zstd frame entirely — measured 155-220 us per ~34 KiB frame on the
        // bench host INDEPENDENT of level (-3..3), i.e. pure frame machinery,
        // while the hot tier is transient material the pack pass re-stores at
        // the cold level anyway. The drained container is byte-identical; the
        // sentinel can never collide with a real dict id (dict ids are
        // assigned 0..=254, and slabs never carried a dict before this).
        if dict_id == 0xFF {
            return Some(data.to_vec());
        }
        // v0.9.41: LZMA2 cold slab streams (CSAVES_COLD_CODEC=lzma2). Same
        // wire-byte dispatch as the 0xFF raw sentinel — every read path
        // (slab cache, read_blob, batch loads, scan rebuild, pack re-group,
        // verify) is covered by this one hook with zero call-site changes.
        if dict_id == SLAB_CODEC_LZMA2 {
            return lzma2_decompress(data, raw_len as usize);
        }
        DEC_C.with(|c| {
            let mut slot = c.borrow_mut();
            let key = (self.uid, dict_id);
            if slot.as_ref().map(|s| s.key) != Some(key) {
                // rebuild ONLY on (store, dict) change: the per-call dicts
                // read-lock went away (was hit on every decompressed blob)
                let dec = if dict_id == 0 {
                    build_dec(None)
                } else {
                    // scope the read guard BEFORE load_dict_from_disk: the
                    // match temporary would otherwise hold dicts.read()
                    // across the dicts.write() inside the lazy reload ->
                    // same-thread read->write self-deadlock
                    let cached = self.dicts.read().get(&dict_id).map(Arc::clone);
                    let dict = match cached {
                        Some(d) => d,
                        None => self.load_dict_from_disk(dict_id)?, // lazy reload
                    };
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

    /// Import-path batching: ONE hot-tier slab frame instead of one zstd
    /// frame per section. The per-section bulk zstd call dominated the
    /// ensure stage (~130 small calls per chunk); a single frame per chunk
    /// removes the per-call overhead AND hands zstd the whole-chunk window
    /// (cross-section matches inside one chunk compress immediately, before
    /// the pack pass ever sees the bytes).
    ///
    /// Wire format = SLAB V2 (kind 8) + the 0x40 hot-tier bit. No GLOB
    /// variant here: the write path stays lean, the cold re-pack re-decides
    /// the form with a 4-8 MiB window (the same end state the individual
    /// pipeline produced). Dedup stays content-pure: map hits and in-batch
    /// duplicates never enter the frame.
    fn ensure_blobs_batch<'a>(&self, items: &[(Hash, &'a [u8])]) -> Result<(), String> {
        let _tm = std::time::Instant::now();
        let mut misses: Vec<(Hash, u32, &'a [u8])> = Vec::with_capacity(items.len());
        let mut seen: std::collections::HashSet<Hash, RawBuild> =
            std::collections::HashSet::with_capacity_and_hasher(items.len(), RawBuild);
        {
            let blobs = self.blobs.read();
            for (h, raw) in items {
                if blobs.contains_key(h) {
                    self.dedup_hits.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if !seen.insert(*h) {
                    continue; // in-batch duplicate of an earlier miss
                }
                misses.push((*h, 0, raw));
            }
        }
        ENSX_MAP.0.fetch_add(_tm.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_MAP.1.fetch_add(1, Ordering::Relaxed);
        let raw_total: usize = misses.iter().map(|m| m.2.len()).sum();
        if misses.len() < HOT_SLAB_MIN_SECS || raw_total < HOT_SLAB_MIN_RAW {
            let _t = std::time::Instant::now();
            let r = (|| -> Result<(), String> {
            for (h, _, raw) in &misses {
                self.ensure_blob(*h, raw, false)?;
            }
            Ok(())
            })();
            ENSX_PERB.0.fetch_add(_t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            ENSX_PERB.1.fetch_add(1, Ordering::Relaxed);
            r?;
            return Ok(());
        }
        for m in &mut misses {
            m.1 = self.next_blob_id.fetch_add(1, Ordering::Relaxed) as u32;
        }
        let _td = std::time::Instant::now();
        for (_, _, raw) in &misses {
            self.dict_sample(raw);
        }
        ENSX_DSMP.0.fetch_add(_td.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_DSMP.1.fetch_add(1, Ordering::Relaxed);
        // split into slab-sized groups (same cap rule as the pack path)
        let slab_cap = if self.whole_mode.load(Ordering::Relaxed) {
            self.cfg.slab_max_raw_whole
        } else {
            self.cfg.slab_max_raw
        };
        let mut group: Vec<(Hash, u32, &[u8])> = Vec::new();
        let mut group_raw = 24usize; // entry-table floor (pack-path rule)
        for m in misses {
            let need = 24 + m.2.len();
            if !group.is_empty() && group_raw + need > slab_cap {
                self.flush_hot_slab_group(std::mem::take(&mut group))?;
                group_raw = 24;
            }
            group_raw += need;
            group.push(m);
        }
        if !group.is_empty() {
            self.flush_hot_slab_group(group)?;
        }
        Ok(())
    }

    /// Write one hot-tier slab frame: SLAB V2 stream (varint zigzag
    /// id-delta + varint len + 16B hash per member, raws concatenated),
    /// stored RAW since v0.9.39 (dict_id=0xFF sentinel, no zstd frame): the
    /// hot tier is transient material the pack pass re-stores at the cold
    /// level, and the per-frame zstd machinery measured 155-220 us on a
    /// ~34 KiB frame independent of level — pure overhead on the write path.
    /// NO dictionary, NO GLOB variant (the write path stays lean; the cold
    /// re-pack re-decides the form with a 4-8 MiB window — the drained
    /// container is byte-identical to the compressed-hot pipeline). Fresh
    /// members: no old-location live accounting (their bytes were never on
    /// disk before this record).
    fn flush_hot_slab_group(&self, group: Vec<(Hash, u32, &[u8])>) -> Result<(), String> {
        let _ts = std::time::Instant::now();
        let n = group.len();
        // v0.9.40: the 12-byte header is RESERVED up front and the body is
        // built inside it — the old path assembled [tab][raws] and then
        // splice(0..0) memmove'd the whole ~150 KiB stream to make room for
        // the header. Byte-identical output, one full-frame memmove gone.
        const SLAB_HDR: usize = 12; // non-GLOB slab v2 header: kind,dict,cl,raw,n
        let raws_total: usize = group.iter().map(|g| g.2.len()).sum();
        let mut body: Vec<u8> = Vec::with_capacity(SLAB_HDR + n * 24 + raws_total);
        body.resize(SLAB_HDR, 0);
        let mut prev_id: i64 = 0;
        for (h, id, raw) in &group {
            put_u64(&mut body, zigzag(*id as i64 - prev_id));
            prev_id = *id as i64;
            put_u64(&mut body, raw.len() as u64);
            body.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
            body.extend_from_slice(&(*h as u64).to_le_bytes());
        }
        let tab_len = (body.len() - SLAB_HDR) as u32;
        for (_, _, raw) in &group {
            body.extend_from_slice(raw);
        }
        ENSX_SBUILD.0.fetch_add(_ts.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_SBUILD.1.fetch_add(1, Ordering::Relaxed);
        let _tc = std::time::Instant::now();
        // v0.9.39: RAW stream — no zstd call at all (dict_id=0xFF sentinel;
        // see decompress()). The slab header is patched IN PLACE in front of
        // the stream (saves a full-frame memcpy per flush; the only remaining
        // copy is append's into the segment buffer).
        body[0] = 8u8 | SLAB_KIND_HOT;
        body[1] = 0xFF; // RAW sentinel
        body[2..6].copy_from_slice(&((tab_len as usize + raws_total) as u32).to_le_bytes());
        body[6..10].copy_from_slice(&((tab_len as usize + raws_total) as u32).to_le_bytes());
        body[10..12].copy_from_slice(&(n as u16).to_le_bytes());
        ENSX_SCOMP.0.fetch_add(_tc.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_SCOMP.1.fetch_add(1, Ordering::Relaxed);
        let _ta = std::time::Instant::now();
        let raw_total = (tab_len as usize + raws_total) as u32;
        let dict_id: u8 = 0xFF;
        let blen = body.len() as u64;
        let (seg, off, _) = self.append(&body).map_err(|e| e.to_string())?;
        let share = ((8 + blen) / n as u64).min(u32::MAX as u64) as u32;
        let last_share = (8 + blen) - share as u64 * (n as u64 - 1) as u64;
        let cnt = group.len();
        let _ti = std::time::Instant::now();
        {
            let mut blobs = self.blobs.write();
            // one ids guard for the whole group (was acquired PER MEMBER —
            // 130 lock round-trips per chunk, real MT contention on 2 threads)
            let mut ids = self.ids.write();
            // single pass: slab_off is known at insert time (cursor walks
            // the member raws in group order starting at the table end) —
            // the old code inserted slab_off: 0 and then re-probed every
            // member with blobs.get_mut in a second map walk (~130 hash
            // lookups per chunk for values it already had in hand).
            let mut cursor = tab_len;
            for (i, (h, id, raw)) in group.iter().enumerate() {
                blobs.entry(*h).or_insert(BlobLoc {
                    seg,
                    off,
                    comp_len: raw.len() as u32,
                    raw_len: raw.len() as u32,
                    dict_id,
                    refs: 0,
                    id: *id,
                    slab: true,
                    cold: false, // hot tier: the pack pass may re-group these
                    share: if i + 1 == cnt {
                        last_share.min(u32::MAX as u64) as u32
                    } else {
                        share
                    },
                    slab_off: cursor,
                    slab_raw: raw_total,
                    slab_comp: raw_total, // RAW frame: stream len == compressed len
                });
                ids.insert(*id, *h);
                cursor += raw.len() as u32;
            }
        }
        MT_SLABINS_NS.fetch_add(_ti.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_SAPPND.0.fetch_add(_ta.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_SAPPND.1.fetch_add(1, Ordering::Relaxed);
        ENSX_SLAB.0.fetch_add(_ts.elapsed().as_nanos() as u64, Ordering::Relaxed);
        ENSX_SLAB.1.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn bump_recipe_refs(&self, rec: &Recipe) {
        let _t = std::time::Instant::now();
        let mut blobs = self.blobs.write();
        for h in rec.blob_hashes() {
            if let Some(loc) = blobs.get_mut(&h) {
                loc.refs += 1;
            }
        }
        MT_BUMP_NS.fetch_add(_t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn drop_recipe_refs(&self, rec: &Recipe) {
        let _t = std::time::Instant::now();
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
        MT_DROP_NS.fetch_add(_t.elapsed().as_nanos() as u64, Ordering::Relaxed);
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

    /// DIAGNOSTIC: cross-check every slab entry in the blobs map against the
    /// ON-DISK slab it points at. For each (seg, off) group: parse the slab
    /// header + entry table (the disk ground truth, same code as scan()) and
    /// require (a) the map hash is IN the table and (b) the table's member
    /// offset/len equal the map's slab_off/raw_len. Returns violation lines
    /// (empty = clean). This is the invariant whose violation produced the
    /// "map points into a slab whose entry table lacks the hash" forensics.
    pub fn slab_invariant_scan(&self) -> Vec<String> {
        let mut out = Vec::new();
        // group slab map entries by slab location
        let mut groups: HashMap<(u32, u64), Vec<(Hash, BlobLoc)>> = HashMap::new();
        {
            let blobs = self.blobs.read();
            for (h, l) in blobs.iter() {
                if l.slab {
                    groups.entry((l.seg, l.off)).or_default().push((*h, *l));
                }
            }
        }
        let segs = self.segs.read();
        for ((seg_id, off), members) in groups {
            let Some(seg) = segs.get(&seg_id) else {
                out.push(format!(
                    "slab @({seg_id},{off}): SEGMENT MISSING, {n} map refs",
                    n = members.len()
                ));
                continue;
            };
            let Some(hdr) = seg.read_at(off, 12, false) else {
                out.push(format!("slab @({seg_id},{off}): header unreadable"));
                continue;
            };
            let kind = hdr[0] & 0x3f; // 0x80 = GLOB, 0x40 = hot tier
            if kind != 5 && kind != 8 {
                out.push(format!("slab @({seg_id},{off}): kind {kind} not a slab"));
                continue;
            }
            let dict_id = hdr[1];
            let hlen = if hdr[0] & 0x80 != 0 { 13usize } else { 12usize };
            let cl = u32::from_le_bytes(hdr[2..6].try_into().unwrap()) as usize;
            let raw_total = u32::from_le_bytes(hdr[6..10].try_into().unwrap());
            let n = u16::from_le_bytes(hdr[10..12].try_into().unwrap()) as usize;
            let Some(data) = seg.read_at(off + hlen as u64, cl, false) else {
                out.push(format!("slab @({seg_id},{off}): body unreadable cl={cl}"));
                continue;
            };
            let parsed = if kind == 5 {
                self.parse_slab(dict_id, &data, raw_total, n)
                    .map(|e| (e, n * 24))
            } else {
                self.parse_slab_v2(dict_id, &data, raw_total, n)
            };
            let Some((entries, tab_len)) = parsed else {
                out.push(format!("slab @({seg_id},{off}): ENTRY TABLE UNPARSEABLE n={n}"));
                continue;
            };
            // disk ground truth: hash -> (offset in raw, len)
            let mut disk: HashMap<Hash, (u32, u32)> = HashMap::new();
            let mut cursor = tab_len as u32;
            for (_bid, rl, h) in &entries {
                disk.insert(*h, (cursor, *rl));
                cursor += *rl;
            }
            for (h, l) in &members {
                match disk.get(h) {
                    None => out.push(format!(
                        "slab @({seg_id},{off}): map hash {h:016x} NOT IN entry table (n={n})"
                    )),
                    Some(&(doff, dlen)) => {
                        if l.slab_off != doff || l.raw_len != dlen {
                            out.push(format!(
                                "slab @({seg_id},{off}): hash {h:016x} map (soff={}, raw={}) != disk (soff={doff}, raw={dlen})",
                                l.slab_off, l.raw_len
                            ));
                        }
                    }
                }
            }
        }
        out
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
        self.write_chunk_mode(key, payload, whole_mode, false)
    }

    /// Import-path write: strips derived light (the server relights from
    /// isLightOn=0 — vanilla-standard). Runtime saves MUST use write_chunk:
    /// relit light handed back by Paper is stored, never recomputed again.
    pub fn write_chunk_import(&self, key: u64, payload: &[u8]) -> Result<(), String> {
        self.write_chunk_mode(key, payload, false, true)
    }

    /// Bench/import-model write with an explicit strip decision (models
    /// write_chunk_import while letting benchmarks toggle the mode).
    pub fn write_chunk_stripped(&self, key: u64, payload: &[u8], strip: bool) -> Result<(), String> {
        self.write_chunk_mode(key, payload, false, strip)
    }

    fn write_chunk_mode(
        &self,
        key: u64,
        payload: &[u8],
        whole_mode: bool,
        strip: bool,
    ) -> Result<(), String> {
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
            if std::env::var("CSAVES_NO_SKIP").as_deref() != Ok("1") {
                let _t = std::time::Instant::now();
                let hit = self.last_written.lock().get(&key) == Some(&hash);
                MT_LASTW_NS.fetch_add(_t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                if hit && self.chunk_intact(key) {
                    ST_WRITE_SKIP.fetch_add(1, Ordering::Relaxed);
                    self.write_skips.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                // v0.9.44: pristine contracts stay pristine on identical
                // re-saves even across RESTARTS (last_written is RAM-only,
                // so the match above misses after reopen). The contract
                // hash is the recipe itself — O(1) skip, no flip, no
                // storage: Paper re-serializing the regenerated identical
                // chunk is exactly the content the seed produces.
                if !hit && self.cfg.pristine && self.pristine_hash_of(key) == Some(hash) {
                    ST_WRITE_SKIP.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
        let t0w = std::time::Instant::now();
        // v0.9.44 pristine candidate: FIRST-EVER write of a region-store key
        // under cfg.pristine becomes a regeneration contract instead of
        // stored content. Any later write with different content falls
        // through to the normal pipeline and supersedes the contract via the
        // ordinary batch-push/supersede machinery (flip-on-mismatch); an
        // identical re-save was already caught by the pristine fast path
        // above. Import/strip paths never form contracts (imported content
        // is not seed-reproducible), and whole-mode stores (entities/POI)
        // are excluded — their content is not worldgen-reproducible either.
        let pristine_cand = self.cfg.pristine
            && !whole_mode
            && !strip
            && !self.whole_mode.load(Ordering::Relaxed)
            && !self.chunks.read().contains_key(&key)
            && self.recipe_batch.lock().get(key).is_none();
        let recipe = if pristine_cand {
            Recipe {
                whole: None,
                sections: Vec::new(),
                tail: Tail::None,
                fields: Vec::new(),
                pos_from_key: false,
                pristine: Some(hash),
            }
        } else if whole_mode && payload.len() > 1024 {
            self.ensure_blob(hash, payload, false)?;
            Recipe { whole: Some(hash), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false, pristine: None }
        } else {
            let split = chunknbt::split(payload, whole_mode, pos_hint, strip)?;
            WST_SPLIT.0.fetch_add(t0w.elapsed().as_nanos() as u64, Ordering::Relaxed);
            WST_SPLIT.1.fetch_add(1, Ordering::Relaxed);
            let t1w = std::time::Instant::now();
            let r = match split {
            Split::Whole { hash } => {
                self.ensure_blob(hash, payload, false)?;
                Recipe { whole: Some(hash), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false, pristine: None }
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
                Recipe { whole: None, sections: Vec::new(), tail: Tail::None, fields, pos_from_key, pristine: None }
            }
            Split::Split { sections, tail } => {
                if sections.len() > MAX_SECTIONS {
                    let h = chunknbt::hash_of(payload);
                    self.ensure_blob(h, payload, false)?;
                    Recipe { whole: Some(h), sections: Vec::new(), tail: Tail::None, fields: Vec::new(), pos_from_key: false, pristine: None }
                } else {
                    let mut hashes = Vec::with_capacity(sections.len());
                    // v0.9.39: a blob-eligible tail (>= tail_blob_min) rides
                    // the hot-slab batch as its LAST member — no standalone
                    // compress/record/append per tail (the batch map phase
                    // dedups it for free). Below the slab threshold the old
                    // per-blob loop applies, tail included.
                    let tail_blob = if tail.len() >= self.cfg.tail_blob_min {
                        Some((chunknbt::hash_of(&tail), sections.len() >= HOT_SLAB_MIN_SECS))
                    } else {
                        None
                    };
                    if sections.len() >= HOT_SLAB_MIN_SECS {
                        // hot-slab batch: one frame for the chunk's sections
                        let mut slices: Vec<(Hash, &[u8])> =
                            sections.iter().map(|(h, b)| (*h, b.as_slice())).collect();
                        if let Some((th, _)) = &tail_blob {
                            slices.push((*th, tail.as_slice()));
                        }
                        self.ensure_blobs_batch(&slices)?;
                    } else {
                        for (h, bytes) in &sections {
                            self.ensure_blob(*h, bytes.as_slice(), false)?;
                        }
                        if let Some((th, false)) = &tail_blob {
                            let _tt = std::time::Instant::now();
                            let r = self.ensure_blob(*th, &tail, false);
                            ENSX_TBLOB.0.fetch_add(_tt.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            ENSX_TBLOB.1.fetch_add(1, Ordering::Relaxed);
                            r?;
                        }
                    }
                    for (h, _) in &sections {
                        hashes.push(*h);
                    }
                    // tail: dedup large tails as blobs, inline tiny ones.
                    // compress_tier used to run BEFORE the size branch and
                    // its z6 output was thrown away for every tail >= the
                    // blob threshold (the blob path recompresses inside
                    // ensure_blob) — pure wasted CPU on most chunk tails.
                    // Since v0.9.39 the blob-eligible tail was ALREADY stored
                    // above (batch member on the slab path, ensure_blob on
                    // the per-blob path) — nothing left to do here.
                    let tail_ref = match &tail_blob {
                        Some((th, _)) => Tail::Blob(*th),
                        None => {
                            let _tt = std::time::Instant::now();
                            let (tdict, tcomp) = self.compress_tier(&tail, false);
                            ENSX_TINL.0.fetch_add(_tt.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            ENSX_TINL.1.fetch_add(1, Ordering::Relaxed);
                            Tail::Inline { dict: tdict, comp: tcomp, raw: tail.len() as u32 }
                        }
                    };
                    Recipe { whole: None, sections: hashes, tail: tail_ref, fields: Vec::new(), pos_from_key: false, pristine: None }
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

        // one blobs.read() guard for the WHOLE recipe encode (the closure
        // used to re-acquire per section hash — ~22 read-lock round-trips
        // per chunk; the MT lock census measured 10.8-12.3 ms of a 146 ms
        // 2-thread import wall on that pattern, 660 ns per contended
        // acquisition vs ~40 ns uncontended)
        let rec = {
            let blobs = self.blobs.read();
            let id_of = |h: Hash| -> Option<u32> {
                let l = blobs.get(&h)?;
                if l.id == NO_ID { None } else { Some(l.id) }
            };
            if recipe.fields.is_empty() {
                recipe.encode(id_of)
            } else {
                recipe
                    .encode_fields(id_of)
                    .ok_or_else(|| "field recipe too large".to_string())?
            }
        };
        self.bump_recipe_refs(&recipe);

        // supersede refs: a pending batch entry first, then the chunks-map
        // entry (which is only dropped when it was not already superseded by
        // a pending entry for the same key). The batch Mutex only guards the
        // map removal — parse_recipe_body (segment read + decompress) and
        // drop_recipe_refs (blobs.write + ids.write) run OUTSIDE it (they
        // used to nest inside, serializing MT writers on the batch lock
        // while holding blobs.write).
        let pending_old = {
            let mut batch = self.recipe_batch.lock();
            batch.remove(key)
        };
        if let Some(ref old_body) = pending_old {
            if let Some(old_rec) = self.parse_recipe_body(old_body) {
                self.drop_recipe_refs(&old_rec);
            }
        }
        // v0.9.35 LOSS FIX (CI, concurrent_supersede): resolve the last
        // COMMITTED version (recipe + virtual length) BEFORE queuing the new
        // pending entry — the in-push flush (192 KiB cap) re-points chunks
        // and overwrites batch_idx together, so any resolution after the
        // push would see the NEW version and drop its refs instead. The
        // batched resolution goes through the CAPTURED location (boff must
        // equal the captured offset): a flush that re-pointed the key in
        // between has already taken over this version's retirement.
        let old_committed = if pending_old.is_none() {
            self.chunks
                .read()
                .get(&key)
                .copied()
                .and_then(|(oseg, ooff, olen)| {
                    if olen == BATCHED {
                        let idx = self.batch_idx.lock().get(&key).copied();
                        let Some((boff, soff, rl)) = idx.filter(|(b, _, _)| *b == ooff) else {
                            return None;
                        };
                        let rec = self.load_batched(oseg, boff, soff, rl);
                        Some((oseg, ooff, olen, rec, rl))
                    } else {
                        let rec = self.load_recipe(oseg, ooff, olen);
                        Some((oseg, ooff, olen, rec, olen))
                    }
                })
        } else {
            None
        };
        // v0.9.35: the new pending entry is queued BEFORE the committed
        // version's refs are dropped, so a concurrent segment rewrite always
        // sees it (its pending-skip guard) and never re-queues the old bytes
        // over the newer pending. The old order (drop refs -> eager
        // batch_idx.remove -> push) left a window where chunks still pointed
        // BATCHED at the old batch while batch_idx had no entry: every read
        // of the chunk returned None (CI: "chunk 16 lost"), and the segment
        // rewrite hit the batch_idx miss -> poisoned the segment (8 misses,
        // seg-2 KEPT on the CI run).
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
        if let Some((oseg, ooff, olen, old_rec, rl)) = old_committed {
            if let Some(old_rec) = old_rec {
                self.drop_recipe_refs(&old_rec);
            }
            if let Some(s) = self.segs.read().get(&oseg) {
                s.live.fetch_sub(8 + rl as u64, Ordering::Relaxed);
            }
            // v0.9.35: the batch_idx entry is KEPT here (the old eager
            // remove is gone). It resolves reads of the last committed
            // version inside this window; the flush overwrites it together
            // with the chunks pointer under one lock pair, and the rewrite's
            // retire is offset-guarded so a stale entry pointing into a
            // dying segment is retired exactly once, during the move.
            let _ = (oseg, ooff, olen);
        }
        WST_RECIPE.0.fetch_add(t2w.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let _tl = std::time::Instant::now();
        self.last_written.lock().insert(key, hash);
        MT_LASTW_NS.fetch_add(_tl.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// DIAGNOSTIC: fetch one blob's canonical bytes by hash (read_blob with
    /// glob_to_canon applied). For probes/forensics only.
    pub fn read_blob_by_hash(&self, h: Hash) -> Option<Vec<u8>> {
        let loc = *self.blobs.read().get(&h)?;
        self.read_blob(loc, None)
    }

    /// DIAGNOSTIC: the section hashes of a chunk's recipe, in order.
    pub fn debug_section_hashes(&self, key: u64) -> Option<Vec<Hash>> {
        let rec = self.load_recipe_for_debug(key)?;
        Some(rec.sections)
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
                            // The cache lookup MUST be a standalone statement:
                            // in edition 2021 an `if let` scrutinee temporary
                            // (the MutexGuard here) lives until the end of the
                            // whole if/else, so re-locking probe_cache inside
                            // the else branch was a SAME-THREAD self-deadlock
                            // (spin+park loop) on every probe miss — the
                            // speed-bench G2 phase hung forever and the job
                            // timed out; a Paper autosave of unchanged chunks
                            // would have frozen the server the same way.
                            let cached = self.probe_cache.lock().get(&pk).cloned();
                            if let Some(set) = cached {
                                Some(set)
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

    /// v0.9.44: the pristine contract hash of a key, if its live recipe is
    /// mode 7 (pending batch first — read-your-writes — then committed).
    fn pristine_hash_of(&self, key: u64) -> Option<Hash> {
        if let Some(p) = self.recipe_batch.lock().get(key).cloned() {
            return self.parse_recipe_body(&p).and_then(|r| r.pristine);
        }
        let (seg, off, len) = *self.chunks.read().get(&key)?;
        self.recipe_at(key, seg, off, len).and_then(|r| r.pristine)
    }

    /// Verify/tooling hook: does this key resolve to a pristine
    /// regeneration contract (nothing stored, read = None by design)?
    pub fn is_pristine(&self, key: u64) -> bool {
        self.pristine_hash_of(key).is_some()
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
        // v0.9.44 pristine regeneration contract: nothing is stored. The
        // shim's nReadChunk → null → vanillaReadData → NO_DATA path makes
        // Paper regenerate the chunk from the seed (identical semantics to
        // a missing chunk in the save format).
        if rec.pristine.is_some() {
            return None;
        }
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
                let slices_raw: Vec<&[u8]> = loaded
                    .refs
                    .iter()
                    .map(|(i, s, e)| &loaded.keep[*i][*s as usize..*e as usize])
                    .collect();
                // GLOB members (0xCB/0xCC) convert back to canonical CSEC
                // here — reads are the only place GLOB bytes exist. Results
                // are cached by the section's content hash (member bytes are
                // immutable + registry is append-only = pure function), so
                // each member decodes once per process, not once per read.
                let mut canon: Vec<Option<Arc<Vec<u8>>>> = Vec::with_capacity(slices_raw.len());
                let t_glob = std::time::Instant::now();
                for (i, s) in slices_raw.iter().enumerate() {
                    let c = self.glob_to_canon_cached(rec.sections[i], s);
                    if c.is_some() {
                        ST_GLOB_CONV.1.fetch_add(1, Ordering::Relaxed);
                    }
                    canon.push(c);
                }
                {
                    ST_GLOB_CONV.0.fetch_add(t_glob.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                let mut slices: Vec<&[u8]> = Vec::with_capacity(slices_raw.len());
                for (i, s) in slices_raw.iter().enumerate() {
                    match &canon[i] {
                        Some(c) => slices.push(c.as_slice()),
                        None => slices.push(s),
                    }
                }
                {
                    let el = t_sec.elapsed().as_nanos() as u64;
                    ST_SECTLOAD.0.fetch_add(el, Ordering::Relaxed);
                    ST_SECTLOAD.1.fetch_add(1, Ordering::Relaxed);
                }
                // shift-ref resolution (v0.7): [0xC9] members expand to
                // canonical CSEC3 against their chunk-local target; keys are
                // re-mixed so the fused emit cache stays content-keyed.
                let t_shr = std::time::Instant::now();
                let resolved = match chunknbt::resolve_shifts(&slices, &rec.sections) {
                    Some(r) => r,
                    None => return fail("shift-resolve"),
                };
                {
                    ST_SHIFT_RES.0.fetch_add(t_shr.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    ST_SHIFT_RES.1.fetch_add(1, Ordering::Relaxed);
                }
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
                        ST_SLAB_HIT.1.fetch_add(1, Ordering::Relaxed);
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
                let kind = hdr[0] & 0x3f; // 0x80 = GLOB, 0x40 = hot tier
                if kind != 5 && kind != 8 {
                    return None;
                }
                // GLOB slabs (kind | 0x80) start their data one byte later
                let hlen = if hdr[0] & 0x80 != 0 { 13usize } else { 12usize };
                let cl = u32::from_le_bytes(hdr[2..6].try_into().ok()?) as usize;
                let raw_total = u32::from_le_bytes(hdr[6..10].try_into().ok()?);
                let data = seg.read_at(loc.off + hlen as u64, cl, sealed)?;
                let t_dec = std::time::Instant::now();
                let raw = Arc::new(self.decompress(loc.dict_id, &data, raw_total)?);
                {
                    ST_SLAB_MISS.0.fetch_add(raw_total as u64, Ordering::Relaxed);
                    ST_SLAB_MISS.1.fetch_add(1, Ordering::Relaxed);
                    ST_DECOMP.0.fetch_add(t_dec.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    ST_DECOMP.1.fetch_add(1, Ordering::Relaxed);
                }
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
                return self.glob_to_canon(s);
            }
            let hdr = seg.read_at(loc.off, 12, sealed)?;
            let kind = hdr[0] & 0x3f; // 0x80 = GLOB, 0x40 = hot tier
            if kind != 5 && kind != 8 {
                return None;
            }
            // GLOB slabs (kind | 0x80) carry one extra header byte (reg_w);
            // its value is informational, the data just starts one byte later
            let hlen = if hdr[0] & 0x80 != 0 { 13usize } else { 12usize };
            let cl = u32::from_le_bytes(hdr[2..6].try_into().ok()?) as usize;
            let raw_total = u32::from_le_bytes(hdr[6..10].try_into().ok()?);
            let data = seg.read_at(loc.off + hlen as u64, cl, sealed)?;
            let raw = self.decompress(loc.dict_id, &data, raw_total)?;
            let out = raw
                .get(loc.slab_off as usize..(loc.slab_off + loc.raw_len) as usize)?
                .to_vec();
            drop(segs);
            self.slab_cache.lock().put(key, Arc::new(raw));
            return self.glob_to_canon(&out);
        }
        let data = seg.read_at(loc.off + loc.header_len(), loc.comp_len as usize, sealed)?;
        self.decompress(loc.dict_id, &data, loc.raw_len)
    }

    /// Convert a member's raw bytes to canonical CSEC when they carry a
    /// GLOB form (0xCB raw-u8 legacy or 0xCC rle/u16). Reads are the only
    /// place GLOB bytes exist; every decoder and every re-pack sees
    /// canonical bytes.
    fn glob_to_canon(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if !chunknbt::is_glob_member(bytes) {
            return Some(bytes.to_vec());
        }
        let reg = self.glob_reg.lock();
        match chunknbt::glob_any_to_section(bytes, &reg) {
            Some(c) => Some(c),
            None => {
                ST_GLOB_CONV_FAIL.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[c-saves] GLOB member convert failed len={} reg={}",
                    bytes.len(),
                    reg.len()
                );
                None
            }
        }
    }

    /// Hash-keyed variant for the section read path: the stored member bytes
    /// are immutable (content-addressed) and the registry is append-only, so
    /// the canonical form is computed once per member and reused across
    /// reads. Returns None for non-members (caller keeps the raw slice).
    fn glob_to_canon_cached(&self, h: chunknbt::Hash, bytes: &[u8]) -> Option<Arc<Vec<u8>>> {
        if !chunknbt::is_glob_member(bytes) {
            return None;
        }
        if let Some(c) = self.canon_cache.lock().get(&h) {
            return Some(c);
        }
        let reg = self.glob_reg.lock();
        match chunknbt::glob_any_to_section(bytes, &reg) {
            Some(c) => {
                let c = Arc::new(c);
                self.canon_cache.lock().put(h, c.clone());
                Some(c)
            }
            None => {
                ST_GLOB_CONV_FAIL.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[c-saves] GLOB member convert failed len={} reg={}",
                    bytes.len(),
                    reg.len()
                );
                None
            }
        }
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

    /// Names of dictionary files still on disk (forensics for integrity
    /// reports: which dict versions survived GC).
    pub fn dict_files_on_disk(&self) -> Vec<String> {
        std::fs::read_dir(self.dict_dir())
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.starts_with("dict-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forensics for a chunk that failed to read: walk the read path and name
    /// the FIRST blocking stage (no recipe / recipe unreadable / blob gone
    /// from the map / segment file gone / blob range beyond the segment /
    /// dictionary version absent in RAM AND on disk / inline-tail dict gone).
    /// Integrity-scan tooling, never on the hot path.
    pub fn read_fail_reason(&self, key: u64) -> String {
        let ptr = self.chunks.read().get(&key).copied();
        let Some((seg, off, len)) = ptr else {
            return if self.recipe_batch.lock().get(key).is_some() {
                "pending batch recipe unparseable".to_string()
            } else {
                "no recipe in chunks map".to_string()
            };
        };
        let Some(rec) = self.recipe_at(key, seg, off, len) else {
            return format!("recipe unreadable at seg={seg} off={off} len={len}");
        };
        let hashes = rec.blob_hashes();
        for h in &hashes {
            let l = match self.blobs.read().get(h) {
                Some(l) => *l,
                None => return format!("blob {h:016x} missing from blobs map"),
            };
            let segs = self.segs.read();
            let seg_total = segs.get(&l.seg).map(|s| s.total.load(Ordering::Relaxed));
            drop(segs);
            let Some(seg_total) = seg_total else {
                return format!("blob {h:016x} segment {} removed from segs", l.seg);
            };
            let end = if l.slab {
                l.slab_off as u64 + l.raw_len as u64
            } else {
                l.off + l.header_len() as u64 + l.comp_len as u64
            };
            if end > seg_total {
                return format!(
                    "blob {h:016x} range end {end} > seg {} size {seg_total}",
                    l.seg
                );
            }
            if l.dict_id != 0
                && !self.dicts.read().contains_key(&l.dict_id)
                && !self.dict_dir().join(format!("dict-{}.bin", l.dict_id)).exists()
            {
                return format!(
                    "blob {h:016x} dict v{} absent in RAM and on disk",
                    l.dict_id
                );
            }
        }
        if let Tail::Inline { dict, .. } = &rec.tail {
            if *dict != 0
                && !self.dicts.read().contains_key(dict)
                && !self.dict_dir().join(format!("dict-{dict}.bin")).exists()
            {
                return format!("inline tail dict v{dict} absent in RAM and on disk");
            }
        }
        format!(
            "map-level checks pass (recipe seg={seg}, {} blob refs) — failure is in decompress/join",
            hashes.len()
        )
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
        if self.reg_dirty.swap(false, Ordering::Relaxed) {
            let _ = self.persist_glob_reg();
        }
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
        if self.reg_dirty.swap(false, Ordering::Relaxed) {
            let _ = self.persist_glob_reg();
        }
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
        // ---- Phase A (serial, group order): build BOTH variant streams per
        // group. The GLOB conversion order is IDENTICAL to the serial flow
        // (groups in order, members in order, same `len() <= 65536` gate per
        // group), so registry gids and every stream byte match the old
        // single-threaded pack — drained containers stay byte-identical.
        struct GVar {
            stream: Vec<u8>,
            tab_len: usize,
            lens: Vec<usize>,
        }
        struct GBuild {
            group: Vec<(Hash, u32, BlobLoc, Vec<u8>)>,
            canon: GVar,
            /// (variant stream, reg_w: 1 = all-u8 gids, 2 = u16 present)
            glob: Option<(GVar, u8)>,
        }
        let _tb = std::time::Instant::now();
        let cores = std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(1);
        // ---- Phase A (v0.9.33: parallel build) ---- The GLOB conversion
        // used to be ~95% of the build wall and strictly serial, because
        // registry gid assignment order pins every stream byte. The
        // conversion is now split (chunknbt): stage 1 (parse + unpack +
        // bounds + first-reference order) and stage 3 (gid map + RLE scans
        // + materialize) are PURE functions of the member bytes and run on
        // all cores; stage 2 — the palette-ref get_or_insert sweep — is the
        // only registry-touching part and runs serially in the EXACT old
        // order (group order x member order x first-reference order), so
        // registry gids, the reg_dirty discipline, every failure decision
        // and every stream byte match the serial pack.
        let total: usize = groups.iter().map(|g| g.len()).sum();
        let flat_raws: Vec<&[u8]> = groups
            .iter()
            .flat_map(|g| g.iter().map(|c| c.3.as_slice()))
            .collect();
        // stage 1 (parallel, pure)
        let mut pres: Vec<Option<chunknbt::GlobPre>> = (0..total).map(|_| None).collect();
        {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let outs: std::sync::Mutex<Vec<Option<Option<chunknbt::GlobPre>>>> =
                std::sync::Mutex::new((0..total).map(|_| None).collect());
            std::thread::scope(|sc| {
                for _ in 0..cores.min(total).max(1) {
                    sc.spawn(|| loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        if j >= total {
                            break;
                        }
                        let pre = chunknbt::section_glob_pre(flat_raws[j]);
                        outs.lock().unwrap()[j] = Some(pre);
                    });
                }
            });
            for (d, p) in outs.into_inner().unwrap().into_iter().zip(pres.iter_mut()) {
                *p = d.flatten();
            }
        }
        // stage 2 (SERIAL, the only registry-touching sweep): the
        // per-member `len() <= 65536` gate mirrors section_to_glob's
        // internal check (the registry may cross the line MID-group: later
        // members then refuse conversion, exactly like the old monolithic
        // call), and the group-level reg_dirty flag fires on any growth.
        let mut pgids: Vec<Option<Vec<u32>>> = vec![None; total];
        if self.glob_enabled() {
            let mut m = 0usize;
            for group in &groups {
                let mut reg = self.glob_reg.lock();
                let before = reg.len();
                for _c in group {
                    if reg.len() <= 65536 {
                        if let Some(pre) = &pres[m] {
                            pgids[m] = chunknbt::section_glob_resolve(pre, &mut reg);
                        }
                    }
                    m += 1;
                }
                if reg.len() != before {
                    self.reg_dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        // stage 3 (parallel, pure)
        let mut globs: Vec<Option<Vec<u8>>> = vec![None; total];
        {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let outs: std::sync::Mutex<Vec<Option<Option<Vec<u8>>>>> =
                std::sync::Mutex::new((0..total).map(|_| None).collect());
            let pres_ref = &pres;
            let pgids_ref = &pgids;
            std::thread::scope(|sc| {
                for _ in 0..cores.min(total).max(1) {
                    sc.spawn(|| loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        if j >= total {
                            break;
                        }
                        let r = match (&pres_ref[j], &pgids_ref[j]) {
                            (Some(pre), Some(pgid)) => {
                                chunknbt::section_glob_finish(flat_raws[j], pre, pgid)
                            }
                            _ => None,
                        };
                        outs.lock().unwrap()[j] = Some(r);
                    });
                }
            });
            for (d, g) in outs.into_inner().unwrap().into_iter().zip(globs.iter_mut()) {
                *g = d.flatten();
            }
        }
        drop(flat_raws);
        drop(pres);
        drop(pgids);
        // group offsets into the flat member arrays
        let mut offs = Vec::with_capacity(groups.len() + 1);
        let mut acc = 0usize;
        offs.push(0);
        for g in &groups {
            acc += g.len();
            offs.push(acc);
        }
        let builds: Vec<GBuild> = groups
            .into_iter()
            .enumerate()
            .map(|(gi, group)| {
                let n = group.len();
                // SLAB V2 (kind 8): entry table = varint zigzag(id delta) ++
                // varint(raw len) ++ 16B hash per member, then concatenated
                // raws. v1 spent 8 fixed bytes/member on id+len; with ids
                // assigned in walk order the deltas are ~1 byte, lens ~1-2
                // bytes.
                let build = |lens: &[usize], raws: &[&[u8]]| -> (Vec<u8>, usize) {
                    let mut tab = Vec::with_capacity(n * 4);
                    let mut prev_id: i64 = 0;
                    for (i, (h, id, _loc, _raw)) in group.iter().enumerate() {
                        put_u64(&mut tab, zigzag(*id as i64 - prev_id));
                        prev_id = *id as i64;
                        put_u64(&mut tab, lens[i] as u64);
                        tab.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
                        tab.extend_from_slice(&(*h as u64).to_le_bytes());
                    }
                    let tl = tab.len();
                    let mut st = tab;
                    for r in raws {
                        st.extend_from_slice(r);
                    }
                    (st, tl)
                };
                let canon_lens: Vec<usize> = group.iter().map(|c| c.3.len()).collect();
                let canon_raws: Vec<&[u8]> = group.iter().map(|c| c.3.as_slice()).collect();
                let _tbc = std::time::Instant::now();
                let (canon_stream, canon_tab_len) = build(&canon_lens, &canon_raws);
                PK_B_TAB_NS.fetch_add(_tbc.elapsed().as_nanos() as u64, Ordering::Relaxed);
                // GLOB variant (jj_exp5: x2.16 on vanilla-terrain dense
                // content): the same members with dense CSEC sections
                // converted to the 0xCB global-id form. The slab stores
                // whichever variant compresses smaller at the cold tier —
                // the window-level comparison is the honest gate (raw sizes
                // mislead: the gid stream is wider but matches
                // cross-section). Non-converting members keep their
                // canonical bytes; every member is self-describing by magic.
                // v0.9.33: converted bytes come from the parallel stage 3;
                // the registry sweep already happened in stage 2.
                let mut glob: Option<(GVar, u8)> = None;
                let _tg = std::time::Instant::now();
                {
                    let base = offs[gi];
                    let mut glens = Vec::with_capacity(n);
                    let mut graws: Vec<Vec<u8>> = Vec::with_capacity(n);
                    let mut any = false;
                    for (mi, (_h, _id, _loc, raw)) in group.iter().enumerate() {
                        match globs[base + mi].take() {
                            Some(g) => {
                                any = true;
                                glens.push(g.len());
                                graws.push(g);
                            }
                            None => {
                                glens.push(raw.len());
                                graws.push(raw.clone());
                            }
                        }
                    }
                    if any {
                        let graws_ref: Vec<&[u8]> = graws.iter().map(|v| v.as_slice()).collect();
                        let _tbg = std::time::Instant::now();
                        let (gs, gtab_len) = build(&glens, &graws_ref);
                        PK_B_TAB_NS.fetch_add(_tbg.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        let reg_w = if graws.iter().any(|g| chunknbt::glob_member_wide(g)) {
                            2u8
                        } else {
                            1u8
                        };
                        glob = Some((GVar { stream: gs, tab_len: gtab_len, lens: glens }, reg_w));
                    }
                }
                PK_B_GLOB_NS.fetch_add(_tg.elapsed().as_nanos() as u64, Ordering::Relaxed);
                GBuild {
                    group,
                    canon: GVar { stream: canon_stream, tab_len: canon_tab_len, lens: canon_lens },
                    glob,
                }
            })
            .collect();
        PK_BUILD_NS.fetch_add(_tb.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // ---- Phase B (parallel): one zstd cold-tier compression per variant
        // stream, work-stealing across all cores, LONGEST-FIRST: the pack
        // wall is the longest single-stream compression, so the poles must
        // start while every core is still free (with short tasks first the
        // tail degenerates into one long stream running alone). Streams
        // >= cfg.pack_mt_min bytes additionally split INSIDE one zstd frame
        // (nbWorkers = cfg.pack_mt_workers, or auto = cores split evenly
        // across the batch's big streams, < 2 = stay on the ST bulk path):
        // per-stream parallelism cannot touch a single 12 MB GLOB variant
        // that spends 4-5 s at z19 — intra-frame workers divide exactly
        // that pole. The worker budget never exceeds the core count (no
        // oversubscription with the ST streams still running in the pool).
        // MT frame bytes differ from the ST encoder (job boundaries) but
        // are deterministic per (input, level, worker count) and decode
        // with the stock single-threaded decoder; dedup hashes are
        // content-based and every semantic gate is unaffected. zstd is
        // deterministic per (input, level) on the ST path, so pooled
        // outputs there still match the serial pack byte-for-byte.
        let flat: Vec<&[u8]> = builds
            .iter()
            .flat_map(|b| {
                let mut v = vec![b.canon.stream.as_slice()];
                if let Some((g, _)) = &b.glob {
                    v.push(g.stream.as_slice());
                }
                v
            })
            .collect();
        let outs: std::sync::Mutex<Vec<Option<(u8, Vec<u8>)>>> =
            std::sync::Mutex::new((0..flat.len()).map(|_| None).collect());
        let level = self.cfg.cold_level;
        let lz = cold_codec_lzma2();
        let mt_min = self.cfg.pack_mt_min;
        let big_cnt = if mt_min > 0 {
            flat.iter().filter(|s| s.len() >= mt_min).count()
        } else {
            0
        };
        let mut order: Vec<usize> = (0..flat.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(flat[i].len()));
        // AUTO policy: concentrate the cores on the poles — but ONLY on
        // 4+ core hosts. CI verdict (0.9.26): the 2-core runner is
        // WORK-BOUND (wall ≈ work/cores; comp 6.63 s = 13 core-s of z19
        // over 2 cores), so intra-frame MT cannot shorten the makespan —
        // it only adds job overhead + scheduling contention (7.45 ->
        // 7.87 s, mtjobs:6 crammed into one wall). On 4+ cores the wall
        // math flips: work/4 ≈ 3.3-3.7 s < the 4.86 s single-stream pole
        // → the pole binds and intra-frame MT is exactly the right
        // division. Splitting evenly across every big stream (cores/big)
        // under-workers each pole and dies when big_cnt > cores; instead
        // the k = cores/2 LONGEST streams go intra-frame MT with cores/k
        // workers each and the rest of the bigs stay ST in the pool — the
        // total worker budget stays == cores. Explicit pack_mt_workers
        // overrides everything (all bigs get that worker count — an ops
        // lever, may oversubscribe on any host).
        let k = if self.cfg.pack_mt_workers > 0 {
            big_cnt
        } else if cores >= 4 {
            (big_cnt.min(cores / 2)).max(1)
        } else {
            0
        };
        let per_big: u32 = if mt_min > 0 && big_cnt > 0 && k > 0 {
            if self.cfg.pack_mt_workers > 0 {
                (self.cfg.pack_mt_workers as u32).min(16)
            } else {
                ((cores / k) as u32).min(16)
            }
        } else {
            0
        };
        // rank-select the k longest big streams for MT (deterministic:
        // stream lens are content-determined, ties break by group order)
        let mut mt_sel = vec![false; flat.len()];
        if per_big > 1 {
            let mut r = 0usize;
            for &i in &order {
                if r >= k || flat[i].len() < mt_min {
                    break;
                }
                mt_sel[i] = true;
                r += 1;
            }
        }
        let _tc = std::time::Instant::now();
        let compress_one = |i: usize| -> (u8, Vec<u8>) {
            let stream = flat[i];
            if lz {
                // v0.9.41: LZMA2 cold slab streams. Per-stream intra-frame
                // MT stays zstd-specific; the outer per-stream scope already
                // spreads streams across cores. Any error falls back to the
                // zstd path WITH the matching codec stamp (a stream is never
                // mislabeled).
                match lzma2_compress(stream) {
                    Some(c) => {
                        PK_MT_BYTES.fetch_add(stream.len() as u64, Ordering::Relaxed);
                        (SLAB_CODEC_LZMA2, c)
                    }
                    None => (0, bulk_compress(stream, level).1),
                }
            } else if mt_sel[i] {
                PK_MT_JOBS.fetch_add(1, Ordering::Relaxed);
                PK_MT_BYTES.fetch_add(stream.len() as u64, Ordering::Relaxed);
                let t = std::time::Instant::now();
                let r = mt_compress(stream, level, per_big)
                    .unwrap_or_else(|| bulk_compress(stream, level).1);
                PK_MT_NS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                (0, r)
            } else {
                let r = bulk_compress(stream, level).1;
                (0, r)
            }
        };
        if flat.len() > 1 {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|sc| {
                let par = cores.min(flat.len());
                for _ in 0..par {
                    sc.spawn(|| loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        if j >= order.len() {
                            break;
                        }
                        let i = order[j];
                        let r = compress_one(i);
                        outs.lock().unwrap()[i] = Some(r);
                    });
                }
            });
        } else {
            for i in 0..flat.len() {
                let r = compress_one(i);
                outs.lock().unwrap()[i] = Some(r);
            }
        }
        PK_COMP_NS.fetch_add(_tc.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let mut outs = outs.into_inner().unwrap();
        // ---- Phase C (serial, group order): variant pick + append + blob
        // map updates, exactly the serial flow's decisions and byte layout.
        let mut task_at = 0usize;
        for GBuild { group, canon, glob } in builds {
            let (cc_did, cc) = outs[task_at].take().expect("canon comp missing");
            task_at += 1;
            let cg = glob.as_ref().map(|_| {
                let (d, c) = outs[task_at].take().expect("glob comp missing");
                task_at += 1;
                (d, c)
            });
            let n = group.len();
            let (dict_id, comp, kind_byte, stream, tab_len, lens, gwide): (
                u8,
                Vec<u8>,
                u8,
                Vec<u8>,
                usize,
                Vec<usize>,
                u8,
            ) = if let Some((gv, gwid)) = glob {
                let (cg_did, cg) = cg.expect("glob comp present");
                // CSAVES_FORCE_GLOB=1: debug knob — take the glob variant
                // unconditionally so the read/scan paths are testable
                // regardless of the size heuristic. v0.9.33: once-resolved
                // (was a getenv per GROUP on the pack path). v0.9.35: tests
                // flip the override below, not the env (setenv races the
                // OnceLock and other tests' getenv — UB).
                static FORCE_GLOB: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let force = if cfg!(test) && FORCE_GLOB_TEST.0.load(Ordering::Relaxed) {
                    FORCE_GLOB_TEST.1.load(Ordering::Relaxed) != 0
                } else {
                    *FORCE_GLOB.get_or_init(|| {
                        std::env::var("CSAVES_FORCE_GLOB").as_deref() == Ok("1")
                    })
                };
                if force || cg.len() < cc.len() {
                    // the registry MUST be on disk before the slab that
                    // references its gids (crash = silent misresolution)
                    if self.reg_dirty.swap(false, Ordering::Relaxed) {
                        match self.persist_glob_reg() {
                            Ok(()) => {
                                ST_GLOB_SLABS.fetch_add(1, Ordering::Relaxed);
                                (cg_did, cg, 8u8 | 0x80, gv.stream, gv.tab_len, gv.lens, gwid)
                            }
                            Err(e) => {
                                eprintln!(
                                    "[c-saves] states persist failed ({e}) — keeping the canonical slab"
                                );
                                (cc_did, cc, 8u8, canon.stream, canon.tab_len, canon.lens, 1u8)
                            }
                        }
                    } else {
                        ST_GLOB_SLABS.fetch_add(1, Ordering::Relaxed);
                        (cg_did, cg, 8u8 | 0x80, gv.stream, gv.tab_len, gv.lens, gwid)
                    }
                } else {
                    (cc_did, cc, 8u8, canon.stream, canon.tab_len, canon.lens, 1u8)
                }
            } else {
                (cc_did, cc, 8u8, canon.stream, canon.tab_len, canon.lens, 1u8)
            };
            let raw_total = stream.len() as u32;
            // reg_w: the widest gid stream among this slab's GLOB members
            // (informational on read — members are self-describing by magic);
            // computed at conversion time in Phase A, stamped only when the
            // GLOB variant wins.
            let reg_w = gwide;
            let _ta = std::time::Instant::now();
            let mut body = Vec::with_capacity(13 + comp.len());
            body.push(kind_byte);
            body.push(dict_id);
            body.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            body.extend_from_slice(&raw_total.to_le_bytes());
            body.extend_from_slice(&(n as u16).to_le_bytes());
            if kind_byte & 0x80 != 0 {
                body.push(reg_w); // 1 = u8 gid streams, 2 = u16 present
            }
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
                    // pack-produced slabs are the cold tier: excluded from
                    // re-pack (the 0x40 bit makes hot batch frames distinct)
                    l.cold = true;
                    l.share = if i + 1 == cnt { last_share.min(u32::MAX as u64) as u32 } else { share };
                    l.slab_off = 0; // patched below by cursor
                    l.slab_raw = raw_total;
                    l.slab_comp = comp.len() as u32;
                    // the member's length in THIS slab's form: the GLOB
                    // variant changes member lengths (gid streams vs packed
                    // canonical), so raw_len MUST follow — reads slice the
                    // member out of the slab with it
                    l.raw_len = lens[i] as u32;
                    l.comp_len = lens[i] as u32;
                }
            }
            // patch slab_off per member in order (raws start after entry table)
            let mut cursor = tab_len as u32;
            for (i, (h, _id, _old_loc, _raw)) in group.iter().enumerate() {
                if let Some(l) = blobs.get_mut(h) {
                    l.slab_off = cursor;
                }
                cursor += lens[i] as u32;
            }
            drop(blobs);
            PK_APPEND_NS.fetch_add(_ta.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
                pristine: None,
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
                if s.pack_broken.load(Ordering::Relaxed) {
                    continue; // poisoned: never considered again
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
                // hot material = anything not on the cold tier: fresh
                // individual blobs AND hot-tier slab frames (write-path
                // batches, cold=false by the 0x40 bit). Pack-produced slabs
                // are cold=true and stay in place.
                if l.cold || l.seg == writer_id {
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
        // Poison marker: any unreadable victim on the drop path must PREVENT
        // the segment removal (removal turns the victim's dangling references
        // into permanent chunk loss). The segment is kept, poisoned, and
        // never considered again — correctness over reclaim.
        let mark_broken = || {
            if drop_seg {
                if let Some(s) = self.segs.read().get(&worst_id) {
                    s.pack_broken.store(true, Ordering::Relaxed);
                }
            }
        };
        // Locality-ordered victims: take the segment's blobs DIRECTLY from
        // the blobs map and sort by blob id — blob ids are allocated in
        // section order as chunks are written, so id order approximates
        // chunk-key/section order (same-chunk sections become adjacent slab
        // members). The previous recipe-walk collected the same set but paid
        // a full chunks-map sort + recipe_at decompress per PACKED SEGMENT:
        // on a 262k-chunk world that was O(segments x chunks) batch
        // decompresses (~40+ min of drain). Measured -17% slab bytes came
        // from the locality itself, which id order preserves.
        let mut victims: Vec<(Hash, BlobLoc)> = {
            let blobs = self.blobs.read();
            let mut v: Vec<(Hash, BlobLoc)> = blobs
                .iter()
                .filter(|(_, l)| l.seg == worst_id && (drop_seg || !l.cold))
                .map(|(h, l)| (*h, *l))
                .collect();
            v.sort_by_key(|(_, l)| (l.id, l.off));
            v
        };
        let mut cands: Vec<(Hash, u32, BlobLoc, Vec<u8>)> = Vec::new();
        let mut cand_bytes = 0usize;
        let mut did = false;
        let _tv = std::time::Instant::now();
        for (h, loc) in victims {
            if !drop_seg && loc.cold {
                continue; // cold-tier material: leave in place
            }
            let _rb = std::time::Instant::now();
            let Some(data) = self.read_blob(loc, Some(worst_id)) else {
                eprintln!(
                    "[c-saves] PACK LOSS RISK: blob {h:016x} unreadable in seg-{worst_id:05} — segment will be KEPT"
                );
                mark_broken();
                continue;
            };
            PK_READBLOB_NS.fetch_add(_rb.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
        PK_VICTIMS_NS.fetch_add(_tv.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
        //
        // v0.9.31 LOSS FIX (CI, concurrent_supersede): the writer thread's
        // own 192 KiB auto-flush can land a NEW batch recipe INTO the dying
        // segment AFTER this rewrite's snapshot was taken. The loop below
        // only iterated the snapshot, so the late-arriving recipe was never
        // moved and the segment drop deleted its bytes — read_chunk() later
        // returned None ("chunk 19 lost"). The per-key pointer re-check
        // cannot see this: the recipe is genuinely NEW, not stale. Fix:
        // fixpoint — re-snapshot the chunks map until a full pass finds no
        // recipe still pointing at worst_id. Terminates: after seal() no NEW
        // appends target this segment, so only the one in-flight flush can
        // add pointers; each pass moves at least what it sees.
        let _tr = std::time::Instant::now();
        loop {
        let recs: Vec<(u64, u32, u64, u32)> = {
            let chunks = self.chunks.read();
            chunks
                .iter()
                .filter(|(_, (s, _, _))| *s == worst_id)
                .map(|(k, (s, o, l))| (*k, *s, *o, *l))
                .collect()
        };
        if recs.is_empty() {
            break;
        }
        // v0.9.35: keys whose NEWER version is still a PENDING batch entry
        // must not be moved — re-queueing the old bytes over the pending
        // would roll the chunk back (RecipeBatch is last-write-wins). The
        // end-of-pass flush re-points such keys at the writer segment and
        // the next fixpoint snapshot drains them.
        let mut pending_skipped: Vec<u64> = Vec::new();
        for (key, _s, off, len) in recs {
            // Re-check the CURRENT chunks-map pointer: a supersede after the
            // snapshot moved this key (pending batch entry or a newer batch)
            // and the snapshot's pointer is now DEAD bytes. Moving it anyway
            // would (a) trip the batch_idx-miss poison (the supersede removed
            // the entry) or (b) re-queue the OLD bare recipe over the newer
            // pending one — RecipeBatch is last-write-wins, so the chunk
            // would silently ROLL BACK. The pending/newer state is the truth.
            if self.chunks.read().get(&key).copied() != Some((_s, off, len)) {
                continue;
            }
            if self.recipe_batch.lock().index.contains_key(&key) {
                pending_skipped.push(key);
                continue;
            }
            // normalize to a bare batched recipe
            let bare: Vec<u8> = if len == BATCHED {
                // entry lives inside a compressed kind-9 batch: pull the
                // bare bytes out of the decompressed stream
                let Some((boff, soff, rl)) = self.batch_idx.lock().get(&key).copied() else {
                    eprintln!("[c-saves] LEAK: batch_idx miss key={key:016x} during seg-{worst_id} rewrite");
                    mark_broken();
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
                    mark_broken();
                    continue;
                };
                let cl = u32::from_le_bytes(hdr[5..9].try_into().unwrap()) as usize;
                let raw_total = u32::from_le_bytes(hdr[9..13].try_into().unwrap());
                let Some(data) = seg.read_at(boff + 13, cl, !is_writer) else {
                    eprintln!("[c-saves] LEAK: body read fail key={key:016x} cl={cl}");
                    mark_broken();
                    continue;
                };
                drop(segs);
                let Some(raw) = self.decompress(0, &data, raw_total) else {
                    eprintln!("[c-saves] LEAK: batch decompress fail key={key:016x}");
                    mark_broken();
                    continue;
                };
                let Some(e) = raw.get(soff as usize..(soff + rl) as usize) else {
                    eprintln!("[c-saves] LEAK: slice fail key={key:016x} soff={soff} rl={rl} rawlen={}", raw.len());
                    mark_broken();
                    continue;
                };
                e.to_vec()
            } else {
                let segs = self.segs.read();
                let Some(seg) = segs.get(&worst_id) else { continue };
                let Some(body) = seg.read_at(off, len as usize, false) else {
                    mark_broken();
                    continue;
                };
                drop(segs);
                if body.first() == Some(&0xFF) {
                    body
                } else if body.first() == Some(&2) && body.len() >= 13 {
                    let rl = u32::from_le_bytes(body[9..13].try_into().unwrap()) as usize;
                    if body.len() < 13 + rl {
                        mark_broken();
                        continue;
                    }
                    let mut b = Vec::with_capacity(1 + rl);
                    b.push(0xFFu8);
                    b.extend_from_slice(&body[13..13 + rl]);
                    b
                } else {
                    mark_broken();
                    continue;
                }
            };
            // capture the OLD virtual length and batch location BEFORE the
            // re-queue: an in-loop flush may already give this key a fresh
            // batch in the writer segment, and the cleanup below must only
            // retire the OLD index entry (pointing into the dying segment).
            let old_rl = self.batched_len(key, len);
            {
                // v0.9.35: atomic check-and-push — a supersede during the
                // pull may have queued a NEWER pending entry; last-write-wins
                // means pushing after it would roll the chunk back.
                let mut batch = self.recipe_batch.lock();
                if batch.index.contains_key(&key) {
                    drop(batch);
                    pending_skipped.push(key);
                    continue;
                }
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
        // v0.9.35 anti-spin: pending-skip keys drain via this flush; if a
        // persistent append failure left them pending, the snapshot would
        // never empty — keep the segment (poisoned) instead of spinning.
        if !pending_skipped.is_empty() {
            let b = self.recipe_batch.lock();
            let stuck = pending_skipped.iter().any(|k| b.index.contains_key(k));
            drop(b);
            if stuck {
                mark_broken();
                break;
            }
        }
        // poison escape: a pass that hit mark_broken() leaves its key
        // pointing at worst_id (the snapshot would never drain) — the
        // segment is kept below; do not spin
        if self
            .segs
            .read()
            .get(&worst_id)
            .map(|s| s.pack_broken.load(Ordering::Relaxed))
            .unwrap_or(false)
        {
            break;
        }
        }
        // preserve interned keys recorded in this segment (kind-6 records
        // are not referenced by recipes but the key table must survive)
        self.preserve_keys(worst_id);
        let broken = self
            .segs
            .read()
            .get(&worst_id)
            .map(|s| s.pack_broken.load(Ordering::Relaxed))
            .unwrap_or(false);
        if broken {
            // Never remove a segment whose victims could not all be moved:
            // the recipes still pointing into it would dangle forever. The
            // segment stays on disk (dead bytes included) and is never
            // considered by compaction again.
            eprintln!(
                "[c-saves] packed seg-{worst_id:05} PARTIAL — segment KEPT (poisoned) to preserve dangling references"
            );
            return true;
        }
        // drop the segment
        let old = self.segs.write().remove(&worst_id);
        drop(old);
        let _ = fs::remove_file(self.seg_path(worst_id));
        self.gc_dicts();
        eprintln!("[c-saves] packed seg-{worst_id:05} (full rewrite, cold-tier recompressed)");
        PK_RECIPES_NS.fetch_add(_tr.elapsed().as_nanos() as u64, Ordering::Relaxed);
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
    pub(crate) fn chunk_payload(cx: i32, cz: i32, variant: u64) -> Vec<u8> {
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

    pub(crate) fn semantic(a: &[u8], b: &[u8]) -> bool {
        let va: Value = fastnbt::from_bytes(a).unwrap();
        let vb: Value = fastnbt::from_bytes(b).unwrap();
        va == vb
    }

    #[test]
    fn cold_lzma2_slab_roundtrip() {
        // v0.9.41: the LZMA2 cold codec must produce a fully readable,
        // drainable, reopenable store — same contract as the zstd default,
        // different slab codec on the wire (dict_id=0xFE sentinel).
        set_cold_codec_for_tests(Some("lzma2"));
        let dir = temp_dir("cold-lzma2-roundtrip");
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let n = 40usize;
        let payloads: Vec<Vec<u8>> = (0..n).map(|i| chunk_payload((i % 8) as i32, (i / 8) as i32, (i % 5) as u64)).collect();
        for (i, p) in payloads.iter().enumerate() {
            s.write_chunk(i as u64, p, false).unwrap();
        }
        s.sync();
        s.seal();
        s.request_pack();
        for _ in 0..200 {
            if !s.compact_step() {
                break;
            }
        }
        assert!(!s.compact_step(), "drain did not converge");
        s.sync();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "lzma2 post-pack chunk {i}");
        }
        // reopen: the raw LZMA2 decoder path rebuilds from disk records
        drop(s);
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "lzma2 post-reopen chunk {i}");
        }
        set_cold_codec_for_tests(None);
    }

    #[test]
    fn cold_zstd_optout_slab_roundtrip() {
        // v0.9.42 mirror of the lzma2 roundtrip: CSAVES_COLD_CODEC=zstd opts
        // back out of the new default — the zstd wire (dict_id stamps) must
        // stay fully functional for stores that pinned it.
        set_cold_codec_for_tests(Some("zstd"));
        let dir = temp_dir("cold-zstd-optout-roundtrip");
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let n = 40usize;
        let payloads: Vec<Vec<u8>> = (0..n).map(|i| chunk_payload((i % 8) as i32, (i / 8) as i32, (i % 5) as u64)).collect();
        for (i, p) in payloads.iter().enumerate() {
            s.write_chunk(i as u64, p, false).unwrap();
        }
        s.sync();
        s.seal();
        s.request_pack();
        for _ in 0..200 {
            if !s.compact_step() {
                break;
            }
        }
        assert!(!s.compact_step(), "drain did not converge");
        s.sync();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "zstd post-pack chunk {i}");
        }
        drop(s);
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            assert!(semantic(&s.read_chunk(i as u64).unwrap(), p), "zstd post-reopen chunk {i}");
        }
        set_cold_codec_for_tests(None);
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
        // bounded drain: compact_step must converge, otherwise the test would
        // spin forever (a non-converging maintenance loop is itself a bug)
        for _ in 0..200 {
            if !s.compact_step() {
                break;
            }
        }
        assert!(
            !s.compact_step(),
            "drain did not converge: compact_step still finds work"
        );
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

    /// All-unique-sections payload: every section's data depends on
    /// (cx, cz, y, salt) so a rewrite strands ~24 dead section blobs per
    /// chunk (chunk_payload shares 18 of 24 sections per variant, which
    /// keeps them alive across rewrites). Tail stays < tail_blob_min so it
    /// is stored INLINE in the kind-2 recipe (the loss vector under test).
    fn unique_payload(cx: i32, salt: u64) -> Vec<u8> {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert(
            "Position".to_string(),
            Value::List(vec![Value::Int(cx), Value::Int(0), Value::Int(cx * 7)]),
        );
        let mut hm: Map<String, Value> = Map::new();
        let la: Vec<i64> = (0..37).map(|i| (i as i64) * 7 + salt as i64).collect();
        hm.insert("MOTION_BLOCKING".to_string(), Value::LongArray(fastnbt::LongArray::new(la)));
        root.insert("Heightmaps".to_string(), Value::Compound(hm));
        let mut sec_list = Vec::new();
        for y in 0..24i32 {
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
                    (i as i64)
                        .wrapping_mul(0x1234_9abc_def0_5678)
                        ^ (cx as i64).wrapping_mul(0x9e37_79b9)
                        ^ ((salt as i64) << 8)
                        ^ (y as i64)
                })
                .collect();
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        fastnbt::to_bytes(&Value::Compound(root)).unwrap()
    }

    #[test]
    fn seg_roll_with_pending_buffer_keeps_every_record() {
        // Regression for the 23,380-chunk bigworld-survival loss: the writer
        // roll seeked to `written` instead of `written - buf.len()`, writing
        // the pending append buffer a buffer-length TOO FAR and zeroing the
        // segment tail records. Batch records in the hole decoded rawlen=0
        // ("LEAK: slice fail") and blobs decompressed to garbage; the next
        // full-rewrite pack skipped them and the segment drop made the loss
        // permanent ("blob ... segment N removed from segs" forensics).
        // JJ-corpus benches never rolled a segment with a non-empty buffer
        // (whole store < 64 MiB), so only the 262k-chunk world ever hit it.
        // seg_roll (192 KiB) < BUF_CAP (1 MiB) guarantees the append buffer
        // is non-empty at EVERY roll in this test.
        let dir = temp_dir("seg-roll-buffer");
        let cfg = Cfg {
            seg_roll: 192 * 1024,
            compact_min: u64::MAX, // keep every segment: only the roll path is under test
            auto_pack_bytes: 0,
            ..bench_cfg()
        };
        let st = Store::open_cfg(&dir, true, cfg).unwrap();
        // ~1+ MiB of appends -> several rolls; all-unique sections so a hole
        // cannot hide behind dedup
        let n = 120u64;
        let payloads: Vec<Vec<u8>> = (0..n).map(|i| unique_payload(i as i32, 700 + i)).collect();
        for (i, p) in payloads.iter().enumerate() {
            st.write_chunk(i as u64, p, false).unwrap();
        }
        st.sync();
        st.seal();
        for (i, p) in payloads.iter().enumerate() {
            let got = st
                .read_chunk(i as u64)
                .unwrap_or_else(|| panic!("chunk {i} unreadable after segment rolls (roll-hole regression)"));
            assert!(semantic(&got, p), "chunk {i} corrupted after segment rolls");
        }
    }

    #[test]
    fn concurrent_supersede_during_pack_keeps_newest_state() {
        // Production firing (tps-bench phase B): the runtime's compaction
        // thread packed seg-11 while Paper's save thread superseded chunk
        // recipes — the pack's recs snapshot held DEAD pointers whose
        // batch_idx entries were already removed by the supersede. Moving
        // them anyway would (a) trip the batch_idx-miss poison or (b)
        // re-queue the OLD bare recipe over the newer pending one and
        // silently ROLL the chunk back (RecipeBatch is last-write-wins).
        // The loop must re-check the current chunks-map pointer.
        let dir = temp_dir("supersede-during-pack");
        let cfg = Cfg {
            compact_min: 16 * 1024,
            compact_ratio: 0.10,
            dict_train: 16,
            ..bench_cfg()
        };
        let s = Store::open_cfg(&dir, true, cfg).unwrap();
        // seed: 80 chunks committed in sealed segments
        for i in 0..80u64 {
            s.write_chunk(i, &unique_payload(i as i32, 900), false).unwrap();
        }
        s.sync();
        s.seal();
        // writer: supersede a slice of keys continuously
        let sw = Arc::clone(&s);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            // bounded: 12 rounds x 20 keys, small pauses so the packer can
            // interleave (the race window is snapshot-vs-supersede)
            for r in 0..12u64 {
                for i in 0..20u64 {
                    let _ = sw.write_chunk(i, &unique_payload(i as i32, 910 + r), false);
                }
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            stop2.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        // packer: dead-ratio compaction waves on the main thread
        for _ in 0..60 {
            s.request_pack();
            for _ in 0..4 {
                s.compact_step();
            }
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        s.sync();
        s.seal();
        // nothing may be lost, and every survivor must be valid NBT
        for i in 0..80u64 {
            let got = s
                .read_chunk(i)
                .unwrap_or_else(|| panic!("chunk {i} lost after concurrent supersede+pack"));
            assert!(
                fastnbt::from_bytes::<Value>(&got).is_ok(),
                "chunk {i} unreadable after concurrent supersede+pack"
            );
        }
    }

    #[test]
    fn dict_gc_keeps_files_inline_tails_readable() {
        // Regression for the bigworld-survival 11,819-missing loss: gc_dicts
        // used to DELETE dictionary files. Kind-2 recipe inline tails (raw
        // tail < tail_blob_min) are compressed on the hot tier WITH the
        // then-current dictionary version, and that reference lives in the
        // RECIPE — invisible to the blobs-map-only GC accounting. Deleting
        // dict-vN.bin made every such chunk permanently unreadable.
        // Scenario: train v1 -> write 60 all-unique-section chunks + rewrite
        // 40 (current tails carry dict v1; rewrites strand dead blobs so the
        // dead-ratio compaction fires) -> train v2 -> drain (drop_seg re-tiers
        // v1 blobs to plain slabs / v2 colds; gc sees no live blob on v1).
        // The FILE must survive; reads must succeed via lazy disk reload.
        let dir = temp_dir("dict-gc-tails");
        let cfg = Cfg { dict_train: 16, compact_min: 128 * 1024, ..bench_cfg() };
        let s = Store::open_cfg(&dir, true, cfg).unwrap();
        let mk = |i: u64, salt: u64| unique_payload(i as i32, salt);
        // warm-up: populate dict samples, train v1
        for i in 0..4u64 {
            s.write_chunk(9000 + i, &mk(9000 + i, 50), false).unwrap();
        }
        assert!(s.train_dict_if_needed(), "dict v1 must train");
        // batch: written while cur_dict = v1 -> inline tails carry dict v1
        for i in 0..60u64 {
            s.write_chunk(i, &mk(i, 0), false).unwrap();
        }
        // rewrite 40 with different content (still cur_dict = v1): old
        // section blobs die -> dead-ratio compaction candidate
        for i in 0..40u64 {
            s.write_chunk(i, &mk(i, 1), false).unwrap();
        }
        // more samples -> train v2 (cur_dict = v2 from here on)
        for i in 0..4u64 {
            s.write_chunk(9100 + i, &mk(9100 + i, 51), false).unwrap();
        }
        assert!(s.train_dict_if_needed(), "dict v2 must train");
        // drain: dead-ratio drop_seg compaction re-tiers the v1 blobs;
        // pack_segment's gc_dicts then sees no live blob on v1
        s.sync();
        s.seal();
        s.request_pack();
        loop {
            if !s.compact_step() {
                break;
            }
        }
        // the dict FILE survives (old code deleted it)
        let d1 = dir.join("dicts").join("dict-1.bin");
        assert!(d1.exists(), "dict-1.bin must never be deleted");
        // every chunk readable, incl. current inline tails carrying dict v1
        // (lazy disk reload)
        for i in 0..60u64 {
            let want = if i < 40 { mk(i, 1) } else { mk(i, 0) };
            let got = s
                .read_chunk(i)
                .unwrap_or_else(|| panic!("chunk {i} unreadable after dict gc"));
            assert!(semantic(&got, &want), "chunk {i} drifted");
        }
    }

    #[test]
    fn unchanged_write_skip_and_invalidation() {
        let dir = temp_dir("skip-test");
        let s = Store::open_cfg(&dir, false, Cfg::default()).unwrap();
        let p = chunk_payload(7, 42, 1);
        s.write_chunk(1, &p, false).unwrap();
        // v0.9.31: assert on the PER-STORE counter — the global
        // ST_WRITE_SKIP is shared by every Store in the process, and
        // cargo runs tests in parallel threads, so other tests' identical
        // re-saves bumped it mid-window on CI (left: 81, right: 1).
        let before = s.write_skips.load(Ordering::Relaxed);
        // identical re-save (autosave of an unchanged chunk): skipped
        s.write_chunk(1, &p, false).unwrap();
        assert_eq!(
            s.write_skips.load(Ordering::Relaxed),
            before + 1,
            "identical re-write must hit the skip path"
        );
        assert!(semantic(&s.read_chunk(1).unwrap(), &p));
        // changed content: full path again
        let p2 = chunk_payload(7, 43, 2);
        s.write_chunk(1, &p2, false).unwrap();
        assert_eq!(s.write_skips.load(Ordering::Relaxed), before + 1);
        assert!(semantic(&s.read_chunk(1).unwrap(), &p2));
        // delete invalidates the cache: same content re-writes fully
        s.delete_chunk(1).unwrap();
        assert!(s.read_chunk(1).is_none());
        s.write_chunk(1, &p, false).unwrap();
        assert_eq!(s.write_skips.load(Ordering::Relaxed), before + 1);
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
// v0.9.30: the dictionary-training pool must be arrival-order independent.
// The pre-0.9.30 pool truncated to the FIRST dict_train arrivals, so which
// payloads fs::read_dir coughed up first decided dictionary quality (a
// sandbox recycle flipped the JJ corpus between 2239 B and ~100 KB dicts).
// The bucket reservoir keys on content, so the SAME sample set fed in any
// order must train byte-identical dictionaries.
#[test]
fn dict_training_is_arrival_order_independent() {
    let base = temp_dir("dict-order");
    let cfg = Cfg { dict_train: 16, ..bench_cfg() };
    // 24 distinct >=2 KiB payloads; the store's dict_sample gate refuses
    // raws < 2048 B, so make the section payloads comfortably above it
    let mut samples: Vec<Vec<u8>> = (0..24u64)
        .map(|i| {
            let mut p = unique_payload(i as i32, 1000 + i);
            while p.len() < 4096 {
                p.extend_from_slice(&(i as u64).to_le_bytes());
            }
            p
        })
        .collect();
    let mk = |order_shift: u64| -> Vec<u8> {
        let dir = base.join(format!("s{order_shift}"));
        let s = Store::open_cfg(&dir, true, cfg.clone()).unwrap();
        // feed in different rotations: same SET, different arrival order
        let n = samples.len();
        for i in 0..n {
            let j = ((i as u64 + order_shift) % n as u64) as usize;
            s.dict_sample(&samples[j]);
        }
        assert!(s.train_dict_if_needed(), "must train");
        let (v, d) = s.current_dict().unwrap();
        assert_eq!(v, 1);
        d.as_ref().clone()
    };
    let a = mk(0);
    let b = mk(7);
    let c = mk(23);
    assert!(!a.is_empty());
    assert_eq!(a, b, "dict bytes must not depend on arrival order (rot7)");
    assert_eq!(a, c, "dict bytes must not depend on arrival order (rot23)");
    // and the set is genuinely diverse: no sample is a duplicate
    samples.dedup();
    assert_eq!(samples.len(), 24);
}
}

#[cfg(test)]
pub(crate) mod glob_slab_test {
    use super::tests::{bench_cfg, semantic, temp_dir};
    use super::*;
    use fastnbt::Value;
    use std::collections::HashMap as Map;
    use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

    /// set_var/remove_var are process-global: the two GLOB tests must not
    /// run concurrently or one's cleanup pulls the flags from under the
    /// other (observed: glob_slab_count() == 0 flake).
    static GLOB_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Shared with chunknbt's GLOB env-var tests (CSAVES_GID_RLE_DIV) —
    /// section_to_glob reads it on the pack path these tests exercise.
    pub(crate) fn tests_glob_lock() -> StdMutexGuard<'static, ()> {
        GLOB_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Dense multi-state section with PROPER vanilla packing (the shared
    /// chunk_payload helper predates the strict packing checks and falls
    /// back to whole-blob storage, which never reaches the slab path).
    /// 20-state palettes sliding over a 200-state pool: per-section palettes
    /// differ (canonical streams do not align cross-section), the content is
    /// strata-like so the GLOB gid streams do.
    fn glob_chunk_payload(i: usize) -> Vec<u8> {
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        let mut sec_list = Vec::new();
        for y in 0..24i32 {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(y as i8));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = Vec::new();
            for k in 0..20usize {
                let mut c: Map<String, Value> = Map::new();
                let id = (i * 7 + y as usize * 3 + k) % 200;
                c.insert("Name".to_string(), Value::String(format!("minecraft:st_{id}")));
                let mut props: Map<String, Value> = Map::new();
                props.insert("f".to_string(), Value::String(format!("{}", k % 2)));
                c.insert("Properties".to_string(), Value::Compound(props));
                pal.push(Value::Compound(c));
            }
            bs.insert("palette".to_string(), Value::List(pal));
            // strata voxels: bands by (x+z+y) with speckle — structured, so
            // gid streams align across sections while canonical indices do not
            let mut vox = [0u8; 4096];
            for x in 0..16usize {
                for z in 0..16usize {
                    for yy in 0..16usize {
                        let band = ((x + z + yy) / 4 + i + y as usize) % 20;
                        let speck = (x * 31 + z * 17 + yy * 7 + i) % 19 == 0;
                        vox[x * 256 + z * 16 + yy] = if speck { (band + 5) % 20 } else { band } as u8;
                    }
                }
            }
            // vanilla width for a 20-state palette: ceil(log2(20)) = 5 bits
            let vbits = ((20u32 - 1).ilog2() + 1).max(4);
            let data = chunknbt::dense_to_nostroddle(&vox, 4096, 8, vbits);
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        fastnbt::to_bytes(&Value::Compound(root)).unwrap()
    }

    /// Chunk-level SEMANTIC comparator: sections compared with
    /// sections_semantically_equal (palette-permutation and unused-state
    /// tolerant — the GLOB rebuild drops unused palette entries), every
    /// other root key exactly.
    fn chunk_semantic_glob(a: &[u8], b: &[u8]) -> bool {
        let (Ok(va), Ok(vb)) = (
            fastnbt::from_bytes::<Value>(a),
            fastnbt::from_bytes::<Value>(b),
        ) else {
            return false;
        };
        let (Value::Compound(ma), Value::Compound(mb)) = (&va, &vb) else {
            return va == vb;
        };
        if ma.len() != mb.len() {
            return false;
        }
        for (k, v) in ma {
            let Some(w) = mb.get(k) else { return false };
            if k == "sections" {
                let (Value::List(sa), Value::List(sb)) = (v, w) else { return false };
                if sa.len() != sb.len() {
                    return false;
                }
                for (s1, s2) in sa.iter().zip(sb.iter()) {
                    if !chunknbt::sections_semantically_equal(s1, s2) {
                        return false;
                    }
                }
            } else if v != w {
                return false;
            }
        }
        true
    }

    #[test]
    fn glob_slab_roundtrip_and_reopen() {
        let _g = GLOB_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Integration for the 0xCB GLOB slab form under CSAVES_FORCE_GLOB=1:
        // the pack stores glob slabs, reads convert members back to
        // canonical CSEC (byte-semantically equal), and scan rebuilds the
        // blob map from the 13-byte glob slab header on reopen. The natural
        // size heuristic is gated by the real-corpus CI benches (synthetic
        // payloads cannot reproduce vanilla terrain's palette-diversity
        // statistics where the glob form measured x2.16).
        // v0.9.35: CSAVES_GLOB defaults to enabled (glob_enabled rejects
        // only the explicit "0"), so no env write is needed here — the old
        // set_var("1")/remove_var pair was a semantic no-op AND a
        // setenv/getenv UB source against parallel tests.
        set_force_glob_for_tests(Some(true));
        let dir = temp_dir("glob-slab");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let n = 120usize;
        let payloads: Vec<Vec<u8>> = (0..n).map(glob_chunk_payload).collect();
        for (i, p) in payloads.iter().enumerate() {
            st.write_chunk(i as u64, p, false).unwrap();
        }
        st.sync();
        st.seal();
        for _ in 0..50 {
            st.request_pack();
            if !st.compact_step() {
                break;
            }
        }
        assert!(
            Store::glob_slab_count() > 0,
            "no GLOB slabs were produced under CSAVES_FORCE_GLOB=1"
        );
        for (i, p) in payloads.iter().enumerate() {
            let got = st.read_chunk(i as u64).unwrap_or_else(|| panic!("chunk {i} unreadable post-pack"));
            assert!(chunk_semantic_glob(&got, p), "chunk {i} semantically wrong post-pack");
        }
        // reopen: scan must rebuild the blob map from glob slabs
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            let got = st2
                .read_chunk(i as u64)
                .unwrap_or_else(|| panic!("chunk {i} unreadable post-reopen"));
            assert!(chunk_semantic_glob(&got, p), "chunk {i} semantically wrong post-reopen");
        }
        // the registry must be persisted alongside the glob slabs
        assert!(dir.join("states-1.bin").exists(), "states-1.bin missing");
        set_force_glob_for_tests(None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Chunk with a k=1 vertical shift-ref member (0xC9) whose target is a
    /// dense 3-state section with strictly descending counts. Under GLOB
    /// packing the target is rebuilt from the gid stream — the rebuild MUST
    /// reproduce the canonical (count-DESC) palette order, because the
    /// shift-ref stores its top plane as TARGET-CANONICAL indices.
    ///
    /// REGRESSION: glob_to_section once sorted counts ASCENDING; the rebuilt
    /// target's palette positions rotated and every shift whose target was
    /// glob-rebuilt decoded with permuted states (JJ corpus: 32 chunks where
    /// andesite appeared at palette index 0). The top plane below uses the
    /// state at canonical index 0, which the ascending sort moved to the
    /// END — pre-fix code reads it back as a different block.
    #[test]
    fn glob_rebuild_keeps_shiftref_target_order() {
        let _g = GLOB_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_force_glob_for_tests(Some(true));
        let dir = temp_dir("glob-shift");
        let st = Store::open_cfg(&dir, true, bench_cfg()).unwrap();

        // Biased pseudo-random strata-free content: r = (x*31+z*17+yy*7)%10,
        // S0 for r<6, S1 for r<9, else S2 — counts ~62/31/6% (strictly
        // descending canonical order [S0, S1, S2]) with no runs longer than
        // a few blocks, so neither RLE form is admitted and the sections
        // stay DENSE (section_to_glob refuses RLE members). The member
        // section is the target rotated down one plane (k=1 shift) with the
        // top plane repeating the target's plane 0 (S0-dominant: canonical
        // index 0, which the once-ascending rebuild sort moved to the END).
        // Vanilla palette order is deliberately non-canonical ([S2, S0, S1])
        // so canonicalization really reorders.
        let payload = |parity: usize| -> Vec<u8> {
            let sfx = format!("_p{parity}");
            let states = [
                format!("minecraft:gl_a{sfx}"),
                format!("minecraft:gl_b{sfx}"),
                format!("minecraft:gl_c{sfx}"),
            ];
            let vox_at = |lin: usize| -> usize {
                let x = lin / 256;
                let z = (lin / 16) % 16;
                let yy = lin % 16;
                let r = (x * 31 + z * 17 + yy * 7 + parity) % 10;
                if r < 6 {
                    0
                } else if r < 9 {
                    1
                } else {
                    2
                }
            };
            let mut root: Map<String, Value> = Map::new();
            root.insert("DataVersion".to_string(), Value::Int(4189));
            let mut sec_list = Vec::new();
            for y in 0..2i32 {
                let mut vox = [0u8; 4096];
                for lin in 0..4096usize {
                    // voxel values index the vanilla palette [S2, S0, S1]
                    let v = if y == 0 {
                        vox_at(lin)
                    } else {
                        vox_at((lin + 256) % 4096) // k=1 plane shift
                    };
                    vox[lin] = match v {
                        0 => 1u8, // S0 at vanilla index 1
                        1 => 2u8, // S1 at vanilla index 2
                        _ => 0u8, // S2 at vanilla index 0
                    };
                }
                let mut sec: Map<String, Value> = Map::new();
                sec.insert("Y".to_string(), Value::Byte(y as i8));
                let mut bs: Map<String, Value> = Map::new();
                // vanilla order deliberately non-canonical: [S2, S0, S1]
                bs.insert(
                    "palette".to_string(),
                    Value::List(vec![
                        Value::String(states[2].clone()),
                        Value::String(states[0].clone()),
                        Value::String(states[1].clone()),
                    ]),
                );
                let data = chunknbt::dense_to_nostroddle(&vox, 4096, 8, 4);
                bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
                sec.insert("block_states".to_string(), Value::Compound(bs));
                sec_list.push(Value::Compound(sec));
            }
            root.insert("sections".to_string(), Value::List(sec_list));
            fastnbt::to_bytes(&Value::Compound(root)).unwrap()
        };

        for i in 0..40u64 {
            st.write_chunk(i as u64, &payload(i as usize % 2), false).unwrap();
        }
        st.sync();
        st.seal();
        for _ in 0..50 {
            st.request_pack();
            if !st.compact_step() {
                break;
            }
        }
        assert!(
            Store::glob_slab_count() > 0,
            "no GLOB slabs were produced — target was never glob-rebuilt"
        );
        for i in 0..40u64 {
            let want = payload(i as usize % 2);
            let got = st
                .read_chunk(i as u64)
                .unwrap_or_else(|| panic!("chunk {i} unreadable post-pack"));
            assert!(
                chunk_semantic_glob(&got, &want),
                "chunk {i} semantically wrong post-pack (shift-ref target order broken)"
            );
        }
        drop(st);
        let st2 = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        for i in 0..40u64 {
            let want = payload(i as usize % 2);
            let got = st2
                .read_chunk(i as u64)
                .unwrap_or_else(|| panic!("chunk {i} unreadable post-reopen"));
            assert!(
                chunk_semantic_glob(&got, &want),
                "chunk {i} semantically wrong post-reopen"
            );
        }
        set_force_glob_for_tests(None);
        let _ = fs::remove_dir_all(&dir);
    }

}
#[cfg(test)]
mod repro {
    use super::*;
    use super::tests::{bench_cfg, chunk_payload, semantic, temp_dir};
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

    #[test]
    fn hot_slab_batch_roundtrip_reopen_repack() {
        // Hot-slab write batching (0x40 tier bit): a chunk's sections go to
        // disk as ONE hot-tier slab frame. The frame must (a) read back
        // byte-correctly in the SAME session, (b) survive REOPEN with
        // scan() rebuilding its members as cold=false (the bit), (c) be
        // re-groupable by the pack pass into cold frames, (d) stay readable
        // and invariant-clean through every stage.
        let dir = temp_dir("hot-slab-batch");
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let n = 30usize;
        let payloads: Vec<Vec<u8>> = (0..n)
            .map(|i| chunk_payload((i % 6) as i32, (i / 6) as i32, (i % 4) as u64))
            .collect();
        for (i, p) in payloads.iter().enumerate() {
            s.write_chunk(i as u64, p, false).unwrap();
        }
        // in-session read: every chunk readable, semantically identical
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("in-session read");
            assert!(semantic(&got, p), "in-session mismatch chunk {i}");
        }
        // the batch path actually formed hot slabs (members in slabs, not
        // regular kind-4 blobs)
        let prof = s.profile_json();
        assert!(
            prof.contains("\"members\":") && !prof.contains("\"members\":0"),
            "expected hot slab members, profile={prof}"
        );
        s.sync();
        drop(s);
        // REOPEN: scan() must rebuild the hot slab members with cold=false
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("post-reopen read");
            assert!(semantic(&got, p), "post-reopen mismatch chunk {i}");
        }
        // drain: hot frames must be re-groupable (converging, not spinning)
        s.request_pack();
        let mut steps = 0;
        while s.compact_step() {
            steps += 1;
            assert!(steps < 500, "drain did not converge ({steps} steps)");
        }
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("post-drain read");
            assert!(semantic(&got, p), "post-drain mismatch chunk {i}");
        }
        assert!(
            s.slab_invariant_scan().is_empty(),
            "slab invariant violations after drain"
        );
        drop(s);
        // reopen AFTER the drain too: cold slabs rebuild, content intact
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("final read");
            assert!(semantic(&got, p), "final mismatch chunk {i}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_slab_below_threshold_keeps_per_blob_path() {
        // chunks below HOT_SLAB_MIN_SECS must stay on the per-blob path
        // (the entry-table overhead would dominate tiny writes) and remain
        // fully readable — regression guard for the fallback branch.
        let dir = temp_dir("hot-slab-small");
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let mut root: Map<String, Value> = Map::new();
        root.insert("DataVersion".to_string(), Value::Int(4189));
        root.insert("Position".to_string(), Value::List(vec![Value::Int(1), Value::Int(0), Value::Int(1)]));
        let mut sec_list = Vec::new();
        for y in 0..4i32 {
            let mut sec: Map<String, Value> = Map::new();
            sec.insert("Y".to_string(), Value::Byte(y as i8));
            let mut bs: Map<String, Value> = Map::new();
            let mut pal = Vec::new();
            pal.push(Value::String("minecraft:stone".to_string()));
            pal.push(Value::String("minecraft:deepslate".to_string()));
            pal.push(Value::String("minecraft:air".to_string()));
            bs.insert("palette".to_string(), Value::List(pal));
            let data: Vec<i64> = (0..64)
                .map(|i| (i as i64).wrapping_mul(0x1234_9abc_def0_5678i64) ^ (y as i64))
                .collect();
            bs.insert("data".to_string(), Value::LongArray(fastnbt::LongArray::new(data)));
            sec.insert("block_states".to_string(), Value::Compound(bs));
            sec_list.push(Value::Compound(sec));
        }
        root.insert("sections".to_string(), Value::List(sec_list));
        let payload = fastnbt::to_bytes(&Value::Compound(root)).unwrap();
        // runtime write path: write_chunk_import would add isLightOn=0 (the
        // derived-light strip) and break the byte-level Value comparison
        s.write_chunk(key(1, 1), &payload, false).unwrap();
        s.sync();
        let prof = s.profile_json();
        // no hot slab frame may exist; the sections are regular blobs
        assert!(
            !prof.contains("\"members\":") || prof.contains("\"members\":0"),
            "tiny write must not batch, profile={prof}"
        );
        drop(s);
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        let got = s.read_chunk(key(1, 1)).expect("small-chunk read");
        assert!(semantic(&got, &payload));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pack-pass zstd-MT (intra-frame nbWorkers): same input + level +
    /// worker count compresses deterministically, decodes with the stock
    /// single-threaded decoder, and worker-count variants stay
    /// wire-compatible. Container-level: an MT-drained store stays
    /// semantically clean with clean slab invariants, and the census
    /// proves the MT path actually fired.
    #[test]
    fn pack_mt_deterministic_and_wire_compatible() {
        // codec level: determinism + decode compatibility across workers
        let data: Vec<u8> = (0..2_000_000u32)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(i as u8 % 251))
            .collect();
        let a = mt_compress(&data, 19, 2).expect("mt compress w2");
        let b = mt_compress(&data, 19, 2).expect("mt compress w2 again");
        assert_eq!(a, b, "zstd-MT must be deterministic per (input, level, workers)");
        let w4 = mt_compress(&data, 19, 4).expect("mt compress w4");
        for (name, frame) in [("w2", &a), ("w4", &w4)] {
            let back = zstd::stream::decode_all(std::io::Cursor::new(frame))
                .expect("stock decoder must read MT frames");
            assert_eq!(back, data, "{name} frame decoded wrong");
        }
        // container level: MT threshold low + explicit 2 workers (host-core
        // independent), 2 MiB slabs so the canon/glob variant streams clear
        // the 512 KiB admission threshold. v0.9.42: intra-frame MT stays a
        // ZSTD-path feature (the lzma2 default compresses serially inside
        // the frame; its parallelism is the outer per-stream scope) — pin
        // the zstd codec explicitly for the MT assertion.
        set_cold_codec_for_tests(Some("zstd"));
        let dir = temp_dir("pack-mt-a");
        let cfg = Cfg {
            slab_max_raw: 2 * 1024 * 1024,
            pack_mt_min: 512 * 1024,
            pack_mt_workers: 2,
            ..Cfg::default()
        };
        let s = Store::open_cfg(&dir, true, cfg).unwrap();
        let n = 70usize;
        // variant = i: every chunk unique, otherwise content-addressed dedup
        // collapses the packed candidates below the MT admission threshold
        let payloads: Vec<Vec<u8>> = (0..n)
            .map(|i| chunk_payload((i % 10) as i32, (i / 10) as i32, i as u64))
            .collect();
        for (i, p) in payloads.iter().enumerate() {
            s.write_chunk(i as u64, p, false).unwrap();
        }
        s.request_pack();
        let mut steps = 0;
        while s.compact_step() {
            steps += 1;
            assert!(steps < 500, "drain did not converge ({steps} steps)");
        }
        let census = pack_census_json();
        assert!(!census.contains("mtjobs:0"), "MT path never fired: {census}");
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("post-drain read");
            assert!(semantic(&got, p), "post-drain mismatch chunk {i}");
        }
        assert!(
            s.slab_invariant_scan().is_empty(),
            "slab invariant violations after MT drain"
        );
        drop(s);
        let s = Store::open_cfg(&dir, true, Cfg::default()).unwrap();
        for (i, p) in payloads.iter().enumerate() {
            let got = s.read_chunk(i as u64).expect("reopen read");
            assert!(semantic(&got, p), "reopen mismatch chunk {i}");
        }
        let _ = fs::remove_dir_all(&dir);
        set_cold_codec_for_tests(None);
    }

    #[test]
    fn canon_cache_lru_bounds_and_disable() {
        // The GLOB→canon result cache must (a) return the stored Arc for a
        // known hash, (b) evict LRU-style when the byte budget is exceeded,
        // (c) refuse entries bigger than the cap, (d) store nothing at cap=0
        // (CSAVES_CANON_CACHE=0 disables the cache entirely).
        let mut c = CanonCache::new(100);
        let v1 = Arc::new(vec![1u8; 40]);
        let v2 = Arc::new(vec![2u8; 40]);
        let v3 = Arc::new(vec![3u8; 40]);
        c.put(1, v1.clone());
        c.put(2, v2.clone());
        assert!(Arc::ptr_eq(&c.get(&1).unwrap(), &v1));
        assert!(Arc::ptr_eq(&c.get(&2).unwrap(), &v2));
        // v1 is now LRU; inserting v3 (total would be 120 > 100) evicts it
        c.put(3, v3.clone());
        assert!(c.get(&1).is_none(), "LRU entry must be evicted");
        assert!(Arc::ptr_eq(&c.get(&2).unwrap(), &v2));
        assert!(Arc::ptr_eq(&c.get(&3).unwrap(), &v3));
        // oversize entry: refused, cache state untouched
        let big = Arc::new(vec![4u8; 200]);
        c.put(4, big);
        assert!(c.get(&4).is_none());
        // re-put of an existing key must not double-count its size
        c.put(2, v2.clone());
        c.put(3, v3.clone());
        assert!(Arc::ptr_eq(&c.get(&2).unwrap(), &v2));
        // cap=0 disables
        let mut d = CanonCache::new(0);
        d.put(9, Arc::new(vec![5u8; 8]));
        assert!(d.get(&9).is_none());
    }
    #[test]
    fn pristine_contract_roundtrip_and_flip() {
        // Opt-in per-store cfg (no process-global env race): contracts form
        // ONLY on this store. Region-kind store (whole_mode off).
        let dir = temp_dir("pristine-contract");
        let cfg = Cfg { pristine: true, compact_min: 512 * 1024, ..bench_cfg() };
        let s = Store::open_cfg(&dir, true, cfg).unwrap();
        let k = key(10, 20);
        let a = chunk_payload(10, 20, 0);

        // first-ever write -> regeneration contract, nothing stored
        s.write_chunk(k, &a, false).unwrap();
        assert!(s.is_pristine(k), "first write must form a contract");
        assert!(s.read_chunk(k).is_none(), "contract reads as absent (regenerate)");
        assert!(s.chunk_intact(k), "contract is trivially intact (no blobs)");
        assert_eq!(s.chunk_count(), 1, "the contract is a live chunk record");

        // identical re-save (same session: last_written fast path) -> stays
        // a contract, O(1), no content storage
        s.write_chunk(k, &a, false).unwrap();
        assert!(s.is_pristine(k), "identical re-save must not flip the contract");

        // whole-mode write path (entities/POI) must NEVER form contracts
        let ke = key(30, 40);
        let e = chunk_payload(30, 40, 1);
        s.write_chunk(ke, &e, true).unwrap();
        assert!(!s.is_pristine(ke), "whole-mode writes must stay normal");
        assert!(s.read_chunk(ke).is_some(), "whole-mode content is stored");

        // a contract that is never flipped must survive reopen as a contract
        let kp = key(50, 60);
        let p = chunk_payload(50, 60, 2);
        s.write_chunk(kp, &p, false).unwrap();
        assert!(s.is_pristine(kp));

        // mutation flips the contract to normal storage through the
        // ordinary supersede pipeline
        let b = chunk_payload(10, 20, 7);
        s.write_chunk(k, &b, false).unwrap();
        assert!(!s.is_pristine(k), "different content must flip to normal");
        let got = s.read_chunk(k).expect("flipped chunk must read back");
        assert!(semantic(&got, &b), "flipped chunk must be the new content");

        // sync + reopen: the contract persists (scan parses mode 7)
        s.sync();
        drop(s);
        let cfg2 = Cfg { pristine: true, compact_min: 512 * 1024, ..bench_cfg() };
        let s2 = Store::open_cfg(&dir, true, cfg2).unwrap();
        assert_eq!(s2.chunk_count(), 3, "all records survive reopen");
        // the flipped key is normal forever; the never-touched contract
        // persists and reads absent; the whole-mode chunk reads back
        assert!(!s2.is_pristine(k), "flipped key must stay normal after reopen");
        assert!(s2.is_pristine(kp), "contract must persist across reopen");
        assert!(s2.read_chunk(kp).is_none(), "contract reads absent after reopen");
        let got2 = s2.read_chunk(ke).expect("normal chunk reads after reopen");
        assert!(semantic(&got2, &e), "normal chunk survives reopen");

        // post-restart identical re-save hits the pristine fast path and
        // STAYS a contract (the restart would otherwise re-store it)
        let a2 = chunk_payload(10, 20, 99);
        s2.write_chunk(key(11, 21), &a2, false).unwrap();
        assert!(s2.is_pristine(key(11, 21)));
        s2.sync();
        drop(s2);
        let cfg3 = Cfg { pristine: true, compact_min: 512 * 1024, ..bench_cfg() };
        let s3 = Store::open_cfg(&dir, true, cfg3).unwrap();
        s3.write_chunk(key(11, 21), &a2, false).unwrap();
        assert!(s3.is_pristine(key(11, 21)), "post-restart identical re-save must stay pristine");
        assert!(s3.read_chunk(key(11, 21)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pristine_off_by_default_and_import_paths_exempt() {
        // default cfg: no contracts, everything stored (the CI gates run
        // with pristine OFF — behavior must be bit-identical to 0.9.43)
        let dir = temp_dir("pristine-off");
        let s = Store::open_cfg(&dir, true, bench_cfg()).unwrap();
        let k = key(5, 6);
        let a = chunk_payload(5, 6, 0);
        s.write_chunk(k, &a, false).unwrap();
        assert!(!s.is_pristine(k), "pristine must be OFF by default");
        assert!(s.read_chunk(k).is_some(), "content stored when pristine off");

        // import/strip paths never form contracts even with pristine on
        let dir2 = temp_dir("pristine-import-exempt");
        let cfg = Cfg { pristine: true, compact_min: 512 * 1024, ..bench_cfg() };
        let s2 = Store::open_cfg(&dir2, true, cfg).unwrap();
        let k2 = key(7, 8);
        let b = chunk_payload(7, 8, 0);
        s2.write_chunk_import(k2, &b).unwrap();
        assert!(!s2.is_pristine(k2), "import writes must not form contracts");
        assert!(s2.read_chunk(k2).is_some(), "import content is stored");
        let c = chunk_payload(7, 8, 0);
        s2.write_chunk_stripped(key(9, 10), &c, true).unwrap();
        assert!(!s2.is_pristine(key(9, 10)), "strip writes must not form contracts");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
