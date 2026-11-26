//! scan_timing: the reopen wall decomposition — Store::open_cfg per store
//! dir (the scan = record walk + batch decompress + slab parse + the GLOB
//! identity recompute, v0.9.125 parallelized across the cores), plus the
//! scan re-encode split counters. Usage: scan_timing [store-dir ...]
use c_saves::store::{Cfg, Store};
use std::sync::atomic::Ordering;
use std::time::Instant;

fn main() {
    let dirs: Vec<String> = std::env::args().skip(1).collect();
    let dirs = if dirs.is_empty() {
        vec![
            "/tmp/full_world/store-overworld".into(),
            "/tmp/full_world/store-nether".into(),
            "/tmp/full_world/store-end".into(),
        ]
    } else {
        dirs
    };
    for dir in &dirs {
        c_saves::store::reset_scan_diag();
        let t0 = Instant::now();
        let st = Store::open_cfg(std::path::Path::new(dir), true, Cfg::from_env()).unwrap();
        let open = t0.elapsed();
        let t1 = Instant::now();
        let prof = st.profile_json();
        let prof_t = t1.elapsed();
        let name = std::path::Path::new(dir)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        println!("{name}: open {:?} | profile_json {:?}", open, prof_t);
        println!("  {}", prof);
        println!(
            "  scan-split: conv {} ns / {} members ({:.1} us each) | reencode {} ns | hash {} ns",
            c_saves::store::SCAN_CONV_NS.load(Ordering::Relaxed),
            c_saves::store::SCAN_CONV_N.load(Ordering::Relaxed),
            c_saves::store::SCAN_CONV_NS.load(Ordering::Relaxed) as f64
                / c_saves::store::SCAN_CONV_N.load(Ordering::Relaxed).max(1) as f64
                / 1000.0,
            c_saves::store::SCAN_RE_NS.load(Ordering::Relaxed),
            c_saves::store::SCAN_HASH_NS.load(Ordering::Relaxed)
        );
        println!(
            "  reencode-split: decode {} ns | view {} ns | encode {} ns",
            c_saves::chunknbt::SCAN_RE_DEC.load(Ordering::Relaxed),
            c_saves::chunknbt::SCAN_RE_VIEW.load(Ordering::Relaxed),
            c_saves::chunknbt::SCAN_RE_ENC.load(Ordering::Relaxed)
        );
        println!(
            "  re-wire: hits {} | fused {} | eq_ok {} | eq_bad {}",
            c_saves::chunknbt::RE_WIRE_HITS.load(Ordering::Relaxed),
            c_saves::chunknbt::GLOB_FUSED_HITS.load(Ordering::Relaxed),
            c_saves::chunknbt::RE_EQ_OK.load(Ordering::Relaxed),
            c_saves::chunknbt::RE_EQ_BAD.load(Ordering::Relaxed)
        );
        drop(st);
    }
}
