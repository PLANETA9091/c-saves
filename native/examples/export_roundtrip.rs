//! export_roundtrip: the export path closes (v0.9.49) — measured on a REAL
//! Anvil corpus (dimension folder with region/, e.g. bench-corpus/dim).
//!
//! Before 0.9.49 the exporter silently dropped every pristine regeneration
//! contract (`read_chunk` → None BY DESIGN → `continue`): a partial export
//! was indistinguishable from data loss. The exporter now classifies every
//! key (`ExportReport`: chunks + pristine_skipped + lost == keys) and this
//! probe pins the two round-trip invariants end-to-end through the
//! production import/export pair:
//!
//!   Phase A (pristine OFF, both sides) — full-losslessness: container →
//!   Anvil → container re-import keeps every key with a byte-identical
//!   payload (the production `chunknbt::hash_of` identity).
//!
//!   Phase B (pristine + pristine_import ON, both sides) — convergence:
//!   the exported Anvil is exactly the honestly-stored part of A
//!   (chunks == stored, pristine_skipped == contracted), the contracted
//!   keys are absent from it (vanilla regenerates them from the seed), and
//!   re-importing it contracts exactly those bytes: B's contract set ==
//!   A's stored set with B's raw legend == hash of A's stored payload.
//!
//! Usage: cargo run --release --example export_roundtrip -- <dim-dir>
//! Exit code 0 = both invariants hold (CI-able).

use c_saves::anvil;
use c_saves::chunknbt::hash_of;
use c_saves::store::{Cfg, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn copy_dir(src: &Path, dst: &Path) {
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
    let work = std::env::temp_dir().join(format!(
        "csaves-exp-rt-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    work
}

/// payload hash per key for a store (contracts report None and are absent
/// from the map — the caller tracks them separately via is_pristine).
fn payload_hashes(store: &Store) -> BTreeMap<u64, u128> {
    let mut m = BTreeMap::new();
    for k in store.keys() {
        if let Some(p) = store.read_chunk(k) {
            m.insert(k, hash_of(&p));
        }
    }
    m
}

fn classify(store: &Store) -> (Vec<u64>, Vec<u64>) {
    let mut stored = vec![];
    let mut contracted = vec![];
    for k in store.keys() {
        if store.is_pristine(k) {
            contracted.push(k);
        } else {
            stored.push(k);
        }
    }
    (stored, contracted)
}

/// Valid `NbtIo.write`-form payload with distinct content per DataVersion —
/// the synthetic live-write flip the mixed-state export exercises.
fn flip_payload(data_version: i32) -> Vec<u8> {
    let mut out = vec![10u8, 0, 0]; // TAG_Compound, empty root name
    out.push(3u8); // TAG_Int
    out.extend_from_slice(&11u16.to_be_bytes());
    out.extend_from_slice(b"DataVersion");
    out.extend_from_slice(&data_version.to_be_bytes());
    out.push(0u8); // TAG_End
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));

    // ---- Phase A: pristine OFF — the exporter must be fully lossless ----
    let wa = work_dir("a");
    let base = Cfg::from_env();
    let sa = Store::open_cfg(&wa.join("store"), true, base).unwrap();
    let staging = wa.join("staging-region");
    copy_dir(&dim.join("region"), &staging);
    let irep = anvil::import_dir(&staging, &sa).expect("import ok");
    sa.sync();
    let (stored_a, contracted_a) = classify(&sa);
    assert!(contracted_a.is_empty(), "flag-off import must not contract");
    let hashes_a = payload_hashes(&sa);

    let out_a = wa.join("export");
    let rep_a = anvil::export_dir(&sa, &out_a).expect("export ok");
    assert_eq!(rep_a.chunks, hashes_a.len(), "flag-off export writes every key");
    assert_eq!(rep_a.pristine_skipped, 0, "flag-off export has nothing contracted");
    assert_eq!(rep_a.lost, 0, "flag-off export loses nothing");
    assert_eq!(rep_a.chunks + rep_a.pristine_skipped + rep_a.lost, sa.keys().len());

    let wb = work_dir("b");
    let sb = Store::open_cfg(&wb.join("store"), true, Cfg::from_env()).unwrap();
    let irep2 = anvil::import_dir(&out_a, &sb).expect("re-import ok");
    assert_eq!(irep2.chunks, irep.chunks, "re-import sees the full world");
    let hashes_b = payload_hashes(&sb);
    assert_eq!(hashes_b.len(), hashes_a.len(), "same key count after roundtrip");
    let mut mismatches = 0usize;
    for (k, h) in &hashes_a {
        if hashes_b.get(k) != Some(h) {
            mismatches += 1;
            eprintln!("[A] payload mismatch at key {k:016x}");
        }
    }
    assert_eq!(mismatches, 0, "flag-off roundtrip must be byte-identical");
    println!(
        "A flag-off: {} chunks imported, exported {} regions/{} chunks, re-imported byte-identical {}/{}",
        irep.chunks,
        rep_a.regions,
        rep_a.chunks,
        hashes_a.len(),
        hashes_a.len()
    );

    // ---- Phase B: pristine IMPORT contracts — the convergence invariant ----
    let wc = work_dir("c");
    let cfg = Cfg { pristine: true, pristine_import: true, ..Cfg::from_env() };
    let sc = Store::open_cfg(&wc.join("store"), true, cfg).unwrap();
    let staging_c = wc.join("staging-region");
    copy_dir(&dim.join("region"), &staging_c);
    let irep_c = anvil::import_dir(&staging_c, &sc).expect("import ok");
    sc.sync();
    let (stored_c, contracted_c) = classify(&sc);
    // make the state MIXED (the real production picture: pregen + player
    // edits): flip every 91st contracted key through the LIVE path with a
    // synthetic different payload — those chunks store honestly and the
    // export must materialize exactly them.
    let mut flipped_payloads: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for (n, k) in contracted_c.iter().step_by(91).take(8).enumerate() {
        let p = flip_payload(4000 + n as i32);
        sc.write_chunk(*k, &p, false).expect("live flip ok");
        assert!(!sc.is_pristine(*k), "flip must supersede the contract");
        flipped_payloads.insert(*k, p);
    }
    sc.sync();
    let (stored_c, contracted_c) = classify(&sc);
    assert_eq!(stored_c.len(), flipped_payloads.len());
    let hashes_c = payload_hashes(&sc);
    for (k, p) in &flipped_payloads {
        assert_eq!(hashes_c[k], hash_of(p), "stored flip must read back byte-identical");
    }
    assert_eq!(hashes_c.len(), stored_c.len());
    println!(
        "B import-contracts: {} chunks + {} live flips -> contracted {} stored {} (import ok={})",
        irep_c.chunks,
        flipped_payloads.len(),
        contracted_c.len(),
        stored_c.len(),
        irep_c.failed == 0
    );

    let out_c = wc.join("export");
    let rep_c = anvil::export_dir(&sc, &out_c).expect("export ok");
    assert_eq!(rep_c.chunks, stored_c.len(), "export materializes exactly the stored part");
    assert_eq!(
        rep_c.pristine_skipped, contracted_c.len(),
        "contracts are counted as absent-by-design"
    );
    assert_eq!(rep_c.lost, 0);
    assert_eq!(rep_c.chunks + rep_c.pristine_skipped + rep_c.lost, sc.keys().len());

    let wd = work_dir("d");
    let cfgd = Cfg { pristine: true, pristine_import: true, ..Cfg::from_env() };
    let sd = Store::open_cfg(&wd.join("store"), true, cfgd).unwrap();
    let irep_d = anvil::import_dir(&out_c, &sd).expect("re-import ok");
    sd.sync();
    assert_eq!(irep_d.chunks, stored_c.len(), "re-import sees only the stored part");
    let (stored_d, contracted_d) = classify(&sd);
    // every stored-A key re-contracts in B (import semantics), nothing else exists
    let mut legend_miss = 0usize;
    for k in &stored_c {
        assert!(
            contracted_d.contains(k),
            "stored-A key {:016x} must re-contract in B",
            k
        );
        let (raw, _) = sd.pristine_legend(*k).expect("legend present");
        if raw != hashes_c[k] {
            legend_miss += 1;
            eprintln!("[B] legend mismatch at key {k:016x}");
        }
    }
    assert_eq!(legend_miss, 0, "every B legend must re-derive from the exported bytes");
    assert_eq!(stored_d.len(), 0, "re-imported stored content must contract, not store");
    for k in &contracted_c {
        assert!(
            !sd.keys().contains(k),
            "contracted-A key {:016x} must be absent from the export and from B",
            k
        );
    }
    println!(
        "B convergence: export {} chunks + {} pristine_skipped; re-import contracted {}/{} with exact legends; contracted-A keys absent: {}",
        rep_c.chunks,
        rep_c.pristine_skipped,
        contracted_d.len(),
        stored_c.len(),
        contracted_c.len()
    );

    println!("=== export roundtrip: PASS ===");

    for w in [&wa, &wb, &wc, &wd] {
        let _ = std::fs::remove_dir_all(w);
    }
}
