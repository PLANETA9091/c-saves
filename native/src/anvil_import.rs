//! Background legacy import: drain the vanilla .mca region files into the
//! container, then (optionally) delete them once verified.

use crate::store::Store;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Import only when: legacy dir exists, and the container is fresh or a
/// previous import was interrupted (resumable: present chunks are skipped).
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
    // fresh container OR resumable partial import (legacy still present)
    if existing_chunks > 0 {
        let has_mca = std::fs::read_dir(&legacy)
            .map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".mca")))
            .unwrap_or(false);
        if !has_mca {
            return;
        }
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
                }
                Err(e) => eprintln!("[c-saves] import {kind} failed: {e}"),
            }
        })
        .ok();
}

