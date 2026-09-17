# Phase 2 multimodel candidate compatibility

Linux qualification completed 2026-09-17, exit0. This is candidate evidence,
not a released index-format freeze or complete Phase 2 acceptance.

| Check | Result |
|---|---:|
| Preserved candidate sources | 16 |
| Current default/retained read, update, original-build readback | 32 passed |
| Earlier-engine snapshot/writer admission | 96 passed |
| Source-overlap, symlink and hardlink refusal guards | 48 passed |

The four profiles are scalar+graph plus vector, spatial, text, or all three.
Required logical masks are respectively7,11,19,31. Each current build generates
a checkpointed and pending-WAL source for each profile. The other build checks
its exact documents, stable identities, index metadata, scalar/graph queries,
vector metrics/ties, spatial membership, and fixed BM25/Unicode scores. It
updates a separate copy, checks an old held snapshot, and hands the copy back
to the original build at both checkpointed and pending-WAL boundaries.
Physical and logical features remain unchanged. Every preserved source's file
inventory and bytes remain unchanged.

Earlier engine admission uses a small harness added to copies of preserved
source trees; the engine itself is unchanged. All59 tracked engine/Cargo files
of the Phase1 source match frozen commit59d1cbc770284f160ffda53cc1ee545167733d11.
The only source added to each older tree is `typed_admission_probe.rs`.

| Earlier engine | Snapshot + writer checks | Expected result |
|---|---:|---|
| Phase1 | 32 | All newer profiles refused |
| Scalar/graph candidate | 32 | All newer profiles refused |
| Exact-vector candidate | 8 | Vector profile admitted |
| Exact-vector candidate | 24 | Spatial/text/all profiles refused |

A refusal counts only for the typed `Unsupported` error (exit42), with no file
or inventory changes. Generic corruption or I/O errors would fail the test.
Accepted writer probes use disposable copies; originals remain untouched.
These are admission checks, not evidence that the older vector binary executed
every current query or mutation. The32 semantic handoffs are between current
build configurations, not between separately released index versions.

Default creates unpacked cells; retained enables all four existing packing
features. Both builds decode and update either physical feature setting without
promotion. The job uses the isolated server Linux environment, 2CPU/2GiB limits,
release builds, locked offline dependencies and source inventories captured
before compilation. Source mtimes were refreshed to prevent stale Cargo reuse;
source contents were not changed by that refresh.

Raw reports, commands, logs and source provenance:
[phase2-evidence/multimodel-fixtures-r1](phase2-evidence/multimodel-fixtures-r1/).
The16 candidate databases and five hashed binaries remain preserved on server
under `<scratch>/` and
`multimodel-fixtures-r1-binaries/`. Future qualification must keep these sources
and executables rather than regenerating them with the version under test.

Still required: qualification of the final mixed-query/recovery implementation,
process-kill schedules, verified derived rebuild, full workload measurements,
and a final acceptance audit. No Phase2 release or commit is claimed here.
