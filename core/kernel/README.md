# sekejap-kernel

The storage kernel of [sekejap](https://github.com/sekejapdb/sekejap): a
disk-first B-tree over a page write-ahead log, with typed packed pages, a
persistent free list and crash recovery.

This crate is the bottom layer. It knows pages, keys and bytes; it does not
know collections, queries or indexes. Most users want the [`sekejap`](https://crates.io/crates/sekejap)
crate instead, which is the whole database in one dependency.

Disk format: **sekejap disk format v2**. Page header bytes 18 and 19 carry the
format version, and every sekejap from 0.17.0 reads and writes v2.

License: MIT OR Apache-2.0.
