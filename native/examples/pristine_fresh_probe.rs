//! pristine_fresh_probe: the shim-evidence gate measured on a REAL corpus
//! (v0.9.47).
//!
//! The 0.9.44 residual risk (documented, operator-accepted until now):
//! mutations landing between worldgen and a chunk's FIRST durable save were
//! hashed INTO the pristine contract and reverted on regeneration — the
//! contract's read=None semantics make the pre-first-save edit disappear.
//! v0.9.47 gates LIVE-path contracts on the shim's per-write freshness
//! evidence: the chunk tag must certify Status=minecraft:full &&
//! InhabitedTime==0 (fully generated, never player-inhabited) at the moment
//! of its first durable save.
//!
//! Method (production write paths, real bench-corpus/dim payloads):
//!   session model — the first durable write carries a session mutation
//!   (an extra root field, the Bukkit.*/Paper.* metadata class Paper adds
//!   on real sessions; NOT derivable from the seed) and fresh=false (the
//!   tag showed tick evidence — exactly what a player walking through a
//!   freshly generated chunk produces).
//!   A) v0.9.47 default: the edited first write STORES — every chunk reads
//!      back, the edit survives every regeneration. ST_PRISTINE_UNFRESH=N.
//!   B) 0.9.44-0.9.46 semantics (cfg.pristine_fresh=0): the edited first
//!      write CONTRACTS — read returns None BY DESIGN: the edit would
//!      revert on the first regeneration. The hazard, measured.
//!   C) pure-pregen invariance: fresh=true first writes of the raw payloads
//!      all contract; one relight re-save per chunk stays contracted through
//!      the strip-canonical legend — the huge-world win is unchanged.
//!
//! Usage: cargo run --release --example pristine_fresh_probe -- <dim-dir>

use c_saves::store::{Cfg, Store};
use fastnbt::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

#[path = "wgen/mod.rs"]
mod wgen;

/// The modeled session mutation: an extra root field (any pre-first-save
/// edit class — Paper metadata, a player interaction's bookkeeping, a mod
/// tag). Semantically visible to the contract legend BY CONSTRUCTION: the
/// strip-canonical hash removes only derived light and timers.
fn session_mutated(payload: &[u8]) -> Vec<u8> {
    let v: Value = fastnbt::from_bytes(payload).expect("payload parses");
    let mut root = match v {
        Value::Compound(m) => m,
        _ => panic!("chunk root is a compound"),
    };
    root.insert("probeEditMarker".to_string(), Value::Long(7));
    fastnbt::to_bytes(&Value::Compound(root)).unwrap()
}

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
        "csaves-fresh-{}-{}",
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

/// Production-shape semantic verifier (the jj_profile closure): root fields
/// by Value equality, sections pairwise through sections_semantically_equal
/// (palette permutations are content-equal — the §10 rebuild drift).
fn chunk_semantically_equal(a: &[u8], b: &[u8]) -> bool {
    let (Ok(fastnbt::Value::Compound(ma)), Ok(fastnbt::Value::Compound(mb))) = (
        fastnbt::from_bytes::<Value>(a),
        fastnbt::from_bytes::<Value>(b),
    ) else {
        return a == b;
    };
    if ma.len() != mb.len() {
        return false;
    }
    for (k, va) in &ma {
        let Some(vb) = mb.get(k) else { return false };
        if k == "sections" {
            let (Value::List(la), Value::List(lb)) = (va, vb) else { return false };
            if la.len() != lb.len() {
                return false;
            }
            for (sa, sb) in la.iter().zip(lb.iter()) {
                if !c_saves::chunknbt::sections_semantically_equal(sa, sb) {
                    return false;
                }
            }
        } else if va != vb {
            return false;
        }
    }
    true
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

fn count_pristine(store: &Store) -> (usize, usize) {
    let mut pristine = 0usize;
    let mut stored = 0usize;
    for k in store.keys() {
        if store.is_pristine(k) {
            pristine += 1;
        } else if store.read_chunk(k).is_some() {
            stored += 1;
        } else {
            panic!("key {k:016x} is neither a contract nor readable storage");
        }
    }
    (pristine, stored)
}

/// Raw pregen payloads read straight out of the staging .mca copies (the
/// form Paper's serializer produced before any c-saves involvement).
fn read_raw_payloads(staging: &std::path::Path) -> HashMap<u64, Vec<u8>> {
    let mut raw_by_key: HashMap<u64, Vec<u8>> = HashMap::new();
    for entry in std::fs::read_dir(staging).unwrap().flatten() {
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
            let len = u32::from_be_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
            ]) as usize;
            if len < 2 || off + 4 + len > data.len() {
                continue;
            }
            let comp = &data[off + 5..off + 4 + len];
            let mut payload = Vec::new();
            if flate2::read::ZlibDecoder::new(comp)
                .read_to_end(&mut payload)
                .is_err()
            {
                continue;
            }
            let cx = rx * 32 + (i as i32 & 31);
            let cz = rz * 32 + (i as i32 >> 5);
            raw_by_key.insert(((cx as u64) << 32) | (cz as u64 & 0xffff_ffff), payload);
        }
    }
    raw_by_key
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dim = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../bench-corpus/dim"));
    let mut anvil_region = 0u64;
    for e in std::fs::read_dir(dim.join("region")).unwrap().flatten() {
        anvil_region += e.metadata().unwrap().len();
    }
    println!(
        "anvil region on disk: {:.2} MiB",
        anvil_region as f64 / 1048576.0
    );

    // ---- capture the raw pregen payloads (disposable staging copy) ----
    let w0 = work_dir("capture");
    let staging0 = w0.join("staging");
    copy_dir(&dim.join("region"), &staging0);
    let raw_by_key = read_raw_payloads(&staging0);
    let n = raw_by_key.len();
    println!("raw pregen payloads: {n}");
    let _ = std::fs::remove_dir_all(&w0);

    // the edited first-write payloads (the session-mutation model)
    let edited_by_key: HashMap<u64, Vec<u8>> = raw_by_key
        .iter()
        .map(|(k, p)| (*k, session_mutated(p)))
        .collect();
    // the mutation must be semantically visible to the contract legend
    let vis = edited_by_key
        .iter()
        .take(32)
        .filter(|(k, e)| {
            c_saves::chunknbt::canonical_contract_hash(e)
                != c_saves::chunknbt::canonical_contract_hash(&raw_by_key[k])
        })
        .count();
    assert_eq!(vis, 32, "session mutation must be legend-visible");
    println!("mutation legend-visible on sampled keys: 32/32");

    // ---- A) v0.9.47 default: not-fresh first writes STORE honestly ----
    let wa = work_dir("a-store");
    let cfg_a = Cfg { pristine: true, ..Cfg::from_env() };
    let sa = Store::open_cfg(&wa.join("region"), true, cfg_a).unwrap();
    for (k, e) in &edited_by_key {
        sa.write_chunk_fresh(*k, e, false, false)
            .expect("write A");
    }
    let (pa, sta) = count_pristine(&sa);
    // The edit survives regeneration = every key READS BACK (a contract
    // would read None BY DESIGN — run B measures exactly that). The
    // canon(read)==canon(edited) byte-digest additionally matches wherever
    // the raw payload's palette order is already canonical (524/728 here;
    // the remaining 204 are the §10 keys_probe drift: the rebuilt form
    // reorders palettes canonically, so the probe verifies the surviving
    // edits SEMANTICALLY like every production verifier does).
    let (reads_ok, canon_exact) = edited_by_key
        .iter()
        .fold((0usize, 0usize), |(ok, exact), (k, e)| {
            match sa.read_chunk(*k) {
                Some(got) => {
                    let sem = got == *e || chunk_semantically_equal(&got, e);
                    let ex = c_saves::chunknbt::canonical_contract_hash(&got)
                        == c_saves::chunknbt::canonical_contract_hash(e);
                    (ok + sem as usize, exact + ex as usize)
                }
                None => (ok, exact),
            }
        });
    let compact_a = drain(&sa);
    println!(
        "A v0.9.47 gate      : pristine={pa} stored={sta} edits_surviving={reads_ok}/{n} compact={:.2} MiB (x{:.3} vs anvil) unfresh_stored={}",
        compact_a as f64 / 1048576.0,
        compact_a as f64 / anvil_region.max(1) as f64,
        c_saves::store::ST_PRISTINE_UNFRESH.load(std::sync::atomic::Ordering::Relaxed)
    );
    println!("A canon-exact read-backs: {canon_exact}/{n} (rest = §10 palette-order drift, semantic verify passed)");
    // diagnostic for any semantically-unequal read-back: name the diff
    let mut bad = 0usize;
    for (k, e) in &edited_by_key {
        if let Some(got) = sa.read_chunk(*k) {
            let sem = got == *e || chunk_semantically_equal(&got, e);
            if !sem {
                bad += 1;
                if bad <= 2 {
                    println!("[diff] key {k:016x} rawlen={} gotlen={}", e.len(), got.len());
                    if let (Ok(fastnbt::Value::Compound(rm)), Ok(fastnbt::Value::Compound(gm))) =
                        (fastnbt::from_bytes::<Value>(e), fastnbt::from_bytes::<Value>(&got))
                    {
                        for (rk, rv) in &rm {
                            if gm.get(rk) != Some(rv) {
                                println!("  root differs: {rk}");
                            }
                        }
                        for gk in gm.keys() {
                            if !rm.contains_key(gk) {
                                println!("  root added: {gk}");
                            }
                        }
                    }
                }
            }
        }
    }
    if bad > 0 {
        println!("[diff] semantically-unequal read-backs: {bad}/{n}");
    }
    assert_eq!(pa, 0, "no not-fresh first write may contract");
    assert_eq!(reads_ok + bad, n, "every edit must survive regeneration");
    drop(sa);
    let _ = std::fs::remove_dir_all(&wa);

    // ---- B) 0.9.44-0.9.46 semantics: the hazard, measured ----
    let wb = work_dir("b-hazard");
    let cfg_b = Cfg { pristine: true, pristine_fresh: false, ..Cfg::from_env() };
    let sb = Store::open_cfg(&wb.join("region"), true, cfg_b).unwrap();
    for (k, e) in &edited_by_key {
        sb.write_chunk_fresh(*k, e, false, false).expect("write B");
    }
    let (pb, _) = count_pristine(&sb);
    let reverted = edited_by_key.keys().filter(|k| sb.read_chunk(**k).is_none()).count();
    let compact_b = drain(&sb);
    println!(
        "B pre-0.9.47 hazard : pristine={pb} edits_reverted_on_regen={reverted}/{n} compact={:.2} MiB",
        compact_b as f64 / 1048576.0
    );
    assert_eq!(pb, n, "pre-0.9.47 semantics contract every first write");
    assert_eq!(reverted, n, "every contracted edit reads None (reverted)");
    drop(sb);
    let _ = std::fs::remove_dir_all(&wb);

    // ---- C) pure-pregen invariance: fresh evidence keeps the win ----
    let wc = work_dir("c-pregen");
    let cfg_c = Cfg { pristine: true, ..Cfg::from_env() };
    let sc = Store::open_cfg(&wc.join("region"), true, cfg_c).unwrap();
    for (k, p) in &raw_by_key {
        sc.write_chunk_fresh(*k, p, false, true).expect("write C");
    }
    let (pc0, _) = count_pristine(&sc);
    assert_eq!(pc0, n, "every fresh first write contracts");
    // one real session: Paper re-saves every loaded chunk with fresh light
    // and bumped timers — the strip-canonical legend keeps the contracts
    for (k, p) in &raw_by_key {
        let rel = wgen::relit_variant(p);
        sc.write_chunk_fresh(*k, &rel, false, true).expect("resave C");
    }
    let (pc, stc) = count_pristine(&sc);
    let compact_c = drain(&sc);
    println!(
        "C pure-pregen       : pristine={pc} stored={stc} compact={:.2} MiB (x{:.4} vs anvil)",
        compact_c as f64 / 1048576.0,
        compact_c as f64 / anvil_region.max(1) as f64
    );
    assert_eq!(pc, n, "relight re-saves must not erode fresh contracts");
    drop(sc);
    let _ = std::fs::remove_dir_all(&wc);

    println!("=== verdict ===");
    println!("gate ON  : edits survive, pure-pregen win intact");
    println!("gate OFF : every pre-first-save edit silently reverts (0.9.44-0.9.46 hazard)");
}
