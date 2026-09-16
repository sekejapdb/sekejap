# Frozen baseline evidence

See ../FORMAT_BASELINE.md for scope, outcomes and future commands.

SHA256SUMS covers every retained artifact here except itself. The source archive
contains the clean59d1cbc checkout including actual shallow Git history; ignore
Mac archive metadata and normalize ownership on Linux extraction. The binary
archive retains actual tested executables and the new immutable corpus.

reference-evidence.tar.gz contains initial20arms, three expanded7-test suites,
and recovery CLI evidence. final-driver-evidence.tar.gz contains the final
path-hardened Python runner and repeated20passing arms. The engine and Rust
harness are unchanged between those passes. harness-manifest.json describes
initial harness sources; PROVENANCE.json also records final driver/test hashes.

The path-regression red/green logs establish that the three pure filesystem
controls reject the previous runner and pass the final runner. They are not
additional engine corruption cases. Raw logs remain verbatim.

These are cross-build tests of one declared engine revision. Future engine
versions must reuse the retained baseline binary and corpus; never rebuild or
replace the old artifacts as a substitute for compatibility.
