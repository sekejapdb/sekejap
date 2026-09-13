# Page-WAL foundation branch

The baseline is preserved in commit `ee98182`. Branch `pagewal-foundation`
imports the measured safety-v2 candidate, previously isolated in
`/tmp/e4-pagewal-qualify`, for review and further correctness work.

Imported source matches the qualification archive SHA256
`5bb5e4648e8703e4b1122f39eda9606aad4ee910cf467866e70eeec87d6a5b4b`,
except `pagewal_cap.rs`, which includes the separately recorded short-reader
diagnostic. Existing reports and the failing control fixture test are retained.
Databases and executable artifacts remain outside Git.

Commit `d0b96ee` adds the stale-frame checksum guard and its deterministic
regressions. [The completed correctness loop](PAGEWAL_STALE_FRAME.md) records
363 distinct Mac tests, 30 selected Pi tests, and three rotated comparisons
per platform. Guarded E4 remains within the owner's ordinary raw-KV time/size
thresholds; the native old-Store stale-page cause is still unresolved.

This is a development checkpoint, not release promotion. The seven laws and
the current F1 blockers still apply. The page-WAL module remains separate from
the collection Store; importing it does not switch the public collection API.
