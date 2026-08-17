//! A process whose only job is to be killed in the middle of a compaction.
//!
//! `tests/crash_recovery.rs` spawns this, waits a random interval, and sends
//! SIGKILL. The store it leaves behind must still open and still hold every row
//! that was committed before it started — a compaction is a rewrite of the whole
//! durable half, published by renaming files into place, and the question is
//! whether the moment of publication is atomic.
//!
//! In-process there is no way to ask: `compact()` is synchronous, and a panic
//! part-way through still unwinds and runs destructors. A real crash does not.
//!
//!     cargo run --release --example compact_victim -- <dir>

use sekejap::CoreDB;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: compact_victim <dir>");
    let mut db = match CoreDB::open(&dir) {
        Ok(db) => db,
        Err(e) => { eprintln!("open failed: {e}"); std::process::exit(2) }
    };
    // Something to fold in, so the compaction has work to do rather than
    // returning immediately.
    for i in 0..2_000 {
        let _ = db.put(&format!("p/extra{i}"), &format!(
            r#"{{"_collection":"p","_key":"extra{i}","n":{i}}}"#));
    }
    println!("armed");
    loop {
        if db.compact().is_err() { std::process::exit(3) }
    }
}
