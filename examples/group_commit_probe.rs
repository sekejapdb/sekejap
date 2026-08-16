//! Does sharing the fsync actually buy anything, and does a lone writer pay for it?
//!
//! Writers all commit durably (`SyncMode::Full`); the only question is how many
//! fsyncs that takes. With one writer the answer must be "the same as before" —
//! group commit must not make the single-user case worse.
use sekejap::engine::Engine;
use std::sync::Arc;
use std::time::Instant;

fn run(threads: usize, per_thread: usize) -> f64 {
    let dir = tempfile::TempDir::new().unwrap();
    let eng = Arc::new(Engine::open_as_service(dir.path().to_str().unwrap()).unwrap());
    eng.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();

    let start = Instant::now();
    let mut handles = Vec::new();
    for t in 0..threads {
        let eng = Arc::clone(&eng);
        handles.push(std::thread::spawn(move || {
            for i in 0..per_thread {
                eng.execute(&format!(
                    "INSERT INTO d (_key, body) VALUES ('t{t}r{i}', 'the quick brown fox {i}')"
                )).unwrap();
            }
        }));
    }
    for h in handles { h.join().unwrap(); }
    let elapsed = start.elapsed().as_secs_f64();
    (threads * per_thread) as f64 / elapsed
}

fn main() {
    println!("{:<10}{:>16}{:>14}", "writers", "writes/sec", "vs 1 writer");
    let base = run(1, 150);
    println!("{:<10}{:>16.0}{:>14}", 1, base, "-");
    for t in [2usize, 4, 8, 16] {
        let r = run(t, 150);
        println!("{:<10}{:>16.0}{:>13.1}x", t, r, r / base);
    }
}
