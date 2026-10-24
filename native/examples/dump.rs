use c_saves::store::Store;
use fastnbt::Value;
use std::path::Path;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let st = Arc::new(Store::open(Path::new(&args[2]), true).expect("open"));
    let mut keys = st.keys();
    keys.sort();
    println!("chunks: {}", keys.len());
    let n: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(6);
    let mut statuses = std::collections::HashMap::new();
    for k in keys.iter().take(n) {
        let cx = (*k >> 32) as i32;
        let cz = (*k as u32) as i32;
        if let Some(data) = st.read_chunk(*k) {
            if let Ok(Value::Compound(m)) = fastnbt::from_bytes::<Value>(&data) {
                let status = match m.get("Status") {
                    Some(Value::String(s)) => s.clone(),
                    Some(_) => "<non-string>".into(),
                    None => "<missing>".into(),
                };
                let secs = match m.get("sections") {
                    Some(Value::List(l)) => l.len(),
                    _ => 9999,
                };
                *statuses.entry(status.clone()).or_insert(0) += 1;
                println!("({cx:>4},{cz:>4}) status={status} sections={secs} {}B", data.len());
            }
        }
    }
    println!("status histogram (first {n}): {statuses:?}");
}
