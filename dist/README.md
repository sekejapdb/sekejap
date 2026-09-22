# sekejap-dist

The distribution layer of [sekejap](https://github.com/sekejapdb/sekejap): the
service wrapper (one writer, snapshot readers), the PostgreSQL wire server, and
the `sekejap` command line.

The wire server speaks the PostgreSQL frontend/backend protocol, so `psql` and
tools that speak it can connect. What it answers and what it refuses is stated
in `docs/dist/WIRE_CONTRACT.md` in the repository.

Embedding sekejap in a Rust program needs the [`sekejap`](https://crates.io/crates/sekejap)
crate instead.

License: MIT OR Apache-2.0.
