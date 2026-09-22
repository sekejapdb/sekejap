# sekejap-lang

The query language of [sekejap](https://github.com/sekejapdb/sekejap): SQL
parsed and compiled to the engine's atomics, with prepared statements, `$n`
parameters, and `pg_catalog` and `information_schema` as views over the
catalog rows.

What the language accepts, what it refuses and why is stated in
`docs/lang/QL_CONTRACT.md` in the repository.

This is an internal layer. Most users want the [`sekejap`](https://crates.io/crates/sekejap)
crate, which is the whole database in one dependency.

License: MIT OR Apache-2.0.
