#!/bin/sh
set -eu
ROOT=<scratch>
BIN=target/release/lifecycle
"$BIN" --space-check "$ROOT/frequent-updates-100k" 100000 updates 1000 > "$ROOT/frequent-updates-100k.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-updates-400k" 400000 updates 1000 > "$ROOT/frequent-updates-400k.log" 2>&1
"$BIN" --space-check "$ROOT/double-updates-100k" 100000 updates 0 2 > "$ROOT/double-updates-100k.log" 2>&1
"$BIN" --space-check "$ROOT/double-updates-400k" 400000 updates 0 2 > "$ROOT/double-updates-400k.log" 2>&1
"$BIN" --space-check "$ROOT/double-frequent-100k" 100000 updates 1000 2 > "$ROOT/double-frequent-100k.log" 2>&1
"$BIN" --space-check "$ROOT/double-frequent-400k" 400000 updates 1000 2 > "$ROOT/double-frequent-400k.log" 2>&1
"$BIN" --space-check "$ROOT/frequent-long-100k" 100000 mixed_long 1000 > "$ROOT/frequent-long-100k.log" 2>&1
