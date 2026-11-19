//! drift_diag: correlate the glob drift class with DEAD PALETTE ENTRIES.
//! For a sample of corpus chunks (deterministic builder, split exactly
//! like the import path): every section runs the pack gate standalone
//! (section_to_glob -> reconstruct -> canonical re-encode -> byte compare)
//! and gets a dead-entry count from its GlobPre (palette states no voxel
//! references). Aggregates the 2x2: stable/divergent x dead/used-only.
#[path = "inc/tectonic_corpus.rs"]
mod tc;

use c_saves::chunknbt::{self, Split};
use std::collections::HashSet;

fn main() {
    let f: i32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(40);

    let (mut stable_dead0, mut stable_dead, mut drift_dead0, mut drift_dead) = (0u64, 0u64, 0u64, 0u64);
    let (mut conv_total, mut conv_unstable) = (0u64, 0u64);
    let mut dead_examples: Vec<(usize, usize, usize)> = Vec::new(); // (dead, n, used)
    let mut b = tc::Builder::new(f);
    let mut reg = chunknbt::StateRegistry::default();
    let mut sections = 0u64;
    for cz in 0..f {
        for cx in 0..f {
            let (payload, _) = b.payload(cx, cz);
            let Ok(split) = chunknbt::split(&payload, false, Some((cx, cz)), true) else {
                continue;
            };
            let Split::Split { sections: secs, .. } = split else { continue };
            for (h, sec) in secs {
                sections += 1;
                let bytes = sec.as_slice();
                let Some(pre) = chunknbt::section_glob_pre(bytes) else { continue };
                // dead entries: palette states no voxel references
                let used: HashSet<u32> = pre.vox.iter().copied().collect();
                let dead = pre.states.len().saturating_sub(used.len());
                // the pack gate, standalone
                let Some(glob) = chunknbt::section_to_glob(bytes, &mut reg) else {
                    continue; // refused conversion — not part of the class
                };
                conv_total += 1;
                let derivable = match chunknbt::glob_any_to_section(&glob, &reg) {
                    Some(d) => chunknbt::canonical_reencode(&d)
                        .map(|c| c == bytes)
                        .unwrap_or(false),
                    None => false,
                };
                if !derivable {
                    conv_unstable += 1;
                    if dead > 0 {
                        drift_dead += 1;
                        if dead_examples.len() < 8 {
                            dead_examples.push((dead, pre.states.len(), used.len()));
                        }
                    } else {
                        drift_dead0 += 1;
                    }
                } else if dead > 0 {
                    stable_dead += 1;
                } else {
                    stable_dead0 += 1;
                }
                let _ = h;
            }
        }
    }
    println!("sections={sections} converted={conv_total} unstable={conv_unstable}");
    println!("2x2 stable/divergent x dead>0:");
    println!("  stable dead=0: {stable_dead0} | stable dead>0: {stable_dead}");
    println!("  DRIFT  dead=0: {drift_dead0} | DRIFT  dead>0: {drift_dead}");
    println!("dead examples (dead, palette_n, used): {dead_examples:?}");
}
