#!/bin/sh
set -eu
ROOT=<scratch>
BIN="$ROOT/lifecycle-after"
"$BIN" --space-check "$ROOT/fixed" > "$ROOT/fixed.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-load-100k" 100000 load 1000 > "$ROOT/frequent-load-100k.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-load-400k" 400000 load 1000 > "$ROOT/frequent-load-400k.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-mixed-100k" 100000 mixed_none 1000 > "$ROOT/frequent-mixed-100k.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-mixed-400k" 400000 mixed_none 1000 > "$ROOT/frequent-mixed-400k.log" 2>&1
