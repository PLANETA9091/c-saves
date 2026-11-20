//! end_tail_probe: the end dimension's tail_nbt mass — unique-member count
//! and a trained-zstd-dictionary A/B on the tail streams (the lever: end
//! cities repeat structure NBT that is similar-but-not-identical).
//!
//! Usage: end_tail_probe <store-dir>
use c_saves::store::Store;
use std::path::Path;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: end_tail_probe <store-dir>"));
    let st = Store::open(&dir, true).unwrap();
    let members = st.slab_members_by_class();
    let mut uniq: std::collections::HashSet<u128> = std::collections::HashSet::new();
    let (mut n, mut raw) = (0usize, 0usize);
    for (cls, body) in &members {
        if cls != "tail_nbt" {
            continue;
        }
        n += 1;
        raw += body.len();
        uniq.insert(xxhash_rust::xxh3::xxh3_128(body));
    }
    println!("tail_nbt members={n} raw={raw} unique={} (store {})", uniq.len(), dir.display());
    // dictionary A/B: train a zstd dictionary on 80% of the tail members,
    // compress the 20% holdout at z19 with and without the dictionary.
    // The baseline is the members' share of the drained slab frame (they
    // ride the mixed LZMA2 window today) — the probe prices the
    // dedicated-stream alternative.
    let mut tails: Vec<Vec<u8>> = Vec::new();
    for (cls, body) in &members {
        if cls == "tail_nbt" {
            tails.push(body.clone());
        }
    }
    if tails.len() >= 16 {
        tails.sort_by_key(|b| std::cmp::Reverse(b.len()));
        let cut = tails.len() * 4 / 5;
        let (train, hold): (&[Vec<u8>], &[Vec<u8>]) = (&tails[..cut], &tails[cut..]);
        let train_raw: usize = train.iter().map(|b| b.len()).sum();
        let hold_raw: usize = hold.iter().map(|b| b.len()).sum();
        // train via from_samples
        let samples: Vec<&[u8]> = train.iter().map(|b| b.as_slice()).collect();
        let dict = match zstd::dict::from_samples(&samples, 64 * 1024) {
            Ok(d) => d,
            Err(e) => {
                println!("dict train failed: {e}");
                return;
            }
        };
        let mut plain = 0usize;
        let mut withd = 0usize;
        for b in hold {
            // plain z19
            let mut c0 = Vec::new();
            let mut e0 = zstd::stream::Encoder::new(&mut c0, 19).unwrap();
            std::io::Write::write_all(&mut e0, b).unwrap();
            let _ = e0.finish().unwrap();
            plain += c0.len();
            // z19 + trained dictionary
            let mut c1 = Vec::new();
            let mut e1 =
                zstd::stream::Encoder::with_dictionary(&mut c1, 19, &dict).unwrap();
            std::io::Write::write_all(&mut e1, b).unwrap();
            let _ = e1.finish().unwrap();
            withd += c1.len();
        }
        println!(
            "holdout: members={} raw={}B | z19 plain={} ({:.1}x) | z19+dict64k={} ({:.1}x, {:.1}% vs plain) | train_raw={train_raw}B",
            hold.len(),
            hold_raw,
            plain,
            hold_raw as f64 / plain as f64,
            withd,
            hold_raw as f64 / withd as f64,
            100.0 * (withd as f64 - plain as f64) / plain as f64
        );
    }
    drop(st);
}
