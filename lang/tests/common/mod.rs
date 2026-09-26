//! What the GQL test files share: the small store every test opens.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};

/// A 1 MiB buffered store with full sync.
pub fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}
