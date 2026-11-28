//! autoregen_probe (v0.9.133): the end-to-end auto-regen loop priced —
//! absent keys regenerate from the wgen generator (the same payload class
//! the pristine tier's CI gates use), store through the live pipeline and
//! serve; re-reads are plain store hits.
//!
//! This is the missing half of `set_autoregen_source`'s story: the store
//! test pins the CONTRACT, this probe pins the COST (regen wall per chunk,
//! container growth per regenerated chunk) and the self-heal at scale.
//!
//! Environment:
//!   AUTOREGEN_DIR=<dir>    work dir (required)
//!   AUTOREGEN_N=<n>        keys to touch (default 1024 = a 32x32-chunk view)
//!   AUTOREGEN_SEED=<u64>   wgen world seed (default 0xC0FFEE)
//!
//! Exit code 0 = every touched key served after regeneration AND every
//! re-read hit the store (self-healed); nonzero otherwise.

use c_saves::store::{Cfg, Store, ST_AUTOREGEN};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[path = "wgen/mod.rs"]
mod wgen;

fn env_parse<T: std::str::FromStr>(name: &str, dflt: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

fn key_of(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u32 as u64)
}

fn main() {
    let dir = std::env::var("AUTOREGEN_DIR").expect("AUTOREGEN_DIR required");
    let n: usize = env_parse("AUTOREGEN_N", 1024);
    let seed: u64 = env_parse("AUTOREGEN_SEED", 0xC0FFEE);
    let side = (n as f64).sqrt().ceil() as i32;

    let cfg = Cfg { autoregen: true, ..Cfg::default() };
    let s = Store::open_cfg(&PathBuf::from(&dir), true, cfg).unwrap();

    let world = wgen::World::new(seed);
    let src_world = std::sync::Mutex::new(world);
    s.set_autoregen_source(Some(Arc::new(move |key| {
        let cx = (key >> 32) as i32;
        let cz = key as u32 as i32;
        let w = src_world.lock().unwrap();
        // round=0: pure worldgen, the payload class a first autosave produces
        Some(wgen::chunk_payload(&w, cx, cz, 0))
    })));

    // 1. regeneration wall: N absent keys, each read = regen + store + serve
    let t = Instant::now();
    let mut served = 0usize;
    for i in 0..n {
        let cx = (i as i32 % side) - side / 2;
        let cz = (i as i32 / side) - side / 2;
        if s.read_chunk(key_of(cx, cz)).is_some() {
            served += 1;
        }
    }
    let wall = t.elapsed().as_secs_f64();
    let attempts = ST_AUTOREGEN.0.load(Ordering::Relaxed);
    let stored = ST_AUTOREGEN.1.load(Ordering::Relaxed);
    println!(
        "autoregen wall: {served}/{n} served in {wall:.2}s ({:.1} ms/chunk regen+store)",
        wall * 1000.0 / n.max(1) as f64
    );
    println!("autoregen counters: attempts={attempts} stored={stored}");

    // 2. self-heal: every re-read is a plain store hit (fast path)
    let t2 = Instant::now();
    let mut hits = 0usize;
    for i in 0..n {
        let cx = (i as i32 % side) - side / 2;
        let cz = (i as i32 / side) - side / 2;
        if s.read_chunk(key_of(cx, cz)).is_some() {
            hits += 1;
        }
    }
    let warm = t2.elapsed().as_secs_f64();
    println!(
        "self-heal: {hits}/{n} store hits in {warm:.3}s ({:.2} ms/chunk — {:.0}x faster than regen)",
        warm * 1000.0 / n.max(1) as f64,
        (wall / warm.max(1e-9))
    );

    // 3. durability: sync + reopen, everything still readable
    s.sync();
    drop(s);
    let cfg2 = Cfg { autoregen: false, ..Cfg::default() };
    let s2 = Store::open_cfg(&PathBuf::from(&dir), true, cfg2).unwrap();
    let mut reopen_ok = 0usize;
    for i in 0..n {
        let cx = (i as i32 % side) - side / 2;
        let cz = (i as i32 / side) - side / 2;
        if s2.read_chunk(key_of(cx, cz)).is_some() {
            reopen_ok += 1;
        }
    }
    println!("reopen (flag OFF): {reopen_ok}/{n} served from the stored container");
    let ok = served == n && hits == n && reopen_ok == n && attempts == n as u64 && stored == n as u64;
    println!("=== autoregen probe: {} ===", if ok { "PASS" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}

use std::sync::atomic::Ordering;
