# sekejap-core

The engine layer of [sekejap](https://github.com/sekejapdb/sekejap): typed
collections over the storage kernel, graph edges as a native keyspace, and the
vector, spatial and full-text index families.

Rows are typed positional records, never JSON text on disk. JSON is a wire
format at the API boundary only.

This is an internal layer. Most users want the [`sekejap`](https://crates.io/crates/sekejap)
crate, which is the whole database in one dependency.

License: MIT OR Apache-2.0.
