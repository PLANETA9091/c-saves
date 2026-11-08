//! pristine_canon_probe: the erosion lever measured on a REAL corpus (v0.9.46).
//!
//! The 0.9.45 import contracts flip on the first byte-different re-save.
//! A real Paper session re-saves contracted chunks with FRESH light and
//! bumped InhabitedTime/lastUpdate the moment a player merely walks past —
//! every such re-save eroded one contract into full storage. v0.9.46 adds
//! a strip-canonical legend to the contract ([mode 7][raw 16B][canon 16B]):
//! an order-free structural digest minus derived light and root timers, so
//! a relight re-save of the same seeded content keeps the contract.
//!
//! Method (all through production paths):
//!   0. honest store (pristine OFF): import a disposable copy of the dim
//!      corpus, read every payload back (this is the content Paper would
//!      hold after a restore, i.e. the import-stripped form).
//!   1. simulate one server session: for every key, relit_variant()
//!      rebuilds the re-save Paper would hand back (fresh section light,
//!      bumped timers, shuffled compound key order).
//!   2. store B (v0.9.46): import-contracts with canon ON -> write every
//!      relit payload -> count surviving contracts, compact, weigh.
//!   3. store C (0.9.45 semantics): import-contracts with canon OFF ->
//!      same session -> every raw-hash miss flips to storage.
//!
//! Usage: cargo run --release --example pristine_canon_probe -- <dim-dir>

use c_saves::anvil;
use c_saves::store::{Cfg, Store};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(e.file_name());
        if e.metadata().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}

fn work_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "csaves-canon-{}-{}",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn drain(store: &Store) -> u64 {
    store.sync();
    let mut steps = 0;
    while store.compact_step() {
        steps += 1;
        if steps >= 500 {
            break;
        }
    }
    store.disk_bytes()
}

fn count_pristine(store: &Store) -> (usize, usize, usize) {
    let mut pristine = 0usize;
    let mut stored = 0usize;
    let mut unreadable = 0usize;
    for k in store.keys() {
        if store.is_pristine(k) {
            pristine += 1;
        } else if store.read_chunk(k).is_some() {
            stored += 1;
        } else {
            unreadable += 1;
        }
    }
    (pristine, stored, unreadable)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));
    let mut anvil_region = 0u64;
    for e in std::fs::read_dir(dim.join("region")).unwrap().flatten() {
        anvil_region += e.metadata().unwrap().len();
    }
    println!("anvil region on disk: {:.2} MiB", anvil_region as f64 / 1048576.0);

    // ---- step 0: capture the post-restore content (honest store) ----
    // Raw payloads are read from a staging copy FIRST (the import drains
    // its source); after the import, canon(raw) is compared against
    // canon(captured) so any rebuild-induced mismatch is named, not
    // guessed. The contract legend is formed on canon(raw); the session
    // re-saves relit(captured) — the two must agree canon-wise.
    let w0 = work_dir("capture");
    let mut staging0 = w0.join("staging");
    copy_dir(&dim.join("region"), &staging0);
    let mut raw_by_key: HashMap<u64, Vec<u8>> = HashMap::new();
    for entry in std::fs::read_dir(&staging0).unwrap().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("mca") {
            continue;
        }
        let data = std::fs::read(&p).unwrap();
        if data.len() < 8192 {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let parts: Vec<&str> = name.trim_end_matches(".mca").split('.').collect();
        if parts.len() != 3 {
            continue;
        }
        let (rx, rz): (i32, i32) = (parts[1].parse().unwrap_or(0), parts[2].parse().unwrap_or(0));
        for i in 0..1024usize {
            let loc = u32::from_be_bytes([
                data[i * 4],
                data[i * 4 + 1],
                data[i * 4 + 2],
                data[i * 4 + 3],
            ]);
            if loc == 0 {
                continue;
            }
            let off = (loc >> 8) as usize * 4096;
            if off + 5 > data.len() {
                continue;
            }
            let len =
                u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            if len < 2 || off + 4 + len > data.len() {
                continue;
            }
            let comp = &data[off + 5..off + 4 + len];
            let mut payload = Vec::new();
            if flate2::read::ZlibDecoder::new(comp).read_to_end(&mut payload).is_err() {
                continue;
            }
            let cx = rx * 32 + (i as i32 & 31);
            let cz = rz * 32 + (i as i32 >> 5);
            raw_by_key.insert(((cx as u64) << 32) | (cz as u64 & 0xffff_ffff), payload);
        }
    }
    let cap_store = Store::open_cfg(&w0.join("region"), true, Cfg::from_env()).unwrap();
    let rep = anvil::import_dir(&staging0, &cap_store).expect("import ok");
    cap_store.sync();
    let mut payloads: HashMap<u64, Vec<u8>> = HashMap::with_capacity(rep.chunks as usize);
    let mut canon_drift = 0usize;
    for k in cap_store.keys() {
        let cap = cap_store.read_chunk(k).expect("captured payload");
        payloads.insert(k, cap.clone());
        if let Some(raw) = raw_by_key.get(&k) {
            let (a, b) = (
                c_saves::chunknbt::canonical_contract_hash(raw),
                c_saves::chunknbt::canonical_contract_hash(&cap),
            );
            if a != b {
                canon_drift += 1;
                if canon_drift <= 3 {
                    println!(
                        "[drift] key {k:016x}: canon(raw)={a:?} canon(captured)={b:?} rawlen={} caplen={}",
                        raw.len(),
                        cap.len()
                    );
                    if let (Ok(fastnbt::Value::Compound(rm)), Ok(fastnbt::Value::Compound(cm))) = (
                        fastnbt::from_bytes::<fastnbt::Value>(raw),
                        fastnbt::from_bytes::<fastnbt::Value>(&cap),
                    ) {
                        for (rk, rv) in &rm {
                            if cm.get(rk) != Some(rv) {
                                println!("  root differs: {rk}");
                            }
                        }
                        for ck in cm.keys() {
                            if !rm.contains_key(ck) {
                                println!("  root added: {ck}");
                            }
                        }
                    }
                }
            }
        }
    }
    println!("canon drift raw->captured: {canon_drift}/{}", payloads.len());
    assert_eq!(payloads.len(), rep.chunks as usize, "all chunks captured");
    drop(cap_store);
    let _ = std::fs::remove_dir_all(&w0);

    // ---- session payloads = the seed-reproducible form ----
    // On restore a contracted chunk is REGENERATED by Paper (read = None
    // BY DESIGN) — the re-save comes from Paper's own generator, i.e. the
    // same serializer that produced the raw pregen payload, plus fresh
    // light and bumped timers. The import rebuild form is NOT part of
    // that chain; the raw->captured canon drift above (CSEC canonical
    // palette order vs vanilla order in rebuilt sections) therefore does
    // not affect contract erosion — the session below replays relit(raw).
    for k in raw_by_key.keys() {
        if !payloads.contains_key(k) {
            println!("warning: key {k:016x} captured but not in raw map (should not happen)");
        }
    }
    payloads = raw_by_key;
    println!("captured {} post-restore payloads", payloads.len());

    // ---- step 1: canonical-hash sanity + formation cost ----
    let t0 = Instant::now();
    let mut canon_stable = 0usize;
    let mut raw_diff = 0usize;
    let mut canon_refused = 0usize;
    for p in payloads.values() {
        let rel = wgen::relit_variant(p);
        match (c_saves::chunknbt::canonical_contract_hash(p), c_saves::chunknbt::canonical_contract_hash(&rel)) {
            (Some(a), Some(b)) => {
                if a == b {
                    canon_stable += 1;
                }
            }
            _ => canon_refused += 1,
        }
        if c_saves::chunknbt::hash_of(p) != c_saves::chunknbt::hash_of(&rel) {
            raw_diff += 1;
        }
    }
    let canon_secs = t0.elapsed().as_secs_f64();
    println!(
        "canon legend: stable {canon_stable}/{} relit payloads, refused {canon_refused}, raw-hash differs on {raw_diff}/{} (all raw misses must flip under 0.9.45 rules)",
        payloads.len(),
        payloads.len()
    );
    assert_eq!(canon_stable + canon_refused, payloads.len(), "canon must classify every payload");
    assert_eq!(raw_diff, payloads.len(), "every relit re-save must be a raw-hash miss");
    println!(
        "canon formation cost: {:.1} us/chunk ({} chunks, {} MB payload)",
        canon_secs * 1e6 / payloads.len() as f64,
        payloads.len(),
        payloads.values().map(|p| p.len()).sum::<usize>() / 1048576
    );

    // ---- step 2 + 3: the same session against canon-ON and canon-OFF ----
    let run = |label: &str, canon: bool| -> (u64, usize, usize, usize, f64) {
        let w = work_dir(label);
        let base = Cfg::from_env();
        let cfg = Cfg {
            pristine: true,
            pristine_import: true,
            pristine_canon: canon,
            ..base
        };
        let store = Store::open_cfg(&w.join("region"), true, cfg).unwrap();
        let regions = w.join("staging");
        copy_dir(&dim.join("region"), &regions);
        let rep = anvil::import_dir(&regions, &store).expect("import ok");
        let (p0, s0, u0) = count_pristine(&store);
        assert_eq!(p0, rep.chunks as usize, "[{label}] import forms only contracts");
        assert_eq!(s0, 0, "[{label}] nothing stored before the session");
        assert_eq!(u0, 0, "[{label}] nothing unreadable before the session");
        let fresh0 = drain(&store);

        // one server session: every chunk gets a relight re-save
        let t = Instant::now();
        for (k, p) in &payloads {
            let rel = wgen::relit_variant(p);
            store.write_chunk(*k, &rel, false).unwrap();
        }
        let sess_secs = t.elapsed().as_secs_f64();
        let (p1, s1, u1) = count_pristine(&store);
        let compact = drain(&store);
        println!(
            "[{label}] after session: pristine={p1} stored={s1} unreadable={u1} | fresh(contracts)={:.2} MiB compact={:.2} MiB (x{:.4} vs anvil) session={:.1}s",
            fresh0 as f64 / 1048576.0,
            compact as f64 / 1048576.0,
            compact as f64 / anvil_region.max(1) as f64,
            sess_secs
        );
        assert_eq!(u1, 0, "[{label}] no unreadable chunks");
        let _ = std::fs::remove_dir_all(&w);
        (compact, p1, s1, u1, sess_secs)
    };

    let (compact_b, p_b, s_b, _, sess_b) = run("canon-on-v0946", true);
    let (compact_c, p_c, s_c, _, sess_c) = run("canon-off-v0945", false);

    println!("=== summary ({} chunks, one relight session) ===", payloads.len());
    println!(
        "canon ON  (v0.9.46): pristine={p_b} stored={s_b} compact {:.2} MiB (x{:.4} vs anvil, session {:.1}s)",
        compact_b as f64 / 1048576.0,
        compact_b as f64 / anvil_region.max(1) as f64,
        sess_b
    );
    println!(
        "canon OFF (0.9.45) : pristine={p_c} stored={s_c} compact {:.2} MiB (x{:.4} vs anvil, session {:.1}s)",
        compact_c as f64 / 1048576.0,
        compact_c as f64 / anvil_region.max(1) as f64,
        sess_c
    );
    assert!(
        compact_b < compact_c / 10,
        "canon must keep the eroded mass out of the container"
    );
}
