#!/bin/bash
# Run only in the explicitly authorized Pi workspace. Source is unpacked by
# the controller before launch; never edit this file while it is executing.
set -euo pipefail
root=<scratch>
test "$USER" = device
cd "$root"
printf '%s  %s\n' dbbbbcdd24b5c1b58fbc3bc32db282e5707be7089e765a5e0cf3f77ad2b5d086 vendor.tar.gz | sha256sum --check
test -x toolchain/bin/rustc
test -f src/Cargo.lock
tar -xzf vendor.tar.gz
mkdir -p src/.cargo cargo-home tmp artifacts
cat > src/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "../../vendor"
EOF
# Cargo resolves the vendor path relative to the workspace root.
sed -i "s|../../vendor|$root/vendor|" src/.cargo/config.toml
export PATH="$root/toolchain/bin:$PATH"
export CARGO_HOME="$root/cargo-home"
export TMPDIR="$root/tmp"
export SQLITE_TMPDIR="$root/tmp"
export CARGO_BUILD_JOBS=2
cd src
rustc --version > "$root/artifacts/build-versions.txt"
cargo --version >> "$root/artifacts/build-versions.txt"
cargo test --offline --release -p kernel --features sqlite-balance,compact-cells --test resource_limits -- --test-threads=1 > "$root/artifacts/resource-tests.log" 2>&1
cargo test --offline --release -p kernel --features sqlite-balance,compact-cells --lib constrained_commit -- --test-threads=1 > "$root/artifacts/commit-fault-tests.log" 2>&1
cargo build -p sekejap-dist --offline --release --features sqlite-balance,compact-cells --bin lifecycle > "$root/artifacts/build.log" 2>&1
cp target/release/lifecycle "$root/artifacts/lifecycle-pi"
sha256sum "$root/artifacts/lifecycle-pi" > "$root/artifacts/lifecycle-pi.sha256"
date -Is > "$root/artifacts/build-complete"
