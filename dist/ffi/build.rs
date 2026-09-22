//! Regenerate `include/sekejap.h` from the `extern "C"` surface with
//! cbindgen, so the header always matches the Rust functions.
//!
//! Best-effort, as the e1 build script was: if cbindgen fails for any reason
//! the committed header stays authoritative and the build does not break.
//! `docs/dist/C_ABI.md` is the contract the header carries.

use std::path::PathBuf;

fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let header = PathBuf::from(&crate_dir).join("include").join("sekejap.h");

    // Only re-run when the surface or the configuration changes.
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
            println!("cargo:warning=cbindgen header generation skipped: {e}");
        }
    }
}
