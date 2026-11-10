//! paper_probe (v0.9.62): offline verification of a REAL Paper server's
//! container — the ground truth behind "the server plays from the container".
//!
//! Gates (CI asserts on the exact lines):
//!   - every indexed chunk reads back non-empty (verify, bad=0);
//!   - the diamond block set by the live `setblock` command survived the
//!     server restart inside the chunk (0,0) palette — the edit only ever
//!     touched the container (zero .mca files on disk);
//!   - container size on disk is printed for the size accounting.
//!
//! Usage: paper_probe <container-dir> [--diamond]

use c_saves::store::{Cfg, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: paper_probe <dir> [--diamond]");
    let want_diamond = args.next().as_deref() == Some("--diamond");

    let st = Store::open_cfg(std::path::Path::new(&dir), true, Cfg::default())
        .expect("open container");
    let keys = st.keys();
    let mut ok = 0usize;
    let mut bad = 0usize;
    for k in &keys {
        match st.read_chunk(*k) {
            Some(d) if !d.is_empty() => ok += 1,
            _ => bad += 1,
        }
    }
    println!("paper probe: chunks={} read_ok={} bad={}", keys.len(), ok, bad);
    if bad != 0 {
        println!("=== paper probe: FAIL (bad>0) ===");
        std::process::exit(1);
    }

    // total on-disk container size (all files under dir)
    let mut total = 0u64;
    fn walk(p: &std::path::Path, total: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let t = e.path();
                if t.is_dir() {
                    walk(&t, total);
                } else if let Ok(md) = e.metadata() {
                    *total += md.len();
                }
            }
        }
    }
    walk(std::path::Path::new(&dir), &mut total);
    println!("paper probe: container bytes on disk: {}", total);

    if want_diamond {
        let k0 = ((0i64 as u64) << 32) | (0i64 as u64 & 0xffff_ffff);
        match st.read_chunk(k0) {
            Some(d) => {
                let needle = b"minecraft:diamond_block";
                let hit = d.windows(needle.len()).any(|w| w == needle);
                println!(
                    "paper probe: chunk (0,0) bytes={} diamond_block={}",
                    d.len(),
                    if hit { "PRESENT" } else { "ABSENT" }
                );
                if !hit {
                    println!("=== paper probe: FAIL (edit lost) ===");
                    std::process::exit(1);
                }
            }
            None => {
                println!("paper probe: chunk (0,0) missing");
                println!("=== paper probe: FAIL (chunk 0,0 absent) ===");
                std::process::exit(1);
            }
        }
    }
    println!("=== paper probe: PASS ===");
}
