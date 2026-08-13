# Mega benchmark — history

Compact results of `cargo bench --bench mega_benchmark`: **sekejap vs in-memory
SQLite** across 20 scenarios (filtering, sort, graph, spatial, vector, hybrid) on
20k venues + a graph + 64-dim embeddings.

This log is **run on demand, not per release** — the benchmark is heavy, so we
capture a snapshot when it's worth comparing (e.g. before/after a feature like
snapshot reads). Each entry is dated and tied to the commit it ran at, so a
feature's impact is visible across entries. Newest first.

To capture a run:
```bash
cargo bench --bench mega_benchmark        # run it
scripts/mega-bench-capture.sh --prepend   # prepend a compact entry here, then commit
```

`sekejap` column = the faster of the SQL / atomic surfaces. `sqlite` runs
in-memory with all applicable indexes + R*Tree (so several rows are disk-vs-RAM,
not apples-to-apples — noted where it matters). `vs sqlite` > 1 = sekejap faster.

<!-- entries -->
