# Phase 1 qualification evidence

Primary result: ../PHASE1_QUALIFICATION.md. Runtime source and tests are
identified by source-r2-manifest.json; the Linux runner and Job definition
are preserved here. Original five fixture databases live in
../format-v1-fixtures/ and must never be regenerated in place.

FORMAT_REVIEW.md and COMPAT_REVIEW.md are independent working reviews.
Their pending-test statements reflect when those reviews were written;
the final qualification report and raw Linux logs determine the final result.

The Linux Job reuses only compiled artifacts from the earlier cache. It
extracts a checksummed fresh source archive and recompiles changed files.
Negative tests use explicitly preserved earlier source, then restore the
fixed source before positive tests. No failed stage is silently skipped.

Counts in the final report take the last summary of each top-level Cargo
target, avoiding duplicate child-process summaries. Repeated feature modes
are test executions, not different logical tests. Existing ignored tests
are listed explicitly.

Final result: PHASE1_R2_EXIT=0; default422/0/2, compact427/0/2,
retained427/0/2 (pass/fail/ignored). Evidence archive SHA256:
`93b1d2ad381a5db8d657939e0445a1f2603a31390974fb4a80d66665d9b79e73`.
The 2.08MB archive includes exact tested source, original/final raw logs and
source manifest. Its local digest matches the Linux export.
