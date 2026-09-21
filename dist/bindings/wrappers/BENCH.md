# Wrapper overhead benchmark

How much does using sekejap from each language cost versus native Rust? This runs
the **same workload** — N point-lookup queries (`SELECT v FROM t WHERE _key = 'k500'`)
against a disk-backed DB — through every binding and compares to native `CoreDB`.

Run it:

```bash
./wrappers/bench.sh          # env N=<iters>, default 50000
```

## Results (30k queries, Apple Silicon, single run)

| lang | ops/sec | µs/op | vs Rust | mechanism |
|---|---:|---:|---:|---|
| **rust** (native `CoreDB`) | 321,159 | 3.11 | 1.00× | native, no FFI |
| c | 301,714 | 3.31 | 1.06× | C ABI (JSON string) |
| python (PyO3) | 301,279 | 3.32 | 1.07× | native CPython extension |
| swift (SwiftPM) | 290,534 | 3.44 | 1.11× | C ABI (JSON string) |
| node (napi-rs) | 285,948 | 3.50 | 1.12× | C ABI (JSON string) |
| go (cgo) | 277,580 | 3.60 | 1.16× | C ABI (JSON string) |
| kotlin (Panama/FFM) | 222,476 | 4.50 | 1.44× | C ABI via FFM (JDK 22+) |

*(Absolute ops/sec vary per machine and run; the ratios are the stable signal.)*

## What it means

- **Every wrapper lands within ~1.06–1.44× of native Rust** — using sekejap from
  another language is nearly free. That's the payoff of a thin C ABI (and, for
  Python/Node, purpose-built native-extension FFIs).
- **Python (PyO3) is right at the top (~1.07×)** — even though it materializes native
  Python objects. PyO3 builds them directly from `Hit`s and **skips the JSON serialize**
  the C ABI does, so it keeps pace with the C-ABI bindings that only fetch a string.
- **Kotlin uses Panama / FFM (JDK 22+)** at ~1.44× — the slowest, but a big step up
  from the JNA it replaced (~2.1×). We measured the three JVM options head-to-head:
  **JNI ≈ 1.15×**, **Panama ≈ 1.44×**, **JNA ≈ 2.10×**. JNI is fastest but needs a C
  shim compiled+shipped per platform; Panama is pure JVM (no shim, no JNA jar) and
  drops the extra dependency — chosen for the cleanest modern binding. FFM's per-call
  cost (downcall dispatch + `MemorySegment` string marshalling) is the ~0.3× vs JNI.

## Fairness notes

- **Two different mechanisms.** The C-ABI bindings (c/go/node/swift/kotlin) receive
  the result as a **JSON string and discard it** (no parse) — overhead is FFI +
  the C ABI's Rust-side JSON serialize. **Python (PyO3)** and native Rust
  **materialize result objects** with no JSON. So the categories do slightly
  different work; each represents the *idiomatic* way to run a query in that language.
- Native Rust (`db.query().collect()`) is the floor: materialized `Hit`s, no JSON.
- Every binding: disk-backed `open`, 1000-row table, warm-up call before timing.
