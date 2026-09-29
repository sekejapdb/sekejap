//! Open a database with THIS build's normal open path and say what it
//! answered. Built against a release's own source (a worktree at its tag with
//! this file copied in, as the release fixtures are), it is how a test proves
//! what a RELEASED binary does with a newer file (docs/core/SUPPORTIVE.md,
//! section 6, step 2): exit 0 and `OPENED`, or exit 2 and `REFUSED <reason>`.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;

fn main() {
    let path = std::env::args().nth(1).expect("usage: open_check DATABASE_DIRECTORY");
    let cfg = Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full };
    match Database::open(&path, cfg) {
        Ok(_) => println!("OPENED"),
        Err(e) => {
            println!("REFUSED {e}");
            std::process::exit(2);
        }
    }
}
