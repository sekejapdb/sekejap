//! Opens a COPY: Store::open may replay WAL. Never point this at source evidence.
use kernel::{store::{Config, Store, SyncMode}, io::IoMode};
use serde_json::json;
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3);
    let n: u64 = args[2].parse()?;
    let store = Store::open(Path::new(&args[1]), Config {
        budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off,
    })?;
    let mut count = 0;
    let mut disorder = 0;
    let mut previous = None;
    let mut bad_values = 0;
    let mut scan_error = None;
    for row in store.scan(&[])? {
        match row {
            Ok((key, value)) => {
                let key = u64::from_be_bytes(key.try_into().unwrap());
                if previous.is_some_and(|p| p >= key) { disorder += 1; }
                previous = Some(key);
                if value != vec![b'x'; 200] { bad_values += 1; }
                count += 1;
            }
            Err(e) => { scan_error = Some(e.to_string()); break; }
        }
    }
    let mut wrong_points = 0;
    let mut examples = Vec::new();
    for i in 0..n {
        let key = i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes();
        let result = store.get(&key);
        if !matches!(result.as_ref(), Ok(Some(v)) if v == &vec![b'x'; 200]) {
            wrong_points += 1;
            if examples.len() < 16 { examples.push(json!({"insertion_index":i,"result":format!("{result:?}")})); }
        }
    }
    println!("{}", json!({"expected_rows":n,"scan_rows":count,"scan_disorder":disorder,
        "scan_bad_values":bad_values,"scan_error":scan_error,"wrong_points":wrong_points,"examples":examples}));
    Ok(())
}
