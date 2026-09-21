#!/bin/bash
set -euo pipefail
root=<scratch>
export TMPDIR="$root/tmp"
set +e
cargo test --release --offline --features sqlite-balance,compact-cells --test version_reuse > "$root/version-red.log" 2>&1
red=$?
set -e
if [[ $red == 0 ]]; then echo 'Expected regression did not fail'; exit 1; fi
rg -q 'intermediate versions accumulate' "$root/version-red.log"
python3 /tmp/e4_version_edit.py
python3 /tmp/e4_policy_edit.py
python3 /tmp/e4_policy_tests.py
python3 /tmp/e4_sustained_profile.py
python3 /tmp/e4_policy_crash.py
cargo test --release --offline --features sqlite-balance,compact-cells --test version_reuse --test reader_release > "$root/reuse-green.log" 2>&1
cargo test --release --offline -p kernel byte_policy > "$root/policy-tests.log" 2>&1
cargo build -p sekejap-dist --release --offline --features sqlite-balance,compact-cells --bin lifecycle > "$root/build.log" 2>&1
cp target/release/lifecycle "$root/lifecycle-candidate"
