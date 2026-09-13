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

## Proposed query spelling

The policy above is accepted. These examples describe the intended simple
interface; they are not claims of an implemented SQL parser. The Rust collection API
below is implemented.

```sql
-- Default: no automatic timestamps
CREATE COLLECTION sensor_readings;

-- Explicit opt-in: both automatic timestamps
CREATE COLLECTION people WITH (timestamps = ON);

-- Explicitly state the default
CREATE COLLECTION sensor_archive WITH (timestamps = OFF);
```

Creation-only mode was discussed as an extension. It is not required to
deliver the accepted off-by-default / easy-opt-in policy.

## Reason and measured cost

Optional metadata lets collections choose their storage cost. In the verified
[10M people benchmark](PEOPLE_10M.md), adding the two fields cost 84,307,968
bytes: 8.43 bytes/person, or 4.70% over plain E4. That percentage depends on
the workload; smaller sensor records may pay proportionally more.

## Implementation follow-through

Implemented by the internal Rust collection API in the typed collection loop:

```rust
CollectionOptions::default()                 // OFF
CollectionOptions { timestamps: false }      // explicit OFF
CollectionOptions { timestamps: true }       // explicit ON
```

The policy persists across reopen and applies to replacement and patch writes.
Clock units are Unix seconds; creation time is preserved and update time never
moves backward. No-op writes count as writes. Callers cannot supply managed
values when enabled; there is no restore override or policy toggle yet. Off
permits ordinary user timestamp values. Tests cover these behaviors, including
an injected backward clock. See [the complete collection contract](COLLECTIONS.md).
The SQL spelling above remains proposed. This is still a product/schema policy,
not an eighth law.
