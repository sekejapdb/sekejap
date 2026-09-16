#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH
export CARGO_HOME=<scratch>
export CARGO_TARGET_DIR=<scratch>
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
mkdir -p "$R/logs" "$R/src" "$TMPDIR"
finish() {
 local rc=$?
 echo "PHASE1_R2_EXIT=$rc"
 echo "$rc" > "$R/exit-code"
 cp -a <scratch> "$R/r1-logs"
 tar -czf "$R/evidence.tar.gz" -C "$R" logs r1-logs exit-code source-r2.tar.gz src/_phase1/source-manifest.json
 sha256sum "$R/evidence.tar.gz"
 sleep 300
 exit "$rc"
}
trap finish EXIT
echo '0fc052d05f55ec48eb3c45ca129319a8a97cb2d0ff15de2901131dbd12a71075  <scratch>' | sha256sum -c -
tar -xzf "$R/source-r2.tar.gz" -C "$R/src"
cd "$R/src"
rustc --version
cargo --version
cp kernel/src/meta.rs "$R/fixed-meta.rs"
cp _phase1/before/kernel/src/meta.rs kernel/src/meta.rs
echo STAGE=negative-version-admission
set +e
cargo test --release --locked --offline --test format_replica_refusal store_version_admission_precedes_version_specific_payload_parsing -- --exact --test-threads=1 > "$R/logs/negative-version-admission.log" 2>&1
neg=$?
set -e
cp "$R/fixed-meta.rs" kernel/src/meta.rs
tail -30 "$R/logs/negative-version-admission.log"
if [ "$neg" -eq 0 ] || ! grep -q 'store_version_admission_precedes_version_specific_payload_parsing ... FAILED' "$R/logs/negative-version-admission.log"; then
 echo NEGATIVE_CONTROL_INVALID; exit 1
fi
echo ADMISSION_NEGATIVE_CAUGHT=1
run() {
 local name=$1; shift
 echo "STAGE=$name START $(date -u +%FT%TZ)"
 set +e
 "$@" > "$R/logs/$name.log" 2>&1
 local rc=$?
 set -e
 grep -E '^test result:|^error:|^failures:|FAILED|panicked' "$R/logs/$name.log" || true
 echo "STAGE=$name EXIT=$rc $(date -u +%FT%TZ)"
 if [ "$rc" -ne 0 ]; then tail -60 "$R/logs/$name.log"; return "$rc"; fi
}
run focused-default cargo test --release --locked --offline --test format_replica_refusal --test format_v1_compat --test write_path -- --test-threads=1
run corrected-limits cargo test --release --locked --offline --lib pagewal::hint_tests::runtime_limits_refuse_before_framing_and_survive_rollback -- --exact --test-threads=1
run workspace-default cargo test --workspace --release --locked --offline --no-fail-fast -- --test-threads=1
run workspace-compact cargo test --workspace --release --locked --offline --no-fail-fast --features compact-cells,sqlite-balance -- --test-threads=1
run workspace-retained cargo test --workspace --release --locked --offline --no-fail-fast --features compact-cells,sqlite-balance,keyspace-append,slotref-split -- --test-threads=1
