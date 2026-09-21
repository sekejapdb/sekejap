//! Cross-version typed admission probe. Copy this harness into a preserved
//! engine checkout without changing that engine, and record its source hash.
//! Test drivers must use disposable database copies and compare inventories.
use sekejap_core::collections::{Database, Error};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!(
            "{}",
            serde_json::json!({
                "harness":"typed-admission-probe-v1",
                "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "features":{
                    "compact-cells":cfg!(feature="compact-cells"),
                    "sqlite-balance":cfg!(feature="sqlite-balance"),
                    "keyspace-append":cfg!(feature="keyspace-append"),
                    "slotref-split":cfg!(feature="slotref-split")
                }
            })
        );
        return;
    }
    if args.len() != 3
        || !matches!(args[0].as_str(), "accept" | "refuse")
        || !matches!(args[1].as_str(), "snapshot" | "writer")
    {
        eprintln!("usage: typed_admission_probe accept|refuse snapshot|writer COPIED_DB");
        std::process::exit(2);
    }
    let config = Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    };
    let result = if args[1] == "snapshot" {
        Database::open_snapshot(&args[2], config)
    } else {
        Database::open(&args[2], config)
    };
    match (args[0].as_str(), result) {
        ("accept", Ok(database)) => {
            drop(database);
            println!(
                "{}",
                serde_json::json!({"result":"ADMITTED","mode":args[1]})
            );
        }
        ("refuse", Err(Error::Unsupported(reason))) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "result":"UNSUPPORTED","mode":args[1],"reason":reason
                })
            );
            std::process::exit(42);
        }
        (_, Ok(_)) => {
            eprintln!("unexpected successful typed admission");
            std::process::exit(1);
        }
        (_, Err(error)) => {
            // A generic corruption/I/O error is not proof that an old binary
            // recognized and safely refused a newer encoding.
            eprintln!("unexpected typed admission error: {error:?}");
            std::process::exit(1);
        }
    }
}
