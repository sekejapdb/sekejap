# Sekejap Internals

Engine internals for contributors and deep integrators. For how to *use*
sekejap, see [`../guide/`](../guide/README.md).

## Contents

- [architecture.md](architecture.md) — the three pillars (fast startup,
  disk-first memory, lightspeed queries), execution paths, index-maintenance
  rules, storage layout, and regression patterns. **The invariants to preserve
  before changing anything.**
- [requirements.md](requirements.md) — what sekejap must be: vision,
  non-negotiables, the 50 GB-on-1 GB scaling target, and the phased storage
  roadmap.
- [durability.md](durability.md) — WAL formats, fsync/sync levels, and the
  durability semantics behind write performance (honest cross-engine benchmarks).
- [payload-binary-format.md](payload-binary-format.md) — SKBIN, the sealed
  on-disk payload format (schema-aware binary, per-record CRC, recoverability-first).
- [topology-format-v2.md](topology-format-v2.md) — the Phase 0 dense-id,
  offset-addressable topology format spec (the path to mmap-paged billions).
- [edge-design-thought-experiment.md](edge-design-thought-experiment.md) —
  cross-domain reasoning behind the typed-edge model.
