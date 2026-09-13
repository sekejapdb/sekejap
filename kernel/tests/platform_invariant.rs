//! D21's enforcement: platform code lives in io.rs and NOWHERE else.
//!
//! The compile gates (cargo check --target windows-msvc / android / arm-linux)
//! prove today's tree is portable; this test keeps it true tomorrow. A
//! `std::os::unix` imported into vector code in phase 2e would pass every
//! functional test on this machine and break three platforms silently -- the
//! e1 rot pattern, applied to portability. This fails the build instead.

use std::path::Path;

#[test]
fn platform_specific_code_only_in_io_rs() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        scan(&path, &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "platform-specific code outside io.rs (D21):\n{}",
        offenders.join("\n")
    );
}

fn scan(path: &Path, out: &mut Vec<String>) {
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap() {
            scan(&e.unwrap().path(), out);
        }
        return;
    }
    if path.extension().is_none_or(|x| x != "rs") { return; }
    if path.file_name().is_some_and(|n| n == "io.rs") { return; }
    let text = std::fs::read_to_string(path).unwrap();
    for (i, line) in text.lines().enumerate() {
        let l = line.trim_start();
        if l.starts_with("//") { continue; }
        for needle in ["std::os::", "libc::", "cfg(unix", "cfg(windows", "cfg(target_os"] {
            if l.contains(needle) {
                out.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
}
