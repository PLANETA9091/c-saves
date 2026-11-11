//! Engine: store registry keyed by container dir + the save-cycle coordinator.

use crate::store::Store;
use parking_lot::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub static ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct Registry {
    stores: RwLock<Vec<(Arc<Store>, bool)>>, // store + whole_mode (entities/poi)
    by_dir: Mutex<std::collections::HashMap<String, usize>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { stores: RwLock::new(Vec::new()), by_dir: Mutex::new(std::collections::HashMap::new()) }
    }

    pub fn attach(&self, kind: &str, level: &str, container_dir: &str) -> usize {
        let mut by_dir = self.by_dir.lock();
        if let Some(idx) = by_dir.get(container_dir) {
            return *idx;
        }
        let path = PathBuf::from(container_dir);
        let dict_enabled = std::env::var("CSAVES_DICT").map(|v| v != "0").unwrap_or(true);
        let store = Store::open(&path, dict_enabled).unwrap_or_else(|e| {
            eprintln!("[c-saves] store open {container_dir} failed: {e}; using temp fallback");
            Store::open(&PathBuf::from(format!("/tmp/csaves-fallback-{kind}-{level}")), false).unwrap()
        });
        let whole_mode = kind != "region";
        if whole_mode {
            store.set_whole_mode();
            // v0.9.53: POI-store elision, opt-in per process. The poi store
            // is a DERIVED index — vanilla re-derives POIs from the chunk's
            // block states on load (PoiManager.checkConsistencyWithBlocks /
            // Yarn initForPalette; Moonrise redirects the same call), so
            // persisting poi bytes buys nothing across restarts. Entities
            // are NOT regenerable (mob spawn randomness) and never elide.
            let poi_elide = std::env::var("CSAVES_POI_ELIDE")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(false);
            if kind == "poi" && poi_elide {
                store.set_poi_elide();
                eprintln!(
                    "[c-saves] poi store: ELIDE mode (bounded session scratchpad, 0 B persisted — vanilla rebuilds from chunk block states: PoiManager.checkConsistencyWithBlocks)"
                );
            }
        }
        let chunk_count = store.chunk_count();
        let mut stores = self.stores.write();
        stores.push((store.clone(), whole_mode));
        let idx = stores.len() - 1;
        by_dir.insert(container_dir.to_string(), idx);
        eprintln!(
            "[c-saves] attached store #{idx} kind={kind} level={level} dir={container_dir} chunks={chunk_count}"
        );
        drop(by_dir);
        drop(stores);
        crate::anvil_import::maybe_spawn_import(store.clone(), container_dir, kind, chunk_count);
        idx
    }

    pub fn get(&self, idx: usize) -> Option<(Arc<Store>, bool)> {
        self.stores.read().get(idx).cloned()
    }

    pub fn all(&self) -> Vec<(Arc<Store>, bool)> {
        self.stores.read().to_vec()
    }

    pub fn sync_all(&self) {
        for (s, _) in self.all() {
            s.sync();
        }
    }

    /// Seal active segments so background compaction can reclaim dead bytes.
    pub fn seal_all(&self) {
        for (s, _) in self.all() {
            s.seal();
        }
    }

    pub fn train_dicts(&self) {
        for (s, _) in self.all() {
            s.train_dict_if_needed();
        }
    }

    pub fn compact_step(&self) -> bool {
        let mut did = false;
        for (s, _) in self.all() {
            did |= s.compact_step();
        }
        did
    }
}

// latency meters (micro-benchmark counters; printed by the bench harness)
static READ_LAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static READ_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITE_LAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITE_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AUTOSAVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// v0.9.63: per-op latency HISTOGRAMS for the live path (n_read_chunk /
// n_write_chunk drive note_read_latency / note_write_latency). Averages hid
// the tail; the player-visible question is p50/p90/p99. Bucketing: linear
// microsecond slots up to 1023 µs — a live chunk read lands in the exact-µs
// zone — then floor-log2 buckets up to 2^31 µs. p-values below 1 ms are
// therefore exact; above, the reported number is the bucket's LOWER bound
// (a conservative "at least this fast" claim).
const LIN_SLOTS: usize = 1024; // us 0..=1023 (slot == us)
const LOG_SLOTS: usize = 22; // 2^10 ..= 2^31 boundaries
const OVERFLOW_SLOT: usize = LIN_SLOTS + LOG_SLOTS; // > 2^31 µs
const TOTAL_SLOTS: usize = OVERFLOW_SLOT + 1;

pub struct LatPct {
    pub n: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

struct Hist {
    slots: Box<[std::sync::atomic::AtomicU64]>,
    max: std::sync::atomic::AtomicU64,
}

impl Hist {
    fn new() -> Self {
        Hist {
            slots: (0..TOTAL_SLOTS)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            max: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn slot_of(us: u64) -> usize {
        if us < LIN_SLOTS as u64 {
            us as usize
        } else {
            let k = 63 - us.leading_zeros() as usize; // floor(log2(us)), us >= 1024 -> k >= 10
            if k >= 10 + LOG_SLOTS {
                OVERFLOW_SLOT
            } else {
                LIN_SLOTS + (k - 10)
            }
        }
    }

    /// Lower bound of the µs range a slot represents (exact below 1 ms).
    fn slot_lower(slot: usize) -> u64 {
        if slot < LIN_SLOTS {
            slot as u64
        } else if slot < OVERFLOW_SLOT {
            1u64 << (slot - LIN_SLOTS + 10)
        } else {
            1u64 << (10 + LOG_SLOTS)
        }
    }

    fn record(&self, us: u64) {
        self.slots[Self::slot_of(us)].fetch_add(1, Ordering::Relaxed);
        self.max.fetch_max(us, Ordering::Relaxed);
    }

    fn percentile(&self, q: u64) -> u64 {
        // q in percent (50, 90, 99); rank is 1-indexed: ceil(q/100 * n)
        let mut n = 0u64;
        for s in &self.slots {
            n += s.load(Ordering::Relaxed);
        }
        if n == 0 {
            return 0;
        }
        let rank = ((q as u128 * n as u128) + 99) / 100; // ceil, u128-safe
        let mut cum = 0u64;
        for (idx, s) in self.slots.iter().enumerate() {
            cum += s.load(Ordering::Relaxed);
            if (cum as u128) >= rank {
                return Self::slot_lower(idx);
            }
        }
        Self::slot_lower(OVERFLOW_SLOT)
    }

    fn snapshot(&self, n_hint: u64) -> LatPct {
        LatPct {
            n: n_hint,
            p50: self.percentile(50),
            p90: self.percentile(90),
            p99: self.percentile(99),
            max: self.max.load(Ordering::Relaxed),
        }
    }
}

static READ_HIST: std::sync::OnceLock<Hist> = std::sync::OnceLock::new();
static WRITE_HIST: std::sync::OnceLock<Hist> = std::sync::OnceLock::new();

fn read_hist() -> &'static Hist {
    READ_HIST.get_or_init(Hist::new)
}
fn write_hist() -> &'static Hist {
    WRITE_HIST.get_or_init(Hist::new)
}

pub fn note_read_latency(us: u64) {
    READ_LAT.fetch_add(us, Ordering::Relaxed);
    READ_N.fetch_add(1, Ordering::Relaxed);
    read_hist().record(us);
}
pub fn note_write_latency(us: u64) {
    WRITE_LAT.fetch_add(us, Ordering::Relaxed);
    WRITE_N.fetch_add(1, Ordering::Relaxed);
    write_hist().record(us);
}
pub fn note_autosave() {
    AUTOSAVES.fetch_add(1, Ordering::Relaxed);
}

/// v0.9.63: percentile snapshot of the live-path latency meters.
/// One JNI call returns the whole picture: reads and writes, each with
/// n / p50 / p90 / p99 / max (µs; 0 where no sample exists).
pub fn read_lat() -> LatPct {
    read_hist().snapshot(READ_N.load(Ordering::Relaxed))
}
pub fn write_lat() -> LatPct {
    write_hist().snapshot(WRITE_N.load(Ordering::Relaxed))
}

pub fn latency_snapshot() -> (u64, u64, u64, u64, u64) {
    let rn = READ_N.load(Ordering::Relaxed);
    let wn = WRITE_N.load(Ordering::Relaxed);
    (
        if rn > 0 { READ_LAT.load(Ordering::Relaxed) / rn } else { 0 },
        rn,
        if wn > 0 { WRITE_LAT.load(Ordering::Relaxed) / wn } else { 0 },
        wn,
        AUTOSAVES.load(Ordering::Relaxed),
    )
}

/// Background maintenance: periodic fsync + compaction. Runs detached from
/// the server main thread; bounded work per tick to never steal IO bandwidth.
pub fn spawn_maintenance(reg: Arc<Registry>) {
    std::thread::Builder::new()
        .name("csaves-maint".into())
        .spawn(move || {
            let mut last_sync = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                let elapsed = last_sync.elapsed().as_millis() as u64;
                if elapsed > 20_000 {
                    reg.sync_all();
                    reg.seal_all();
                    last_sync = std::time::Instant::now();
                }
                let _ = reg.compact_step();
            }
        })
        .ok();
}

#[cfg(test)]
mod lat_tests {
    use super::*;

    #[test]
    fn exact_us_below_one_ms() {
        let h = Hist::new();
        for &v in &[5u64, 5, 5, 250, 250, 900, 1023] {
            h.record(v);
        }
        // n=7; ranks: p50 -> ceil(3.5)=4 -> sorted[3]=250 (exact);
        // with a 7-sample histogram p90/p99 collapse onto the max (1023)
        let s = h.snapshot(7);
        assert_eq!(s.n, 7);
        assert_eq!(s.p50, 250, "p50 below 1 ms must be the exact µs value");
        assert_eq!(s.p90, 1023);
        assert_eq!(s.p99, 1023);
        assert_eq!(s.max, 1023);
    }

    #[test]
    fn log2_buckets_report_lower_bounds() {
        let h = Hist::new();
        h.record(2000); // [1024, 2048) -> lower bound 1024
        h.record(5_000_000); // [2^22, 2^23) -> lower bound 4_194_304
        let s = h.snapshot(2);
        assert_eq!(s.p50, 1024, "n=2: p50 rank 1 = the smaller sample's bucket");
        assert_eq!(s.p99, 4_194_304, "n=2: p99 rank 2 = the larger sample's bucket");
        assert_eq!(s.max, 5_000_000, "max stays exact at any scale");
    }

    #[test]
    fn empty_hist_is_all_zero() {
        let s = Hist::new().snapshot(0);
        assert_eq!((s.n, s.p50, s.p90, s.p99, s.max), (0, 0, 0, 0, 0));
    }

    #[test]
    fn overflow_slot_never_panics() {
        let h = Hist::new();
        h.record(u32::MAX as u64);
        h.record(u64::MAX);
        let s = h.snapshot(2);
        assert_eq!(s.max, u64::MAX);
        assert_eq!(s.p50, 1u64 << 31, "n=2: rank 1 = the 2^31-boundary sample");
    }

    #[test]
    fn live_meters_surface_percentiles() {
        // the natives drive note_*; one value each so the global histograms
        // have deterministic content as long as no other test records reads
        let before = read_lat().n;
        note_read_latency(7);
        let s = read_lat();
        assert_eq!(s.n, before + 1);
        assert!(s.p50 <= 7, "a 7 µs sample cannot push p50 above 7");
        let wb = write_lat().n;
        note_write_latency(11);
        assert_eq!(write_lat().n, wb + 1);
    }
}
