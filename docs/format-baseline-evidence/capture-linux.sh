#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:$PATH
export CARGO_HOME=<scratch> CARGO_TARGET_DIR=<scratch>
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export TMPDIR=$R/tmp SQLITE_TMPDIR=$R/tmp
mkdir -p "$R/logs" "$R/baseline-src" "$R/bin" "$TMPDIR"
finish() { local rc=$?; echo "REFERENCE_EXIT=$rc"; echo "$rc" > "$R/exit-code"; sleep 300; exit "$rc"; }
trap finish EXIT
echo 'dec765694b0a9b907c6729bf310275479afb7692ae6f53eac0e84c6c98e75dae  <scratch>' | sha256sum -c -
tar --exclude='._*' --no-same-owner --warning=no-unknown-keyword -xzf <scratch> -C "$R/baseline-src"
cd "$R/baseline-src"
test "$(git rev-parse HEAD)" = 59d1cbc770284f160ffda53cc1ee545167733d11
test -z "$(git status --porcelain)"
rustc --version; cargo --version
cargo build --release --locked --offline --bin format_fixture --bin pagewal_repair > "$R/logs/build-generator.log" 2>&1
cp "$CARGO_TARGET_DIR/release/format_fixture" "$R/bin/format_fixture-59d1cbc"
sha256sum "$R/bin/format_fixture-59d1cbc"
test ! -e "$R/corpus"
"$R/bin/format_fixture-59d1cbc" "$R/corpus"
sha256sum "$R/corpus/INDEX.json"
tar -czf "$R/corpus.tar.gz" -C "$R" corpus
printf 'BASELINE_CORPUS_READY\n'
while [ ! -f "$R/continue.sh" ]; do sleep 5; done
bash "$R/continue.sh"
