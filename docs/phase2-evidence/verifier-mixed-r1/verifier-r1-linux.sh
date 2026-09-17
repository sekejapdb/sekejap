#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch>
export SQLITE_TMPDIR=$TMPDIR
[ -f "$R/mixed-query-r1.exit" ]
test ! -e "$R/verifier-r1-src"
cp -a "$R/mixed-query-r1-src" "$R/verifier-r1-src"
tar -xzf "$R/verifier-r1-overlay.tar.gz" -C "$R/verifier-r1-src"
cd "$R/verifier-r1-src"
python3 - <<'INNER'
from pathlib import Path
p=Path('src/collections.rs');s=p.read_text();hook='#[path = "index_verifier.rs"]\npub mod verification;\n';assert hook not in s;needle='#[path = "query.rs"]';assert s.count(needle)==1;p.write_text(s.replace(needle,hook+needle))
INNER
python3 "$R/multimodel-fixtures-r1-inventory.py" "$R/verifier-r1-source.json"
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline --test index_verifier "${flags[@]}" > "$R/logs/verifier-r1-$mode.log" 2>&1
 cargo test --release --locked --offline --lib pagewal::current_reader:: "${flags[@]}" >> "$R/logs/verifier-r1-$mode.log" 2>&1
 printf 'VERIFIER_R1_PASS %s\n' "$mode"
done
