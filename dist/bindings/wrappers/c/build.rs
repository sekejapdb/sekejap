//! Build script: regenerate `include/sekejap.h` from the `extern "C"` surface
//! with cbindgen, so the header always matches the Rust functions. This is the
//! automation analog of maturin for the Python wrapper — the artifact (here, the
//! C header) is produced from the source on every build, never hand-maintained.
//!
//! Best-effort: if cbindgen fails for any reason, we warn and keep the committed
//! header, so a toolchain hiccup never breaks `cargo build`.

use std::path::PathBuf;

fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let header = PathBuf::from(&crate_dir).join("include").join("sekejap.h");

    // Only re-run when the surface or config changes.
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let config = cbindgen::Config::from_root_or_default(&crate_dir);
    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => {
            bindings.write_to_file(&header);
        }
        Err(e) => {
            // Don't fail the build; the committed header stays authoritative.
            println!("cargo:warning=cbindgen header generation skipped: {e}");
        }
    }
}
