//! z19: compress given files with zstd-19 bulk (same call as the engine).
//! Usage: cargo run --release --example z19 -- file1 [file2...]

fn main() {
    for f in std::env::args().skip(1) {
        let data = std::fs::read(&f).unwrap();
        let c = zstd::bulk::compress(&data, 19).unwrap();
        println!(
            "{f}: raw {} -> z19 {} (x{:.2})",
            data.len(),
            c.len(),
            data.len() as f64 / c.len() as f64
        );
        println!("head: {}", data[..data.len().min(64)].iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
}
