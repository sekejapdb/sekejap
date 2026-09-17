# Phase 2 same-copy rollback qualification

Candidate evidence,2026-09-17: the authorized Raspberry Pi passed **416 full
rollback cycles over104 original fixture databases**, plus208 pairwise semantic
handoffs and200 admission checks using an actual preserved earlier engine.
No Phase2 release, format freeze or query-performance acceptance is claimed.

This closes the missing *test behavior*: the preserved writer now commits over
bytes written by the comparison build on the **same copy**. Earlier pairwise
checks started each direction from an independent copy and could not prove it.
These new full cycles use default/retained builds of one captured engine
revision. They prepare the first baseline for later cross-release tests; they
are not historical released-version evidence.

| Corpus | Originals | Pairwise handoffs | Earlier-engine admission checks | Full rollback cycles |
|---|---:|---:|---:|---:|
| Scalar and graph | 4 | 8 | 0 | 16 |
| Exact/quantized vector, spatial, text and combined | 20 | 40 | 40 | 80 |
| Graph-independent Ready/Building/Dropping/post-drop | 80 | 160 | 160 | 320 |
| Total | 104 | 208 | 200 | 416 |

Every cycle covers one of four boundary pairs: checkpointed or committed WAL
at the first handoff, independently checkpointed or committed WAL at the second.
The comparison build changes Original to Updated; the preserved build checks
Updated and commits distinct update/delete/insert changes to Roundtrip. The
comparison build then verifies the snapshot and writable reopen, followed by
another preserved-build read. Held snapshots retain their previous expected
state. Independent oracles check typed values/identities, applicable graph and
index answers, catalogs, feature masks and physical codecs.

All2508 recorded cycle commands exited0. Each fixture has exactly the four
boundary combinations; all reports record `protected_unchanged:true` and
`fixture_generation_invoked:false` for cycle execution. Generation is a
separate explicit stage. Original fixture bytes and executable identities are
checked, rather than trusting a successful process exit alone.

The lifecycle fixture also verifies the corrected raw vector locator: physical
slot3 includes the internal `__e4_key` slot preceding user fields. server's
lifecycle-r1 expectedslot2 incorrectly; its failure remains preserved in
[the original evidence](phase2-evidence/lifecycle-r1/). The engine was unchanged.
The Pi's complete lifecycle corpus passes the corrected expectation, including
BUILDING, partially DROPPING and retained feature bits after drop.

## Provenance and limits

The host is `contributor@example.invalid`, Linuxaarch64, using its previously provisioned
Rust1.96.0 toolchain. Builds use offline vendored dependencies, a pinned472-file
current source inventory and159 preserved earlier-engine source files. The
current runtime includes the **unaccepted scalar-membership query experiment**.
Default and retained feature binaries are distinct and their exact hashes and
version records match the report. Actual earlier five-family ARM admission
probes are compiled from the preserved earlier source, not a current engine
with a reduced advertised mask. Admission checks are not full earlier-engine
semantic rollback cycles.

This is correctness evidence only. The Pi runs existing services; its times
must not be compared with server's timed benchmark. CPU quota is enforced at one
CPU, with one Cargo job and low priority. The Pi kernel lacks a memory cgroup
controller: the configured systemd MemoryMax does not enforce a cap. A separate
0.25-second sampled RSS/available-memory guard protects this task, but is not a
hard memory limit or a whole-run peak measurement.

The separate query integration suites subsequently passed: 8 multimodel and
3 scalar tests per build, default and retained, for 22 passing tests and no
failures. The main run ended with `run.exit=0` and all 11 stages passing. See
[native query evidence](PHASE2_PI_QUERY_RESULTS.md). These tests are separate
from the 416-cycle count and do not establish a query speedup.
server's full1M benchmark and queued compatibility runs remain separate evidence.
Eight [pure provenance regression tests](../tools/test_phase2_rollback_provenance.py)
pass and guard against mixed revisions, criss-cross build labels, missing cycle
capability and mismatched profiles. They do not replace native database tests.

## Preserved results and future use

[Raw cycle/fixture reports, logs and source/binary identities](phase2-evidence/pi-cycles-r1/)
are stored locally; `DOWNLOAD.json` pins the downloaded archive and reports.
The original run remains under:

`<scratch>`

Family directories are `{graph,multimodel,lifecycle}-{fixtures,cycles}`;
originals are in each fixture directory's `corpus/`; binaries are in `bin/`.
A portable archive of the actual original corpora, ARM executables, current and
earlier source snapshots, offline dependencies and reports is now preserved at:

`<scratch>`

The archive is 55,689,410 bytes; SHA-256:
`5f2554a6e1a57aafaebd97c39e3bef06a15b6b82be6503f956b6150f69f5891d`.
All 2,029 member hashes, sizes and modes were verified after download. Adjacent
`ARCHIVE.json` records relative paths and restoration instructions; the receipt
is also preserved with [query evidence](phase2-evidence/pi-query-r1/DOWNLOAD.json).
Original fixture and source files were checked against their qualification
inventories before and after packaging. Generated mutation copies and Cargo
build targets are excluded. The archive retains the unaccepted query candidate;
portable preservation does not freeze or accept that runtime.

A Linux restoration/replay check also passes from a distinct directory: all
2,029 files restore correctly and all 416 cycles pass using the relocated
executables and original corpora, without generating fixtures. Every restored
input and the source archive remain unchanged. These repeated cycles validate
archive portability, not additional format coverage; see
[restoration evidence](phase2-evidence/pi-restore-r1/RESTORE_REPORT.json). Earlier archives of helpers
without the third-state command cannot substitute for this rollback-capable
baseline.

The [test-group commands](PHASE2_TEST_GROUPS.md) distinguish same-revision
cross-build preparation from future cross-revision checks. The latter require
actual preserved binaries and independently pinned comparison build/source
provenance. The original frozen Phase1 compatibility gate remains mandatory.
