//! zab: A/B zstd levels on real dumped slab streams.
fn main() {
    let mut t19 = 0; let mut t22 = 0; let mut raw = 0;
    for f in std::env::args().skip(1) {
        let data = std::fs::read(&f).unwrap();
        if data.len() < 100_000 { continue; } // only big slabs matter
        let c19 = zstd::bulk::compress(&data, 19).unwrap();
        let c22 = zstd::bulk::compress(&data, 22).unwrap();
        println!("{f}: raw {} z19 {} z22 {} ({:+.1}%)", data.len(), c19.len(), c22.len(), 100.0*(c22.len() as f64 - c19.len() as f64)/c19.len() as f64);
        raw += data.len(); t19 += c19.len(); t22 += c22.len();
    }
    if t19 > 0 { println!("TOTAL raw {raw} z19 {t19} z22 {t22} ({:+.1}%)", 100.0*(t22 as f64 - t19 as f64)/t19 as f64); }
}
