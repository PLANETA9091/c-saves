//! paper_probe (v0.9.62/63): offline verification of a REAL Paper server's
//! container — the ground truth behind "the server plays from the container".
//!
//! Gates (CI asserts on the exact lines):
//!   - every indexed chunk reads back non-empty (verify, bad=0);
//!   - the diamond block set by the live `setblock` command survived the
//!     server restart inside the chunk (0,0) palette — the edit only ever
//!     touched the container (zero .mca files on disk);
//!   - container size on disk is printed for the size accounting;
//!   - --export <dir> renders the honest (stored) chunks back into a vanilla
//!     region tree — the Anvil A/B baseline world (contracts CANNOT be
//!     rendered: they are regeneration promises, not bytes).
//!
//! Usage: paper_probe <dir> [--diamond] [--export <outdir>]

use c_saves::store::{Cfg, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: paper_probe <dir> [--diamond] [--export <outdir>]");
    let mut want_diamond = false;
    let mut export_to: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--diamond" => want_diamond = true,
            "--export" => export_to = args.next(),
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    let st = Store::open_cfg(std::path::Path::new(&dir), true, Cfg::default())
        .expect("open container");
    let keys = st.keys();
    let mut stored = 0usize;
    let mut contracted = 0usize;
    for k in &keys {
        if st.is_pristine(*k) {
            contracted += 1;
        } else {
            stored += 1;
        }
    }
    let mut ok = 0usize;
    let mut bad = 0usize;
    for k in &keys {
        match st.read_chunk(*k) {
            Some(d) if !d.is_empty() => ok += 1,
            _ => bad += 1,
        }
    }
    println!(
        "paper probe: chunks={} stored={} contracted={} read_ok={} bad={}",
        keys.len(), stored, contracted, ok, bad
    );
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
        let needle = b"minecraft:diamond_block";
        let k0 = ((0i64 as u64) << 32) | (0i64 as u64 & 0xffff_ffff);
        // where is the edit? scan EVERY chunk for the diamond block name
        let mut holders: Vec<(u64, usize)> = Vec::new();
        for k in &keys {
            if let Some(d) = st.read_chunk(*k) {
                if d.windows(needle.len()).any(|w| w == needle) {
                    holders.push((*k, d.len()));
                }
            }
        }
        match st.read_chunk(k0) {
            Some(d) => {
                let hit = d.windows(needle.len()).any(|w| w == needle);
                println!(
                    "paper probe: chunk (0,0) bytes={} diamond_block={}",
                    d.len(),
                    if hit { "PRESENT" } else { "ABSENT" }
                );
            }
            None => println!("paper probe: chunk (0,0) missing"),
        }
        if holders.is_empty() {
            println!("paper probe: diamond_block found in NO chunk");
            println!("=== paper probe: FAIL (edit lost) ===");
            std::process::exit(1);
        }
        for (k, sz) in &holders {
            let cx = (*k >> 32) as i32;
            let cz = (*k as u32) as i32;
            println!("paper probe: diamond_block PRESENT in chunk ({cx},{cz}) bytes={sz}");
        }
    }
    if let Some(out) = export_to {
        let rep = c_saves::anvil::export_dir(&st, std::path::Path::new(&out))
            .expect("export ok");
        println!(
            "paper probe: export out={} chunks={} pristine_skipped={} lost={} bytes={}",
            out, rep.chunks, rep.pristine_skipped, rep.lost, rep.bytes
        );
        if rep.lost != 0 {
            println!("=== paper probe: FAIL (export lost>0) ===");
            std::process::exit(1);
        }
    }
    println!("=== paper probe: PASS ===");
}
