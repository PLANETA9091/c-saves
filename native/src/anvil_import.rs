//! Background legacy import: drain the vanilla .mca region files into the
//! container, then (optionally) delete them once verified.

use crate::store::Store;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// v0.9.53: POI-store elision is opt-in per process (CSAVES_POI_ELIDE=1),
/// the same switch engine::attach reads for the live whole-mode path.
pub fn poi_elide_enabled() -> bool {
    std::env::var("CSAVES_POI_ELIDE")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// v0.9.53: the legacy `poi/` dir is a DERIVED index, not primary world
/// data — vanilla rebuilds it from chunk block states on load
/// (PoiManager.checkConsistencyWithBlocks / Yarn initForPalette). Elide
/// mode names the dir, counts its weight and removes the files
/// (CSAVES_KEEP_MCA=1 keeps them, mirroring the region import). Returns
/// (files, bytes) — the operator-visible weight that stopped existing.
pub fn elide_legacy_poi(poi_dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut names: Vec<(PathBuf, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(poi_dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    files += 1;
                    bytes += m.len();
                    names.push((e.path(), e.file_name().to_string_lossy().to_string()));
                }
            }
        }
    }
    if files == 0 {
        return (0, 0);
    }
    let keep = std::env::var("CSAVES_KEEP_MCA").map(|v| v == "1").unwrap_or(false);
    if !keep {
        for (p, n) in &names {
            let _ = std::fs::remove_file(p);
            eprintln!("[c-saves] poi elided: {n}");
        }
    }
    eprintln!(
        "[c-saves] poi store elided: {files} file(s), {:.1} MiB {}(derived index — vanilla rebuilds from chunk block states on load: PoiManager.checkConsistencyWithBlocks)",
        bytes as f64 / 1048576.0,
        if keep { "KEPT (CSAVES_KEEP_MCA=1) " } else { "" }
    );
    (files, bytes)
}

/// v0.9.96: the legacy .mca drain is OPT-IN. Default operation is
/// native-only: the container is the world from the first byte, and no
/// background Anvil conversion runs at attach. The drain (which used to
/// start automatically on every attach with a legacy <world>/<kind> dir —
/// read every .mca, write the chunks into the container, delete the files
/// once clean) restores with CSAVES_IMPORT=1.
pub fn import_enabled() -> bool {
    std::env::var("CSAVES_IMPORT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Does the legacy dir still hold region files? Shared by the native-only
/// gate's operator log and the resumable-import check.
pub fn legacy_has_mca(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".mca")))
        .unwrap_or(false)
}

/// Import only when: the operator opted in (CSAVES_IMPORT=1), the legacy dir
/// exists, and the container is fresh or a previous import was interrupted
/// (resumable: present chunks are skipped).
pub fn maybe_spawn_import(
    store: Arc<Store>,
    container_dir: &str,
    kind: &str,
    existing_chunks: usize,
) {
    let container = PathBuf::from(container_dir);
    // <world>/c-saves/<kind>  ->  <world>/<kind>
    let Some(world) = container.parent().and_then(|p| p.parent()) else { return };
    let legacy = world.join(kind);
    if !legacy.is_dir() {
        return;
    }
    // v0.9.53 POI elision: the legacy poi/ dir never drains into the
    // container (a poi .mca is not chunk NBT — the chunk-NBT import would
    // just log per-file failures). Elide mode names + removes the derived
    // index instead; nothing else to do for this kind.
    if kind == "poi" && poi_elide_enabled() {
        let dir = legacy.clone();
        std::thread::Builder::new()
            .name("csaves-poi-elide".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let (files, bytes) = elide_legacy_poi(&dir);
                if files > 0 {
                    eprintln!("[c-saves] poi elide done: {files} file(s), {:.1} MiB in {:.1}s", bytes as f64 / 1048576.0, t0.elapsed().as_secs_f32());
                }
            })
            .ok();
        return;
    }
    // v0.9.96 NATIVE-ONLY GATE: without CSAVES_IMPORT=1 no drain thread is
    // ever spawned. The legacy region dir stays INERT: read_chunk answers
    // from the container only (no Anvil fallback exists on the live path),
    // a chunk the container does not know regenerates (vanilla worldgen),
    // and the .mca files are left untouched on disk. One log line names the
    // state so the operator is never surprised by a regenerating world.
    if !import_enabled() {
        if legacy_has_mca(&legacy) {
            eprintln!(
                "[c-saves] native-only: legacy {kind}/*.mca present, background drain is OFF (CSAVES_IMPORT=1 opts in) — the container is the world; chunks it does not know regenerate"
            );
        }
        return;
    }
    // fresh container OR resumable partial import (legacy still present)
    if existing_chunks > 0 && !legacy_has_mca(&legacy) {
        return;
    }
    let kind_owned = kind.to_string();
    std::thread::Builder::new()
        .name(format!("csaves-import-{kind_owned}"))
        .spawn(move || {
            let kind = kind_owned.as_str();
            let t0 = std::time::Instant::now();
            match crate::anvil::import_dir(&legacy, &store) {
                Ok(rep) => {
                    let logical = store.logical();
                    let disk = store.disk_bytes();
                    eprintln!(
                        "[c-saves] import {kind}: files={} chunks={} skipped={} failed={} in {:.1}s (logical {:.1} MB -> disk {:.1} MB)",
                        rep.files, rep.chunks, rep.skipped_present, rep.failed,
                        t0.elapsed().as_secs_f32(),
                        logical as f64 / 1048576.0,
                        disk as f64 / 1048576.0
                    );
                    // per-file removal happens inside import_dir (only clean files)
                    // imported data lands at the hot tier; request a pack pass so
                    // it gets slab-packed + cold-tiered right away
                    store.request_pack();
                }
                Err(e) => eprintln!("[c-saves] import {kind} failed: {e}"),
            }
        })
        .ok();
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_only_gate_predicates() {
        // legacy_has_mca: an unrelated file does not count, a .mca does.
        let d = std::env::temp_dir().join(format!(
            "csaves-test-import-gate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let region = d.join("region");
        std::fs::create_dir_all(&region).unwrap();
        std::fs::write(region.join("level.dat"), b"x").unwrap();
        assert!(!legacy_has_mca(&region), "non-.mca files are not region files");
        std::fs::write(region.join("r.0.0.mca"), vec![0u8; 8192]).unwrap();
        assert!(legacy_has_mca(&region), "a .mca file is detected");

        // the gate predicate: default OFF (native-only), opt-in forms ON.
        std::env::remove_var("CSAVES_IMPORT");
        assert!(!import_enabled(), "native-only is the default");
        std::env::set_var("CSAVES_IMPORT", "1");
        assert!(import_enabled(), "CSAVES_IMPORT=1 restores the drain");
        std::env::set_var("CSAVES_IMPORT", "true");
        assert!(import_enabled());
        std::env::set_var("CSAVES_IMPORT", "0");
        assert!(!import_enabled());
        std::env::remove_var("CSAVES_IMPORT");
        assert!(!import_enabled(), "removal restores the default");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn poi_elide_legacy_dir_named_and_removed() {
        // v0.9.53: the legacy poi/ dir is a derived index — elide mode
        // counts its weight and removes the files (CSAVES_KEEP_MCA=1 keeps
        // them, mirroring the region import's escape hatch).
        let d = std::env::temp_dir().join(format!(
            "csaves-test-poi-legacy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let poi = d.join("poi");
        std::fs::create_dir_all(&poi).unwrap();
        std::fs::write(poi.join("r.0.0.mca"), vec![0u8; 8192]).unwrap();
        std::fs::write(poi.join("r.0.1.mca"), vec![0u8; 4096]).unwrap();
        let (files, bytes) = elide_legacy_poi(&poi);
        assert_eq!(files, 2);
        assert_eq!(bytes, 8192 + 4096);
        assert!(std::fs::read_dir(&poi).unwrap().next().is_none(), "files removed");

        // the escape hatch keeps the files on disk
        std::fs::write(poi.join("r.1.0.mca"), vec![0u8; 100]).unwrap();
        std::env::set_var("CSAVES_KEEP_MCA", "1");
        let (files2, _) = elide_legacy_poi(&poi);
        std::env::remove_var("CSAVES_KEEP_MCA");
        assert_eq!(files2, 1);
        assert!(poi.join("r.1.0.mca").exists(), "KEEP_MCA keeps the file");
        let _ = std::fs::remove_dir_all(&d);
    }
}
