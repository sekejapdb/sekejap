# Payload Binary Format (SKBIN) — Design

Status: **SEALED / OFFICIAL** — implemented in `src/storage/skbin.rs` + `src/lib.rs`
+ `src/query.rs`; integrated across the full DML/DDL surface (resident + paged);
decoder fuzzed (panic-free; every single-bit corruption detected). Enabled via
`Config::payload_binary`. Format version **v1**, identified by first byte `0x02`.
Chosen: 2026-07-26 — **Level 1 / metadata-only**. See §8 for the versioning rule.

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
tag 5  uint    -> plain varint (u64 > i64::MAX)
tag 6  string  -> varint len + utf8 bytes  (ALWAYS literal — value lives here)
tag 7  array   -> varint count + values
tag 8  object  -> varint count + [varint field_id, value]*
tag 11 zstr    -> RETIRED per-field zstd string. zstd was removed from the payload
                  path (and the `zstd` dependency dropped). Never written; on decode
                  it is rejected loudly (record fails to decode) so a record from the
                  removed feature can never be mis-read.
```

Tag 10 is retired (was FSST — removed; it required a shared symbol table).
Deliberately **no interned-string or templated-string tags** — those would put
value data in shared state. Records **> 64 KB stay raw** (preserves the head/tail
extraction fast path for huge GeoJSON). Each stored record is **CRC32-framed**
`[0x02][crc32 LE u32][body]`; a CRC mismatch errors that one record only, and —
proven by fuzzing — the decoder never panics and never serves a corrupted value.

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

## 8. Versioning & format evolution (the sealed rule)

The **first byte is the format namespace** — this is how the format evolves safely
without a flag day:

```
0x7B  '{'   raw JSON            (legacy / >64 KB / un-compacted writes)
0x01        RETIRED whole-record zstd  (recognized only to reject loudly)
0x02        SKBIN v1            ← current official format
0x03..0xFF  RESERVED for future record formats (SKBIN v2, …)
```

Rules that make this safe:

1. **A new record format takes a new first byte.** SKBIN v2 would be `0x03`, never
   a silent change to the `0x02` body. Old and new records coexist in one file.
2. **Readers dispatch on the first byte and reject the unknown cleanly.** A record
   whose first byte a build doesn't recognise decodes to `None` (a *detected*
   "can't read this", never a panic or a misread) — and surfaces the friendly
   "run `sekejap migrate`" guidance (see the CLI toolkit).
3. **Shared frames carry an explicit version byte.** The field table is
   `["SKFT"][version][crc32][payload]`; the snapshot has `SNAPSHOT_FORMAT_VERSION`.
   A newer on-disk version than the binary supports is refused with a clear
   upgrade/migrate message rather than parsed optimistically.
4. **Forward path is compaction.** There is no destructive migration: `compact()`
   rewrites live records into the current format. `sekejap migrate <db>` wraps this
   with a verify-before-finalize pass (read every record back, assert byte-identical
   before swapping). Downgrade (SKBIN → raw) is `compact` with `payload_binary:false`.

So "change the format later" is a defined operation: bump to a new first-byte tag,
teach the reader to dispatch it, ship the migrate/verify path — never an in-place
reinterpretation of existing bytes.

## 9. Status: shipped & verified

All build phases are complete and green: codec + roundtrip, redundant CRC'd field
table (rebuildable from scan/WAL/`CREATE TABLE`), first-byte read dispatch, SKBIN
at compaction, resident **and** paged equivalence, the full DML/DDL surface
(projection/sort/GROUP BY/MATCH/filters/UPDATE/DELETE/ALTER/GIN/BM25), and a fuzzed
decoder (200k random inputs + every single-bit corruption + every truncation →
no panic, corruption always detected). zstd has been removed entirely from the
payload path (and the `zstd` dependency dropped); the `0x01` and tag-11 encodings
are recognized only to reject them loudly on decode, never to decompress.

## 10. Future: leaner WITHOUT breaking the rule (not yet measured)

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

## 11. Rejected alternatives

`.workbench/payload-compression-verdict.md`: global trained dict (whole-DB loss),
fixed/generic dict (worse than none), segment+dict (16k-record blast radius),
value interning + templates (value data in shared state). All violate the rule.
