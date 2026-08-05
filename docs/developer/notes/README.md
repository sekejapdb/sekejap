# Design notes (archive)

History and rationale — why things look the way they do, including designs
that were explored and rejected. These are **not reference docs**; the current
truth lives one level up and in the code.

- [requirements.md](requirements.md) — the vision: what sekejap must be, the
  50 GB-on-1 GB target, the phased storage roadmap.
- [skbin-format.md](skbin-format.md) — the SKBIN payload wire format, full
  spec.
- [topology-format.md](topology-format.md) — the dense-id topology file
  format, full spec.
- [durability-benchmarks.md](durability-benchmarks.md) — fsync levels
  measured, cross-engine.
- [serve-design.md](serve-design.md) — the full HTTP-server design, including
  phases not yet built.
- [search-view-design.md](search-view-design.md) — how materialized search
  views got their shape (and what was dropped).
- [edge-model.md](edge-model.md) — the cross-domain thought experiment behind
  the typed-edge model.
