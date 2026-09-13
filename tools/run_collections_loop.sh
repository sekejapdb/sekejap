#!/bin/sh
set -eu
base=${E4_COLLECTION_ARTIFACTS:-<scratch>
case "$base/" in
  *'/../'*) exit 1 ;;
  <scratch>) ;;
  *) exit 1 ;;
esac
mkdir -p "$base"
for repeat in 1 2; do
  if [ "$repeat" = 1 ]; then engines='e4 sqlite'; else engines='sqlite e4'; fi
  for rows in 100000 400000; do
    for times in off on; do
      for engine in $engines; do
        target/release/collections "$base/run-$repeat" "$engine" "$rows" "$times" > "$base/run-$repeat-$engine-$rows-$times.log" 2>&1
      done
    done
  done
done
for engine in e4 sqlite; do
  target/release/collections "$base/sustained" "$engine" 400000 off 12 > "$base/sustained-$engine.log" 2>&1
done
