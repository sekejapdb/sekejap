# Payload Binary Format (SKBIN) — Design

Status: **proposal** (proven in `examples/schema_encode_bench.rs`; not yet in the engine)
Chosen: 2026-07-26 — **Level 1 / metadata-only**. This is the design we ship first.

## 1. Summary

Replace raw-JSON payload storage (and the opt-in per-record zstd) with a
self-describing **per-record binary encoding**. Field *names* become integer IDs
from a tiny shared table; values are typed (varint ints, f64, packed bool/null)
but strings stay **literal in the record**. Nothing is deduplicated across
records. Measured on 200k realistic records, roundtrip-verified zero loss:

```
raw                 80.7 MB  1.00x
per-record zstd      60.6 MB  1.33x
SKBIN (Level 1)      50.4 MB  1.60x   <- chosen
full-record read: 1846 ns  (10% FASTER than JSON parse)
single-field read:  351 ns  (6x FASTER than parse-then-get)
```

A strict win over raw: smaller, faster, and **identical 1-record corruption
isolation**. zstd is removed from the payload path (worse ratio, no faster; its
only higher-ratio variants require cross-record sharing = corruption spread).

## 2. The one inviolable rule (why Level 1)

**No byte of user data may ever live in shared state; corruption blast radius = 1
record.** Every *value* lives in exactly one record. The only shared artifact is
the field-**name** table — structural metadata, the same class as the offset
index (`nodes.bin`) that raw storage already depends on.

This forbids: cross-record dictionaries, value interning, and per-field string
templates — all of which move value bytes into a shared table (Levels 2/3,
**rejected**; they reach ~2.9× but at the cost of the rule). See
`.workbench/payload-compression-verdict.md`.

## 3. Record format (SKBIN v1)

First byte discriminates format so raw / zstd-legacy / SKBIN coexist with zero
migration (`{`=0x7B raw JSON; `0x01`=legacy zstd; `0x02`=SKBIN).

SKBIN body = one object. Values are tag-prefixed:

```
tag 0  null
tag 1  false
tag 2  true
tag 3  int     -> zigzag varint            (numbers stop being decimal text)
tag 4  float   -> 8 bytes IEEE-754 LE
tag 6  string  -> varint len + utf8 bytes  (ALWAYS literal — value lives here)
tag 7  array   -> varint count + values
tag 8  object  -> varint count + [varint field_id, value]*
```

Deliberately **no interned-string or templated-string tags** — those would put
value data in shared state. Records **> 64 KB stay raw** (preserves the head/tail
extraction fast path for huge GeoJSON). Each stored record is **CRC32-framed**;
a CRC mismatch errors that one record only.

## 4. The shared table (field names only)

`fields: Vec<String>` — `field_id -> name`, every key ever seen. That's it.

- **Tiny + bounded:** sized by *distinct field names*, not record count. ~150 B
  for these records; plateaus (200k records and 200M records → same table).
- **Append-only IDs:** an ID is never reused or rewritten in place; the table
  only grows, so no torn write can corrupt an existing mapping.
- **Redundant + checksummed:** stored ≥3 copies, CRC per copy; a bad copy is
  detected and a good one used.
- **Rebuildable:** field names also live in `CREATE TABLE` schemas and in
  un-compacted WAL `Put` payloads.

## 5. Recoverability analysis

Two domains, kept separate by construction:

**Record bytes (all user data).** Independently encoded, CRC-framed. A corrupt
byte fails that record's CRC → that one record errors; all others intact. Blast
radius = 1 record, identical to raw. ✅

**The field-name table (structural metadata only).** It holds *names*, never
*values*. Even total loss of it costs column **labels**, not data — you still
have every record's values (like a CSV that lost its header), recoverable from
`CREATE TABLE` or by position. Protected as above (redundant + CRC + rebuildable).

This is the same class of shared metadata raw already has (lose `nodes.bin` and
you can't locate a single raw record either). SKBIN adds no new class of risk —
only a tiny, guarded, name-only table — while cutting size 1.6× and speeding
reads. Contrast the rejected zstd dictionary: large, opaque, unrebuildable, and
holding actual value data.

## 6. Read / write / compaction

- **Read:** dispatch on first byte. Full record → `dec_record`. Single field →
  `get_field` **skip-scan** (jump over other fields by length; 6× faster than
  parse-then-get) — a net query-engine win beyond size.
- **Write (`put`):** parse once, append any new field names (append-only ID +
  journal), encode, CRC-frame, append.
- **Compaction:** `compact()` already streams a full rewrite; it writes the
  field table (redundant) first, then re-encodes live records as SKBIN.
- **WAL:** `Put` stays raw JSON — recovery before compaction never needs the
  table; the in-RAM table rebuilds deterministically from replayed payloads.
- **Crash rule:** the field table is durable before any SKBIN record that
  references a new ID.

## 7. Edge cases

- Unknown/new fields: append an ID on first sight — schemaless by construction.
- Type drift (int here, string there): fine — each value self-types by tag.
- Numbers not exactly i64/f64: fall back to literal string (tag 6) for exact
  roundtrip. Property-test this.
- Non-object payloads (rare): encode as a single value, or keep raw.

## 8. Phased build plan

1. SKBIN codec module + property-tested roundtrip (zero loss) + CRC framing.
2. Field table: build / append / persist (redundant + CRC) / rebuild-from-scan /
   rebuild-from-WAL, with tests.
3. Read path: `decode_payload_record` handles `0x02`; `get_field` skip-scan;
   mixed-file tests (raw + zstd + SKBIN coexisting).
4. Write at compaction: image-then-records; reopen (resident + paged) equivalence.
5. Crash tests: kill between table write and payload rename; assert recoverable.
6. Query wiring: route single-field reads through `get_field`.
7. Delete the zstd payload path once SKBIN is default (keep the `0x01` reader for
   migration until a compaction rewrites the last zstd record).

Each phase ships green with tests before the next.

## 9. Future: leaner WITHOUT breaking the rule (not yet measured)

Level 1 is 1.6×. More is possible *only* via techniques that keep per-record
isolation and put **no user data in shared state** — i.e. **universal codecs in
the CODE** (not per-DB value tables):

- Value-type codecs (self-tagged per value): timestamp → epoch varint, zero-padded
  numeric string → (width, int), hex/UUID → packed bytes. Each value stays in its
  record; only the *decoding logic* is shared (it's code, in every binary).
- Schema-positional layout: presence bitmap + values in field order, dropping
  per-field IDs (references only the field-*name* metadata already shared).

Expected ~2–2.2× (the irreducible unique content — keys, emails, free text —
can't shrink without cross-record sharing). Prototype + measure before adopting;
Level 1 is the floor we never drop below.

## 10. Rejected alternatives

`.workbench/payload-compression-verdict.md`: global trained dict (whole-DB loss),
fixed/generic dict (worse than none), segment+dict (16k-record blast radius),
value interning + templates (value data in shared state). All violate the rule.
