//! v0.9.48: pristine contract AUDIT — the regen-drift tooling frontier that
//! research §10/§11 named ("contract/regen-drift audits"). Closes it.
//!
//! A pristine contract is an ASSERTION: "this chunk is pure worldgen under
//! the seed the world will be restored with; on restore Paper regenerates
//! it (read = None BY DESIGN) and any content-bearing re-save flips it to
//! honest storage." The flip is the runtime safety net — but an operator
//! learns about a wrong assertion (wrong seed, post-drain edits, generator
//! drift) only AFTER players hit the flips. This tool moves that discovery
//! to deploy time, read-only, through the production hash chain:
//!
//!   1. INVENTORY — walk the store's recipes; count pristine contracts,
//!      strip-canonical legends (v0.9.46) vs raw-only (0.9.44/45), stored
//!      keys, container size.
//!   2. LEGEND CHECK (--corpus) — for every contract, re-derive the raw
//!      payload hash (`chunknbt::hash_of`) and the strip-canonical hash
//!      (`chunknbt::canonical_contract_hash`) from the ORIGINAL Anvil
//!      corpus bytes — the exact bytes the import hands to
//!      `write_chunk_import`, and the exact form the contract check hashes
//!      on a re-save (the legend is formed AND compared on the INCOMING
//!      payload bytes in the store write path). Classification:
//!        raw_match    — byte-identical content; contract is bulletproof;
//!        canon_match  — bytes differ, canon legend absorbs it (relight /
//!                       timers / key order / palette order class);
//!        DRIFT        — legend differs: this contract WILL flip to honest
//!                       storage on the next content-bearing re-save. The
//!                       pure-worldgen assertion is wrong for this chunk
//!                       (wrong seed, post-drain edit, generator drift) —
//!                       find out HERE, not in production.
//!   3. SESSION CHECK (--session) — replay the §10 relight session model
//!      (`wgen::relit_variant`: fresh section light, bumped
//!      InhabitedTime/lastUpdate, key-order drift — what Paper does to
//!      every chunk a player merely walks past) against the stored canon
//!      legend: kept = the legend absorbs a walking tour; FLIPPED = the
//!      contract erodes on the first session. Pure hashing — the
//!      end-to-end write-path validation of this model is §10/§11's probes
//!      (728/728 kept through real scratch stores).
//!
//! Exit code 0 = every contract legend re-derives from the corpus and the
//! session is absorbed; 1 = drift/flips found (CI-able predeploy gate);
//! 2 = usage error. No writes to the audited store, ever.
//!
//! Modes:
//!   pristine_audit <store_dir> --corpus <dim_dir> [--import] [--session]
//!                  [--limit N]
//!     --import   copy the corpus region files to a staging dir (the
//!                production import DRAINS its source — the original is
//!                never touched), drain them through `anvil::import_dir`
//!                into a FRESH store (contracts form under
//!                CSAVES_PRISTINE=1 + CSAVES_PRISTINE_IMPORT=1 — the
//!                operator's flags), then audit store vs corpus. The
//!                end-to-end operator flow: import → audit → ship.
//!     (without --import/--corpus) audit an EXISTING store: inventory
//!                only, or legend/session checks when --corpus is given.
//!
//! Usage: cargo run --release --example pristine_audit -- <store_dir> \
//!          [--corpus <dim_dir>] [--import] [--session] [--limit N]

use c_saves::anvil;
use c_saves::chunknbt;
use c_saves::store::{Cfg, Store};
use std::io::Read;
use std::path::{Path, PathBuf};

#[path = "wgen/mod.rs"]
mod wgen;

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let p = e.path();
        if p.is_file() {
            std::fs::copy(&p, dst.join(e.file_name())).unwrap();
        }
    }
}

/// Stream every chunk payload out of a region dir (zlib-inflated, exactly
/// the form `anvil::import_dir` hands to the write path). O(1) payloads in
/// memory — the audit hashes and drops. Returns the region file count.
fn parse_corpus(dim: &Path, mut emit: impl FnMut(u64, &[u8])) -> usize {
    let region = dim.join("region");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&region)
        .unwrap_or_else(|e| panic!("corpus region dir {region:?}: {e}"))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mca"))
        .collect();
    entries.sort();
    let mut files = 0usize;
    for p in entries {
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
        files += 1;
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
                u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    as usize;
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
            emit(((cx as u64) << 32) | (cz as u64 & 0xffff_ffff), &payload);
        }
    }
    files
}

struct Audit {
    contracts: usize,
    with_canon: usize,
    raw_only: usize,
    stored_keys: usize,
    corpus_chunks: usize,
    region_files: usize,
    raw_match: usize,
    canon_match: usize,
    drift: usize,
    no_evidence: usize,
    not_contracted: usize,
    canon_refused: usize,
    session_kept: usize,
    session_flipped: usize,
    relit_raw_diff: usize,
    drifted_keys: Vec<u64>,
    flipped_keys: Vec<u64>,
}

impl Audit {
    fn new() -> Self {
        Audit {
            contracts: 0,
            with_canon: 0,
            raw_only: 0,
            stored_keys: 0,
            corpus_chunks: 0,
            region_files: 0,
            raw_match: 0,
            canon_match: 0,
            drift: 0,
            no_evidence: 0,
            not_contracted: 0,
            canon_refused: 0,
            session_kept: 0,
            session_flipped: 0,
            relit_raw_diff: 0,
            drifted_keys: Vec::new(),
            flipped_keys: Vec::new(),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut store_dir: Option<PathBuf> = None;
    let mut corpus: Option<PathBuf> = None;
    let mut do_import = false;
    let mut do_session = false;
    let mut limit = 8usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus" => {
                i += 1;
                corpus = Some(PathBuf::from(args.get(i).unwrap_or_else(|| {
                    eprintln!("--corpus needs a dim dir");
                    std::process::exit(2);
                })));
            }
            "--import" => do_import = true,
            "--session" => do_session = true,
            "--limit" => {
                i += 1;
                limit = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(8);
            }
            "--help" | "-h" => {
                println!(
                    "usage: pristine_audit <store_dir> [--corpus <dim_dir>] [--import] [--session] [--limit N]"
                );
                std::process::exit(0);
            }
            other => {
                if other.starts_with('-') {
                    eprintln!("unknown flag: {other}");
                    std::process::exit(2);
                }
                if store_dir.is_none() {
                    store_dir = Some(PathBuf::from(other));
                } else {
                    eprintln!("unexpected second positional: {other}");
                    std::process::exit(2);
                }
            }
        }
        i += 1;
    }
    let store_dir = store_dir.unwrap_or_else(|| {
        eprintln!("usage: pristine_audit <store_dir> [--corpus <dim_dir>] [--import] [--session]");
        std::process::exit(2);
    });

    println!("=== pristine audit (v0.9.48) ===");
    let cfg = Cfg::from_env();
    if do_import {
        let dim = corpus.clone().unwrap_or_else(|| {
            eprintln!("--import needs --corpus <dim_dir>");
            std::process::exit(2);
        });
        if !cfg.pristine || !cfg.pristine_import {
            eprintln!(
                "--import forms contracts: set CSAVES_PRISTINE=1 and CSAVES_PRISTINE_IMPORT=1 \
                 (the operator's flags) — got pristine={} pristine_import={}",
                cfg.pristine, cfg.pristine_import
            );
            std::process::exit(2);
        }
        let existing = std::fs::read_dir(&store_dir)
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if existing > 0 {
            eprintln!(
                "refusing --import into a non-empty store ({store_dir:?}, {existing} entries) — \
                 audit counts would mix with foreign keys"
            );
            std::process::exit(2);
        }
        // the production import DRAINS its source — stage a copy, keep the
        // operator's corpus untouched for the legend check.
        let staging =
            std::env::temp_dir().join(format!("csaves_audit_staging_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        copy_dir(&dim.join("region"), &staging);
        let store = Store::open_cfg(&store_dir, true, cfg.clone()).expect("open store");
        let rep = anvil::import_dir(&staging, &store).expect("import ok");
        store.sync();
        println!(
            "imported: {} chunks from a staged copy of {dim:?} (source untouched)",
            rep.chunks
        );
        let _ = std::fs::remove_dir_all(&staging);
    }

    let store = Store::open_cfg(&store_dir, true, cfg).expect("open store");
    let mut a = Audit::new();

    // ---- pass 1: inventory (read-only recipe walk) ----
    for k in store.keys() {
        match store.pristine_legend(k) {
            Some((_, canon)) => {
                a.contracts += 1;
                if canon.is_some() {
                    a.with_canon += 1;
                } else {
                    a.raw_only += 1;
                }
            }
            None => a.stored_keys += 1,
        }
    }
    println!(
        "store: {:?} (disk {:.2} MiB) | keys {} = contracts {} (canon legends {}, raw-only {}) + stored {}",
        store_dir,
        store.disk_bytes() as f64 / 1048576.0,
        a.contracts + a.stored_keys,
        a.contracts,
        a.with_canon,
        a.raw_only,
        a.stored_keys
    );

    // ---- pass 2 + 3: legend / session checks against the corpus ----
    if let Some(dim) = corpus {
        let files = parse_corpus(&dim, |key, raw| {
            a.corpus_chunks += 1;
            let Some((rawh, canon)) = store.pristine_legend(key) else {
                a.not_contracted += 1;
                return;
            };
            // ---- legend check (the exact hashes the contract check uses) ----
            let h = chunknbt::hash_of(raw);
            if h == rawh {
                a.raw_match += 1;
            } else {
                match (canon, chunknbt::canonical_contract_hash(raw)) {
                    (Some(c), Some(ch)) if c == ch => a.canon_match += 1,
                    (Some(_), Some(_)) => {
                        a.drift += 1;
                        if a.drifted_keys.len() < 64 {
                            a.drifted_keys.push(key);
                        }
                    }
                    (Some(_), None) => {
                        // the corpus payload refuses the canonical form: this
                        // key defends only on byte-identical re-saves — the
                        // raw-only 0.9.45 behavior, i.e. a flip on relight.
                        a.canon_refused += 1;
                        a.drift += 1;
                        if a.drifted_keys.len() < 64 {
                            a.drifted_keys.push(key);
                        }
                    }
                    (None, _) => {
                        // raw-only contract (0.9.44/45 container): any
                        // byte-different re-save flips — report as drift.
                        a.drift += 1;
                        if a.drifted_keys.len() < 64 {
                            a.drifted_keys.push(key);
                        }
                    }
                }
            }
            // ---- session check (the §10 relight model) ----
            if do_session {
                let rel = wgen::relit_variant(raw);
                if chunknbt::hash_of(&rel) != rawh {
                    a.relit_raw_diff += 1;
                }
                let kept = match (canon, chunknbt::canonical_contract_hash(&rel)) {
                    (Some(c), Some(ch)) => c == ch,
                    (None, _) => false, // raw-only legend cannot absorb relight
                    (Some(_), None) => false,
                };
                if kept {
                    a.session_kept += 1;
                } else {
                    a.session_flipped += 1;
                    if a.flipped_keys.len() < 64 {
                        a.flipped_keys.push(key);
                    }
                }
            }
        });
        a.region_files = files;
        a.no_evidence = a
            .contracts
            .saturating_sub(a.raw_match + a.canon_match + a.drift);
        println!(
            "corpus: {} chunks across {} region files | not contracted (stored/foreign): {}",
            a.corpus_chunks, a.region_files, a.not_contracted
        );
        println!(
            "legend check: raw_match={} canon_match={} drift={} no_evidence={} canon_refused={}",
            a.raw_match, a.canon_match, a.drift, a.no_evidence, a.canon_refused
        );
        if do_session {
            println!(
                "session check: kept={} flipped={} (relight raw-hash differs {}/{} — all raw misses must be canon-held, §10)",
                a.session_kept, a.session_flipped, a.relit_raw_diff, a.corpus_chunks
            );
        }
        for (label, keys) in [("DRIFT", &a.drifted_keys), ("SESSION-FLIP", &a.flipped_keys)] {
            for (n, k) in keys.iter().take(limit).enumerate() {
                let (cx, cz) = ((*k >> 32) as i32, (*k & 0xffff_ffff) as i32);
                println!("  {label}[{n}] key {k:016x} chunk ({cx}, {cz})");
            }
            if keys.len() > limit {
                println!("  {label}: … and {} more", keys.len() - limit);
            }
        }
    }

    let pass = a.drift == 0 && a.session_flipped == 0;
    println!(
        "verdict: {} — {}",
        if pass { "PASS" } else { "FAIL" },
        if pass {
            "every contract legend re-derives from the corpus; the relight session is absorbed"
                .to_string()
        } else {
            format!(
                "drift={} session_flipped={} — the pure-worldgen assertion does not hold for \
                 these chunks; they will flip to honest storage on restore",
                a.drift, a.session_flipped
            )
        }
    );
    println!(
        "{{\"audit\":{{\"contracts\":{},\"with_canon\":{},\"raw_only\":{},\"stored_keys\":{},\"corpus_chunks\":{},\"raw_match\":{},\"canon_match\":{},\"drift\":{},\"canon_refused\":{},\"not_contracted\":{},\"session_kept\":{},\"session_flipped\":{},\"pass\":{}}}}}",
        a.contracts,
        a.with_canon,
        a.raw_only,
        a.stored_keys,
        a.corpus_chunks,
        a.raw_match,
        a.canon_match,
        a.drift,
        a.canon_refused,
        a.not_contracted,
        a.session_kept,
        a.session_flipped,
        pass
    );
    std::process::exit(if pass { 0 } else { 1 });
}
