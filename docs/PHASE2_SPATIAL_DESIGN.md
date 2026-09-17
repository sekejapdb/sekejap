# Phase 2 point spatial index — bounded design candidate

This is an implemented Phase 2 candidate, not a released or frozen format.
Existing entity records remain authoritative and their point coordinates stay
exact `f64` WGS84 `[longitude, latitude]` degrees.

Family3/encoding1 requires feature bit `0x08`; the bit is monotone after explicit
creation. The `E4IDX01` descriptor payload is:

```text
index_id:u64be | collection_id:u32be | family:u8=3 | encoding:u16be=1 |
grid_bits:u8=16 | CRS:u8=1 | metric:u8=1 | options:u8=0 |
state:u8 | cursor:u64be |
name_len:u16be | name:utf8 | field_len:u16be | field:utf8
```

CRS1 is WGS84 longitude/latitude degrees and metric1 is the Karney/GeographicLib
WGS84 inverse. State and cursor use the common catalog encoding: BUILDING0 with
last scanned entity sequence, READY1/zero, DROPPING2/zero. The field must be a
non-unique declared `Kind::Point`. Unknown versions, constants, options or state
combinations are refused.

The point posting is:

```text
key   74 | ordered(index_id) | hilbert:u32be | ordered(entity_sequence)
value longitude:f64le | latitude:f64le                         (16 bytes)
```

Hilbert values use exactly 16 bits per axis. Posting coordinates duplicate the
exact primary coordinates and must not narrow them to `f32`. The common catalog
holds collection and field identity, so the posting key does not repeat them.

## Semantics

Indexed points reject non-finite longitude, latitude, or radius values;
longitude is in `[-180, 180]`, latitude in `[-90, 90]`, and radius is
non-negative metres. Rectangle membership is inclusive. `west > east` means
the rectangle crosses the dateline. `-180` and `180` are aliases for exact
membership, while the stored coordinate retains the spelling supplied by the
entity.

Distance and radius acceptance use `geographiclib-rs` 0.2.7's WGS84 inverse
geodesic, not the older bounded-iteration Vincenty helper. Radius membership is
exactly `distance <= radius`. The GeographicLib geodesic formulation is
documented at <https://geographiclib.sourceforge.io/html/python/geodesics.html>;
the pinned crate's README documents its `Geodesic::wgs84()` and
`InverseGeodesic` API.

Hilbert ranges are only candidate filters. The persisted level is 16 bits per
axis. Rectangle covers split at the dateline. A non-wrapping rectangle touching
one spelling of 180 degrees also scans the opposite edge cell because primary
coordinates preserve `-180` versus `180`. Overlapping ranges are merged so an
entity is not returned twice. The helper returns at most 64 inclusive ranges;
if split covers exceed that fixed budget it uses the conservative whole-world
range. This can increase reads but cannot omit a hit.

A radius candidate envelope uses the global lower bound on the WGS84 metric.
The minimum meridional curvature radius is
`M_min = a(1-e^2) = 6335439.3272928195 m`, and both meridional and
prime-vertical curvature radii are at least `M_min`. Thus an ellipsoidal path
between two geodetic coordinates is at least `M_min` times their spherical
central angle. Every exact hit is inside the spherical cap with angular radius
`radius / M_min`. The cap's latitude and longitude extrema, plus an outward
`1e-10` degree candidate-only margin, form a conservative box. A cap reaching a
pole scans all longitudes; an angular radius of pi or more scans the world.
Exact GeographicLib refinement decides the final result.

## Bounded APIs and costs

A bbox query scans the bounded Hilbert ranges, checks exact stored coordinates,
and returns at most the requested number of smallest matching stable IDs. A
radius query uses the conservative envelope, exact geodesic acceptance and the
same stable-ID limit. Both count every examined posting or filtered candidate;
exhausting the caller's work bound returns an error rather than a partial
successful page.

Exact nearest-neighbour search with `All` scans the complete finite point index
and keeps a bounded `(distance, EntityId)` top-k, ordered by distance then stable
identity. Filtered mode probes only caller-supplied sorted unique entity IDs and
verifies their primary point and expected posting. Both expose posting-work and
result bounds; exhausting a bound is an error, never a partial exact answer.
Approximate nearest search would require a separate named mode, versioned
algorithm, effort control, and recall evidence.

Each point update writes one posting plus descriptor/build maintenance. Queries
pay for every posting in the candidate ranges, exact `f64` checks, dateline
deduplication, and a GeographicLib inverse calculation per radius or nearest
candidate. World fallback is deliberately expensive but bounded by the caller's
posting-work cap. Index-only damage is not silently repaired; rebuild requires
verified surviving primary rows and explicit lifecycle state.

Candidate Linux behavior, corruption, admission, write-fault and compatibility
evidence is recorded in `PHASE2_VECTOR_SPATIAL_RESULTS.md`,
`PHASE2_MULTIMODEL_COMPAT_RESULTS.md`, `PHASE2_REBUILD_RESULTS.md` and
`PHASE2_WORKSPACE_RESULTS.md`. This evidence does not freeze or release the
format. Preserved ARM fixtures now cover READY spatial catalogs and
graph-independent mask 9 through BUILDING with a nonzero cursor, DROPPING and
post-drop retained features. Same-copy cross-build cycles and archive restoration
pass; see [rollback results](PHASE2_ROLLBACK_RESULTS.md). These prepare the first
candidate baseline; they are not historical released-version compatibility.
