# Recovery loop R1 — 2026-09-10

The new `kernel::recover::recover_to(source, new_destination, config)` path
recovers unaffected records around damaged overflow values, reports damaged
page extents without trusting their kind bytes, and does not merge obsolete
CoW leaves into the current tree. Source data, WAL and freelist remain intact.
This is kernel salvage; typed schema recovery and the complete seven-law gate
remain open in tracker.

## Fail → fix → verify

The first three independent fault oracles failed against the inherited
in-place recovery: a broken overflow aborted the rebuild, a changed leaf kind
hid lost rows, and a checkpointed delete was resurrected from an obsolete leaf.
The red log is `<scratch>`.

The source-preserving API passes those cases and the expanded 14-test suite:
crossed valid overflow chains, cycles, wrong identities, missing and truncated
tails, malformed checksummed cells, damaged root/meta evidence, occupied/aliased
destinations, active source writers, committed WAL updates and damaged WAL.
A separate kernel test injects interruption at six repair phase boundaries
and verifies preservation plus retry into another destination.

Tests compare values/deletes against independently generated expected records;
they compare source file bytes before/after. They do not claim that a phase
interruption is equivalent to every possible failed syscall or power cut.
See `RECOVERY_MATRIX.json` for precise coverage and pending cases.

Final validation: **284 passed, 0 failed, 0 ignored** in the feature-enabled
workspace (`recovery-r1-validated.log`); the 14 recovery fault tests also pass
with default features (`recovery-r1-default.log`). Both logs are under
`<scratch>/`. The full run includes density, snapshots,
WAL/durability, corruption bounds and the new concurrent I/O regression.

The older `kernel::recover::recover` remains a low-level forensic regression
path, with an explicit warning that it can resurrect stale rows. It is not
used by the new command. E3 source and its tracker journey were not changed.

## Concrete workflow

Build from the E4 root:

```sh
cargo build --release --offline --features sqlite-balance,compact-cells --bin recover
target/release/recover inspect <scratch>
target/release/recover salvage <scratch> <scratch>
target/release/recover verify <scratch>
```

`inspect` checks physical page CRC/identity; it is not a claim of valid values
or current membership. `salvage` locks the source against cooperating writers
through an OS read-only descriptor, requires a new destination, builds there,
independently verifies the replacement and replays copied WAL there. It never
publishes over the source. Budgets below 8MiB are refused.

Result files:

- `database/`: verified output tree and values, including verified WAL replay.
- `losses.tsv`: streaming known-key losses and unknown extents; binary keys and
  authenticated parent bounds use hex. Lower bounds are inclusive; upper bounds
  are exclusive. No in-memory list proportional to the number of losses.
- `candidates.raw`, when ancestry is damaged: raw valid source cells with page
  and generation, including potentially obsolete versions. No version winner
  is inferred. Overflow markers still refer to source pages, not literal values.
- `COMPLETE`: kernel completion record, written after output verification.
- `report.json`: command report, counters, classification and evidence paths.

`VerifiedSubset` means the surviving output has verified membership evidence;
it does not mean no rows were lost. Consult known losses and unknown extents.
`MembershipUncertain` means meta/WAL damage prevents that membership claim.
Rootless cells always remain separate candidates, even when an unaffected
subtree produced a verified subset. Never automatically promote candidates to
current data. Retrying an incomplete attempt uses a new destination and retains
the earlier evidence.

Raw archive version 1 begins with `E4RAW001`. Each frame is
`page:u32LE | generation:u64LE | cell_length:u32LE | source_cell | crc32c:u32LE`;
the checksum covers the frame header and cell. Cell length is bounded by a
source page. The archive deliberately preserves generic/compact cell framing.

## Retained 40K mixed-data trial

`tools/recovery_trial.py` made clean and damaged copies of the P1 40K large-JSON
fixture, then exercised all three commands. Output:
`<scratch>`.

| Case | Entities recovered | Catalog records | Named value losses | Repair time |
|---|---:|---:|---:|---:|
| Clean copy | 40,000 | 3 | 0 | 0.201 s |
| One damaged overflow page | 39,999 | 3 | 1 | 0.235 s |

Both outputs passed independent command verification. SHA-256 comparisons
confirmed source data/WAL/free unchanged, including the original benchmark.
These are single local warm-cache repair trials, not new ingest measurements.
The damaged source, mutation offset, loss journal and repaired result are retained.

## Additional foundational defect: macOS uncached I/O

The full suite exposed a direct read returning another file's bytes after a
zero-page write. Concurrent isolated I/O tests reproduced it without pager or
recovery activity. Retained expected/observed files are named in
`<scratch>` and
`recovery-r1-workspace-final.log`.

`tools/nocache_probe.c` then reproduced the symptom independently of Rust/E4:
8 native threads, 200 fresh-file round trips each, zero/distinctive payloads,
aligned and ordinary buffers, `pwrite → fsync → pread`. On this macOS + USB
APFS scratch stack, F_NOCACHE had **23 mismatches / 1,600** (13 aligned, 10
ordinary); the same buffered control had **0 / 1,600**. Sequential probes had
no mismatches. This establishes a failure on the tested stack, not whether
the OS, filesystem or device is ultimately responsible.

Evidence: `<scratch>`
and `<scratch>`.
Apple documents F_NOCACHE as a control for data caching
([fcntl documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html));
the local experiments establish the observed failure, not that documentation.

E4 now refuses the macOS uncached path and uses its existing explicitly
reported `Buffered` fallback. **Sacrifice:** a macOS caller requesting Direct
uses the OS cache until a reliable support boundary is demonstrated. Other
platform implementations are unchanged. P1 and P2 already used Buffered,
so their measured configuration is unchanged. Re-enabling F_NOCACHE requires
passing the retained concurrent repro on the affected device class.

## Costs and remaining gates

No new bytes are added to normal entity storage and no ingest/query data path
is rewritten by salvage. Repair pays a verified ancestry walk, source overflow
preflight and relocation, external sorting/packing, and independent output
verification. Broken ancestry adds a full source scan and raw archive. Disk
space temporarily includes source, replacement, sort scratch and any evidence.

The sort arena is 1/4 of configured memory and the build pool 1/2; tree depth
is capped at 64 and losses stream. This is an allocation design, **not yet a
measured hard process-memory bound** for the entire sort/WAL pipeline.

Still open: independent typed layout discovery/all-copy loss, complete current
membership after destroyed ancestry, raw-candidate decoding workflow, explicit
source-read/destination-write/sync/rename fault injection, real process-kill
publication tests, memory scaling, and broader lifecycle/snapshot/device gates.
The full foundation is not declared converged.
