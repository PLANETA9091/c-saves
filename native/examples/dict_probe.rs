//! dict_probe: is the zstd dictionary trainer itself healthy on this host?
//!
//! Trains from three synthetic corpora with KNOWN structure:
//!   A. 400 pseudo-random structured blocks (repeating motifs + noise)
//!   B. the same 400 in a different order (determinism check)
//!   C. 200 near-identical uniform blocks (the degenerate-input case)
//! Prints resulting dictionary sizes. A healthy trainer on corpus A
//! returns ~dict_max; corpus C is ALLOWED to return small.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn motif_block(seed: u64, len: usize) -> Vec<u8> {
    // structured: 8 repeating motifs of 64 bytes + per-block noise tail
    let mut v = Vec::with_capacity(len);
    let mut h = DefaultHasher::new();
    h.write_u64(seed);
    let base = h.finish();
    let mut x = base | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    while v.len() < len {
        let m = (next() % 8) as u8;
        let mut motif = [0u8; 64];
        for (i, b) in motif.iter_mut().enumerate() {
            *b = m.wrapping_mul(31).wrapping_add(i as u8);
        }
        v.extend_from_slice(&motif);
        if v.len() % 512 == 0 {
            v.push((next() & 0xFF) as u8); // noise sprinkle
        }
    }
    v.truncate(len);
    v
}

fn uniform_block(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

fn main() {
    let dict_max = 112 * 1024;

    // A: 400 diverse structured blocks
    let a: Vec<Vec<u8>> = (0..400)
        .map(|i| motif_block(i as u64 * 7919, 2048 + (i as usize * 37) % 14336))
        .collect();
    let da = zstd::dict::from_samples(
        &a.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
        dict_max,
    );
    println!("A diverse-structured: {:?}", da.as_ref().map(|d| d.len()));

    // B: same blocks reversed (order independence)
    let mut b = a.clone();
    b.reverse();
    let db = zstd::dict::from_samples(
        &b.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
        dict_max,
    );
    println!("B reversed-order:     {:?}", db.as_ref().map(|d| d.len()));
    if let (Ok(x), Ok(y)) = (da, db) {
        println!("B bytes equal to A:   {}", x == y);
    }

    // C: near-uniform blocks (allowed to be degenerate)
    let c: Vec<Vec<u8>> = (0..200).map(|_| uniform_block(2048, 0)).collect();
    let dc = zstd::dict::from_samples(
        &c.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
        dict_max,
    );
    println!("C uniform (expect small): {:?}", dc.as_ref().map(|d| d.len()));
}
