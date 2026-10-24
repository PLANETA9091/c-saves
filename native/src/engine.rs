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

pub fn note_read_latency(us: u64) {
    READ_LAT.fetch_add(us, Ordering::Relaxed);
    READ_N.fetch_add(1, Ordering::Relaxed);
}
pub fn note_write_latency(us: u64) {
    WRITE_LAT.fetch_add(us, Ordering::Relaxed);
    WRITE_N.fetch_add(1, Ordering::Relaxed);
}
pub fn note_autosave() {
    AUTOSAVES.fetch_add(1, Ordering::Relaxed);
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
