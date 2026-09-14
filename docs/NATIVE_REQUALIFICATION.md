# Linux qualification of bounded page packing

Decision: **retain the optional compact-cell / bounded redistribution change
for its repeatable size improvement**, with full Linux correctness passing.
This is not a claim of a repeatable 10% speedup or full-engine release readiness.
The page-WAL public collection switch remains pending; all seven laws stay intact.

The owner requests Linux-first acceptance because the Mac runs other development
work. The Pi at `203.0.113.10` is the primary device; server server is a separate
server comparator. Mac database tests are deferred until the owner reboots it
and the independent buffered-I/O control passes. Reboot is not assumed to resolve
the stale reads documented in [SCAN_IO_ISOLATION.md](SCAN_IO_ISOLATION.md).

## Inputs and acceptance

The baseline is `09db9a8` with shared test improvements. The candidate is the
frozen V3 implementation from [PAIR_PACKING.md](PAIR_PACKING.md): compact ordinary
inline cells and redistribution into existing neighboring capacity, falling back
to ordinary splitting when that capacity is exhausted. No SQL, public API switch
or new multimodel indexes are included.

Both variants receive an identical stronger 200K/800K shuffled Store oracle.
The inverse of the fixture's key multiplier proves each key belongs to the
expected input domain. Combined with strict ordering and exact count, this
proves complete key membership in constant extra memory. Every value is also
checked. No diagnostic per-page digest map is included in these engine builds.

Full native workspace tests use release mode, `sqlite-balance,compact-cells`,
and one test thread. Their own configured budgets apply; they are **not** all
run under the benchmark's address-space cap. Benchmark binaries are rebuilt
without test-support instrumentation, in separate variant targets. Source and
binary hashes are retained. `MALLOC_ARENA_MAX=1` is identical across variants
and SQLite; each benchmark process has a 128 MiB address-space limit.

The [lean groups](FOUNDATION_LEAN_GROUPS.json) now also select compact-cell bounds,
the three bounded-partition unit tests and the four FileIo tests. Every added
test passed in the native full suites; the regrouped lean command itself was
not rerun in this loop. This keeps the discovered portability and format edges
in the next routine foundation check.

Fresh qualification uses three rotations of baseline E4, candidate E4 and SQLite:

- Load 10K/100K/1M rows, then 1,000 inserts, updates and deletes, separately for
  local and scattered keys; verify exact final values and membership.
- Load 400K rows, then 12 rounds of 80K updates, 40K deletes and 40K fresh inserts
  per round: 1.92M mutations. Report load separately from mutation time, including
  commits and ending checkpoints, final disk size and sampled process expansion.
- A 1K-row, six-round variable-value resize workload.
- A 10K-row disk-budget workload with no reader, a held reader and rolling readers.

That is 81 benchmark arms per host. Raw-KV figures exclude typed collection
metadata and secondary indexes. Fixed-work disk checks are phase-boundary values;
the mixed/resize peak sampler provides a lower bound, not an enforced ceiling.
The cap workload separately checks refusal before managed logical allowance is
exceeded. The runtime target is below 1.5× SQLite, size target at most 1.10×,
with a 2× loaded-size expansion target. Strict Law 2 and the other outstanding
[F1 gates](FOUNDATION_GATES.json) are not waived by a useful packing improvement.

## Test portability findings

The initial native run stopped at Mac-only artifact-directory assertions in
older storage tests. R2 permits only the already authorized Pi and server artifact
roots in those guards; storage assertions and resource limits remain unchanged.

Pi R2 then failed `concurrent_direct_and_buffered_files_keep_their_own_bytes`
with `EINVAL` at the write, in all four Direct workers. That test used ordinary
`Vec<u8>` buffers despite FileIo's explicit buffer-address alignment contract.
The related Direct fallback test had the same invalid assumption. Both now use
4 KiB-aligned test buffers. This is a shared test-only correction, not a change
to Direct mode, fallback behavior or the storage implementation. Original logs
remain preserved; `aligned-test.patch` records the exact follow-up delta.

server R2 baseline completed the full workspace suite before that correction.
The aligned follow-up completed full workspace suites for both variants on
both hosts, before fresh timing runs began.

Both hosts completed 367 successful reported baseline test executions and
371 candidate executions, with two ignored tests per variant. These totals
include subprocess helpers. Seven non-compact pack-shape tests additionally
pass on server, preserving the original format golden. The candidate's original server suite stopped at
`pack_tree_produces_identical_tree`: the old golden assumes four-byte ordinary
cell framing, while compact framing uses two bytes. Its 50K-row tree therefore
contains 248 leaf pages instead of 275. Before changing the golden, an extended
test independently constructed every expected cell byte, verified every point
lookup and ordered scan row, checked every leaf's next pointer and fill boundary,
and ran the published-tree verifier on all three fixture datasets. Those checks
passed. The legacy golden remains for builds without compact cells.

The compact exact-fit fixture uses four value bytes instead of two so both
leaf and interior records still land exactly on the 3,650-byte target boundary.
The final-single-child fixture uses 33,496 rows instead of 30,857 in compact
mode, retaining 203 full leaves followed by the final child. These changes
preserve the original boundary coverage after the format becomes smaller.
The explicit candidate-only patch is
[native_requal_format_fixture.patch](../tools/native_requal_format_fixture.patch).

## Artifacts

Pi root:
`<scratch>`

server root:
`<scratch>`

Initial attempt logs remain at each root. R2 frozen archives/manifests, initial
R2 logs, the test-only patch and final `aligned-run` logs are under `r2/`.
The isolated server Job is `e4-native-requal-r2-20260914` in namespace
`sekejap-benchmark`, on node `server`, limited to two CPUs and 2 GiB memory.

Runners: [native_requal.sh](../tools/native_requal.sh),
[native_requal_aligned.sh](../tools/native_requal_aligned.sh),
[run_scatter_loop.py](../tools/run_scatter_loop.py).


## Fresh repeated measurements and decision

All **162 benchmark arms pass their exact state oracles** (81 per host).
Each ordinary workload has three repetitions in rotated engine order. MB below
means 1,000,000 bytes. Times are medians; all individual samples remain in
[NATIVE_REQUALIFICATION_RESULTS.json](NATIVE_REQUALIFICATION_RESULTS.json).
[Validation counts and hashes](NATIVE_REQUALIFICATION_VALIDATION.json) are separate.

### 400K load and 1.92M subsequent changes

Load inserts 400,000 rows. Subsequent work makes 12 rounds of 80,000 updates,
40,000 deletes and 40,000 fresh inserts each. The population remains 400,000.
Mutation seconds exclude load and verification, and include transaction commits
and each round's ending checkpoint. No rebuild or VACUUM is included.

| Platform | Work | Baseline E4 | Retained compact E4 | SQLite |
| --- | --- | ---: | ---: | ---: |
| Pi | Load seconds | 8.796 | 10.594 | 9.362 |
| Pi | Mutation seconds | 104.451 | 100.169 | 116.386 |
| server | Load seconds | 4.281 | 4.211 | 4.721 |
| server | Mutation seconds | 54.714 | 50.879 | 43.882 |

Compact E4's fresh mutation gain over baseline is **4.1% on Pi / 7.0% on server**.
Its mutation time is **0.861× / 1.159× SQLite**, within the owner's 1.5× target
for this workload. This does **not** reproduce the previous 17.0% / 10.2% gain.
Pi has considerable timing variation: baseline mutation trials are
104.451 / 113.346 / 97.657 s; candidate 97.177 / 100.169 / 100.748 s;
SQLite 106.448 / 118.810 / 116.386 s. Candidate load also varies
10.594 / 15.097 / 8.918 s. Preserve these rather than select favorable trials.

The reason to retain this change is the deterministic density gain: final
mixed-workload bytes fall **6.67% versus baseline**, and the previously failing
100K scattered size case moves from **1.142× to 1.075× SQLite**. The change
passes the complete native correctness suites and the transaction-capacity
regression. It is not accepted on an unproven 10% timing improvement.

Logical file sizes are identical across hosts/repetitions:

| 400K footprint | Baseline E4 MB | Retained compact E4 MB | SQLite MB |
| --- | ---: | ---: | ---: |
| After load | 118.075392 | 110.202880 | 117.669888 |
| Final after all changes | 129.888256 | 121.229312 | 129.445888 |
| Largest sampled logical footprint | 134.854240 | 125.885696 | 134.258128 |
| Sampled logical peak / loaded size | 1.142× | 1.142× | 1.141× |

### Allocated space: the physical cap remains open

The logical footprint is not the complete disk-full answer. The following uses
`st_blocks * 512`, the filesystem's reported allocated blocks, and shows the
**maximum across all three trials**, not the median of peaks:

| Platform | Baseline E4 peak MB | Compact E4 peak MB | SQLite peak MB |
| --- | ---: | ---: | ---: |
| Pi | 134.860800 | 125.894656 | 134.266880 |
| server | 272.367616 | 263.061504 | 271.745024 |

Pi peaks remain approximately 1.14× their loaded allocation. On server, the worst
per-trial expansion is **2.307× baseline / 2.387× compact / 2.309× SQLite**.
The server mount reports `overlayfs`; these counters are the allocation exposed
by that filesystem, not a measurement of all underlying storage-system copies.
The cause and full underlying-device allocation are not established here.
The observation nevertheless prevents a claim that our logical admission cap
proves physical space stays under 2×. All three engines cross that measured
allocated-space target on server. Smaller final files do not remove this gap.

### Fixed 1,000-operation ladder

Each row below means exactly 1,000 operations at the stated existing population,
with an empty engine cache at phase start; OS caches are retained. Values are
**baseline E4 / retained compact E4 / SQLite seconds**, all including commit and
ending checkpoint. The final footprint follows insert, update and delete phases.

| Platform | Existing rows | Key placement | Insert seconds | Update seconds | Delete seconds | Final MB, E4 base / compact / SQLite |
| --- | ---: | --- | --- | --- | --- | --- |
| Pi | 10,000 | local | 0.020 / 0.020 / 0.027 | 0.025 / 0.020 / 0.025 | 0.023 / 0.023 / 0.021 | 3.256 / 3.043 / 3.281 |
| Pi | 10,000 | scattered | 0.355 / 0.142 / 0.179 | 0.412 / 0.119 / 0.147 | 0.347 / 0.129 / 0.157 | 3.834 / 3.682 / 3.731 |
| Pi | 100,000 | local | 0.048 / 0.025 / 0.036 | 0.038 / 0.024 / 0.023 | 0.051 / 0.023 / 0.026 | 29.823 / 27.836 / 29.749 |
| Pi | 100,000 | scattered | 1.323 / 1.295 / 1.334 | 0.326 / 0.291 / 0.666 | 0.299 / 0.307 / 0.648 | 33.620 / 31.654 / 29.450 |
| Pi | 1,000,000 | local | 0.030 / 0.049 / 0.044 | 0.055 / 0.055 / 0.033 | 0.043 / 0.057 / 0.032 | 295.465 / 275.771 / 294.416 |
| Pi | 1,000,000 | scattered | 1.668 / 1.406 / 0.560 | 0.640 / 0.552 / 0.782 | 0.275 / 0.698 / 1.218 | 299.266 / 279.589 / 294.117 |
| server | 10,000 | local | 0.013 / 0.022 / 0.016 | 0.014 / 0.027 / 0.015 | 0.018 / 0.021 / 0.017 | 3.256 / 3.043 / 3.281 |
| server | 10,000 | scattered | 0.103 / 0.095 / 0.105 | 0.081 / 0.086 / 0.051 | 0.085 / 0.080 / 0.050 | 3.834 / 3.682 / 3.731 |
| server | 100,000 | local | 0.015 / 0.017 / 0.031 | 0.019 / 0.012 / 0.016 | 0.015 / 0.017 / 0.013 | 29.823 / 27.836 / 29.749 |
| server | 100,000 | scattered | 0.252 / 0.226 / 0.093 | 0.126 / 0.139 / 0.090 | 0.172 / 0.140 / 0.103 | 33.620 / 31.654 / 29.450 |
| server | 1,000,000 | local | 0.017 / 0.020 / 0.022 | 0.022 / 0.021 / 0.018 | 0.017 / 0.019 / 0.015 | 295.465 / 275.771 / 294.416 |
| server | 1,000,000 | scattered | 0.298 / 0.318 / 0.105 | 0.121 / 0.090 / 0.133 | 0.125 / 0.097 / 0.140 | 299.266 / 279.589 / 294.117 |

Strict flat-latency Law 2 remains unqualified. For example, 1M scattered inserts
cost **1.406 s compact E4 / 0.560 s SQLite on Pi**, and **0.318 / 0.105 s on
server**: 2.51× and 3.03× SQLite, beyond the 1.5× target. There are regressions
versus baseline in individual medians too (Pi 1M scattered deletes, server 1M
scattered inserts); the complete table is retained. Do not describe this as a
universal speed improvement.

### Resize and retained readers

The 1K variable-size fixture intentionally grows and shrinks values. Its final
files remain **3.289 MB E4 / 1.868 MB SQLite (1.761×)**, unchanged by packing.
Its peak/initial ratio is not a stable-payload expansion test because the
payload itself changes drastically. Mutation medians (baseline / compact /
SQLite) are **0.347 / 0.352 / 0.182 s on Pi**, **0.248 / 0.293 / 0.120 s on
server**. Compact E4 is 1.94× / 2.44× SQLite; resize parity remains open.

Under the separate 10K logical cap, both E4 variants complete all 120,000 updates
without a reader. Held and rolling readers both safely stop at 9,000 committed
updates before exceeding the allowance; SQLite's harness has different stopping
semantics and can observe crossing only afterward. These are verified refusal
and exact-state checks, not evidence that the complete reader workload fits,
not latency benchmarks and not proof of the allocated-space cap.

## Reproduction and retained limitations

Native full suites: `cargo test --release --offline --workspace --features
sqlite-balance,compact-cells -- --test-threads=1`. The final Pi candidate log is
`r2/aligned-run/candidate-final-workspace.log`; server uses
`r2/aligned-run/candidate-workspace.log`. Baseline logs use
`r2/aligned-run/baseline-workspace.log` on both. Runners explicitly set
`MALLOC_ARENA_MAX=1`; fresh release benchmark binaries omit test-support.

Frozen final baseline and candidate manifests match on both hosts:

- Baseline: `62688e3946ac26f3d56b9bc24e0946718ff7aa10d43054e9ac51b787140c5ee3`
- Candidate: `190ce364762660d6ae3dfe0fad4f342c5d600e50c804a59edd9357c8638f3a3a`

Qualified source archives/manifests and binary hashes remain at each `r2` root.
The retained executable code matches those tested sources. A comment afterward
was corrected to describe the actual window of up to three existing leaves;
there is no post-test executable change. No Mac database test was run.

The seven laws remain seven and timestamps remain off by default. Scattered
latency, resize density/time, full reader progress/latency, physical allocation
admission, broader repair/fault coverage, typed page-WAL integration and later
indexed workloads remain release work. The F1 promotion checker must continue
to refuse whole-engine promotion.

## Evidence retention and cleanup

Verified metadata archives remain on each native host with a second hash-matching
copy collected for this loop. Source archives, binaries, all raw reports and
original failure logs remain; earlier loops and Mac corruption evidence were
not touched. Only 81 completed benchmark databases per host and individually
passing large shuffled fixtures were removed. Reclaimed allocated space:
**8,485,056,512 bytes Pi + 8,974,200,832 bytes server = 17,459,257,344 bytes**.
See [per-directory cleanup accounting](NATIVE_REQUALIFICATION_CLEANUP.json).

Metadata archive SHA-256:

- Pi: `efb77f785d13a6f85910f5b47be8f9f7c4811efcffca68609db591e65d96ef07`
- server: `40207e28e4dcd92bbe9b20c9586cc04248712c798cbac45c7cfec8b6514cd202`
