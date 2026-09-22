# Optional collection timestamps

Accepted by the user on 2026-09-10 (Australia/Melbourne).
This is an E4 product/schema design decision, not an additional law.
The existing seven laws are unchanged.

## Accepted policy

- Automatic timestamps are **off by default** for new collections.
- A simple, explicit option at collection creation enables automatic
  `_created_unix` and `_updated_unix` fields, stored as typed integers.
- Persist the policy with the collection schema so every write path follows
  the same setting; callers need not supply managed fields on each insert.
- With the option off, the engine does not add or maintain automatic timestamp
  fields or reserve their per-row storage slots.
- Explicit application time fields, such as an IoT measurement's `observed_at`,
  remain normal data regardless of the automatic-timestamp option.

This supersedes the earlier suggestion to turn timestamps on by default.

## Proposed query spelling — NOT accepted by the parser

The policy above is accepted; this SPELLING is not built. `CREATE COLLECTION`
and a `WITH (timestamps = ...)` clause are not in the grammar
(`lang/src/parser/ddl.rs`: `CREATE` takes `TABLE` or `INDEX`), so the block
below is a sketch of an interface and is tagged `text` rather than `sql` — the
documentation harness runs every `sql` block, and a block it cannot run is not
allowed to look like one it can.

```text
-- Default: no automatic timestamps
CREATE COLLECTION sensor_readings;

-- Explicit opt-in: both automatic timestamps
CREATE COLLECTION people WITH (timestamps = ON);

-- Explicitly state the default
CREATE COLLECTION sensor_archive WITH (timestamps = OFF);
```

Creation-only mode was discussed as an extension. It is not required to
deliver the accepted off-by-default / easy-opt-in policy.

## What SQL does spell today

`CREATE TABLE` always takes the default, which is OFF. What a statement CAN
declare is an ordinary application time column: a declared `TIMESTAMPTZ` or
`DATE` is stored as UTC microseconds in a `Kind::Int` (`docs/lang/QL_CONTRACT.md`
§5 deviation 8) and prints back as the ISO string it was written as. A
`DEFAULT now()` fills it when the INSERT does not name it, which is one clock
read per row.

```sql
CREATE TABLE sensor_log (
    sensor TEXT,
    celsius REAL,
    observed_at TIMESTAMPTZ DEFAULT now(),
    on_day DATE
);
INSERT INTO sensor_log (_key, sensor, celsius, on_day)
    VALUES ('s1-0001', 's1', 21.5, '2026-01-04');
CREATE INDEX sensor_log_day ON sensor_log USING btree (on_day);
CREATE INDEX sensor_log_at ON sensor_log USING btree (observed_at);
SELECT _key, sensor, on_day FROM sensor_log WHERE on_day = '2026-01-04';
SELECT _key FROM sensor_log WHERE EXTRACT(YEAR FROM observed_at) >= 2026
```

The automatic `_created_unix` / `_updated_unix` pair is the engine's, not a
column a statement writes, and the `readings` collection of
[the example fixture](lang/EXAMPLE_FIXTURE.md) is the one that has it on.

## Reason and measured cost

Optional metadata lets collections choose their storage cost. In the verified
[10M people benchmark](PEOPLE_10M.md), adding the two fields cost 84,307,968
bytes: 8.43 bytes/person, or 4.70% over plain E4. That percentage depends on
the workload; smaller sensor records may pay proportionally more.

## Implementation follow-through

Implemented by the `CollectionOptions` field the typed collection loop takes:
`CollectionOptions::default()` and `CollectionOptions { timestamps: false }`
are OFF, `CollectionOptions { timestamps: true }` is ON. Both halves as a
program — the body of a function returning `Result<(), sekejap::Error>`, run
by `dist/rust/tests/doc_examples.rs`:

<!-- doc_example: timestamps_options -->
```rust
use sekejap::core::collections::CollectionOptions;
use sekejap::{Db, FieldKind};

let dir = std::env::temp_dir().join("sekejap-timestamps-example");
let _ = std::fs::remove_dir_all(&dir);
let db = Db::open(&dir)?;

// OFF: `Db::create_collection` and `CREATE TABLE` both take the default.
db.create_collection("sensor_readings", &[("celsius", FieldKind::Real)])?;
assert!(!db.describe("sensor_readings")?.expect("declared").timestamps);

// ON: the option is a `CollectionOptions` field, and there is no Tier-1
// SQL spelling for it, so it is set through the engine handle a
// transaction lends out.
let mut tx = db.transaction()?;
tx.database().create_collection(
    "people",
    vec![("name".to_owned(), FieldKind::Text)],
    CollectionOptions { timestamps: true },
)?;
tx.commit()?;
db.invalidate_plans();
assert!(db.describe("people")?.expect("declared").timestamps);
Ok(())
```

The policy persists across reopen and applies to replacement and patch writes.
Clock units are Unix seconds; creation time is preserved and update time never
moves backward. No-op writes count as writes. Callers cannot supply managed
values when enabled; there is no restore override or policy toggle yet. Off
permits ordinary user timestamp values. Tests cover these behaviors, including
an injected backward clock. See [the complete collection contract](COLLECTIONS.md).
The SQL spelling above remains proposed. This is still a product/schema policy,
not an eighth law.
