# Recovery blocker evidence — 2026-09-16

`linux-v4.stdout.log` is the retrieved summary from server Job
`e4-recovery-tests-20260915-v4`. `negative-fixed-order.stdout.log` is the
read-only retrieval and validation of the deliberately failing fixed-order
counterexample. Verify these two checked-in logs with `SHA256SUMS`.

The complete tested source archive is intentionally not checked in. Its
SHA-256 is:

```text
9729f1b5b72afff0bba0056700bedb91692f747793ba387644dcb509c02f8153  e4-recovery-source-20260915-v2.tar.gz
```

Before reporting the result, the nine paths in `TESTED_SOURCE.sha256` were
verified byte-for-byte against that extracted archive. The fixed-order negative
control copied the same source and changed only `src/pagewal.rs`; its before and
after hashes are printed in the negative log.

These artifacts support the bounded recovery result in
`docs/RECOVERY_BLOCKERS_LOOP.md`. They do not establish release compatibility
or whole-engine qualification against all eight laws.
