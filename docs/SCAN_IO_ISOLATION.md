# Shuffled-scan investigation: buffered I/O isolation — 2026-09-14

**A standalone C program reproduces stale buffered reads on the tested Mac /
scratch stack, without E4 or Rust.** The identical C source passes on the Pi
and server server. This changes the next action: requalify the frozen packing
candidate on the native target platforms before attributing the Mac failures
to its algorithm. That subsequent [Linux qualification](NATIVE_REQUALIFICATION.md)
now retains the packing change for density. Foundation promotion and the
original CONTROL-SCAN investigation remain open.

This is correctness evidence, not a new performance or disk-density benchmark.
The [previous E4/candidate/SQLite measurements](PAIR_PACKING.md) remain separate.
The seven laws and opt-in timestamp policy are unchanged.

## Independent C control

`tools/buffered_rewrite_probe.c` uses four processes with separate files. Each
file has 16,384 pages of 4 KiB (64 MiB). A worker first writes a known pattern
to every page, then performs 200,000 deterministic random page replacements.
After each successful positional write, it overwrites its reusable source
buffer and checks both the newly written page and another previously written
page. Expected bytes derive independently from worker, page, version and word
number. Every one of the 512 eight-byte words is compared. Short calls and
EINTR are handled; filenames are created exclusively in a fresh directory.

There is **no E4, B-tree, Rust, CRC dependency, F_NOCACHE, or O_DIRECT** in this
program. It tests read-after-write visibility within running processes, not
crash durability. Each worker stops at its first mismatch and preserves the
expected and observed 4 KiB buffers plus the original data file.

| Platform / run | Outcome |
|---|---|
| Mac / scratch, first run | Two workers return stale pages; two complete |
| Mac / scratch, second run | One worker returns a stale page; three complete |
| Pi, `203.0.113.10` | All four complete: 1,600,000 checked reads |
| server server, isolated Job | All four complete: 1,600,000 checked reads |

All three Mac failures occur while revisiting a previously written page,
after that replacement had already passed its immediate read check. All 512
words match the previous version of the same worker's same page:

| Mac run / worker | Step | Page | Expected version | Observed version |
|---|---:|---:|---:|---:|
| First / 3 | 19,262 | 514 | 1 | 0 |
| First / 1 | 26,305 | 2,984 | 2 | 1 |
| Second / 0 | 17,020 | 12,364 | 1 | 0 |

Later independent reads of the closed files return the expected page in the
first case and the stale page in the other two. These are observations at
that later read, not guarantees about permanent physical-media contents.
The three buffers, full-file evidence and SHA-256 hashes are preserved.

The tested Mac volume is `/dev/disk5s1`, APFS, mounted at `/Volumes/scratch`.
The experiment establishes an I/O failure on this tested stack; it does not
identify macOS versus APFS versus USB/device as the responsible component.
Ordinary development load can affect timings. Returning an older successful
write is a separate correctness failure and must not be dismissed as timing
noise. The earlier [F_NOCACHE failure](RECOVERY_R1.md) was a different probe;
disabling F_NOCACHE does not prevent the buffered failure demonstrated here.

## Connecting the observation to E4

The original V3 800K fixture was inspected without `Store::open`, replay or
checkpoint. `tools/audit_shuffled_pages.rs` opens the file read-only, checks
page CRC/identity/slot bounds, independently decodes this fixed fixture and
checks key membership against the inverse of its multiplicative generator.
It also checks within-page order and immediate parent/child intervals.

The physical file has 800,000 leaf records, seven duplicated keys, seven
expected keys absent, one within-page ordering violation at page 39,769 and
one immediate interval violation at child 26,984 / parent 456. All page CRCs
pass. The baseline physical control has none of these defects. This is a
**physical inventory**, not an assertion that all seven missing physical keys
were missing from the live committed Store: that test left dirty cache pages
and committed WAL, and the original live scan measured count and ordering.
Original data/WAL hashes still match the previous loop's preservation manifest.

An isolated copy of frozen V3 then adds two forensic checks:

- Each released write guard verifies that its leaf/interior keys are ordered.
- Each positional read compares its page digest with the most recent
  successful positional write at that offset in the same file object.

Six serial Mac runs pass, as does another run of the original uninstrumented
binary. Four concurrent independent Stores produce **three read/write digest
mismatches**, at offsets 107,143,168; 71,798,784; and 15,400,960. The other
Store passes. The planned second concurrent batch is skipped after failure.
No generated-page ordering assertion fires first in these three cases.
Their partial data/WAL fixtures remain preserved; they are failed runs, not
successful committed-state recovery tests.

Together with the independent C reproducer, these observations establish a
real external I/O failure class capable of invalidating Store reads. They do
**not** prove the exact causal history of the older 800K fixture or the earlier
400K stale-page cases. Nor do they prove every packing policy correct. Those
claims require native qualification and, where needed, further tracing.

## Pi diagnostic and allocator correction

The same instrumented source is compiled natively on the Pi with
`sqlite-balance,compact-cells,test-support`. Each run tests separate 200K and
800K shuffled Stores with 200-byte values and the original 16 MiB pool budget.
All three runs under a **128 MiB address-space cap** pass ordered scans, counts,
write-guard checks and the remembered-write I/O checks.

The first attempt with glibc's default arena policy became extremely slow.
The process remained live and was making I/O progress; it was not restarted
because of a polling timeout. Its virtual-memory peak reached 131,072 KiB.
A GDB sample placed the worker in `mmap64` called from `malloc` and the test's
counting allocator. This attempt was deliberately terminated (exit 143), with
its partial fixtures, process status and I/O counters retained.

Using **the same binary and cap**, with `MALLOC_ARENA_MAX=1`, the complete
200K/800K pairs finish in 36.51, 38.59 and 34.01 seconds. This is evidence of
allocator overhead under the cap, not an E4/SQLite speed comparison. Future
capped diagnostic test commands must record this allocator setting.

The remembered-write map deliberately retains one digest per written page.
It is forensic instrumentation, **not production code and not a bounded-memory
Law-1 proof**, even though these particular test assertions pass. The program
is single-writer per file; its digest check is not a general concurrent FileIo
implementation. `tools/scan_trace_instrumentation.patch` records the exact
changes to the frozen source, including the diagnostic root-number output.
Apply this zero-context diagnostic patch to that frozen source with
`git apply --unidiff-zero`; it is not a patch for the retained baseline.

## Reproduction and retained scope

Build the independent control with:

```sh
cc -O2 -Wall -Wextra tools/buffered_rewrite_probe.c -o /tmp/e4-buffered-rewrite-probe
/tmp/e4-buffered-rewrite-probe <scratch>
```

The directory must not already exist. The probe preserves failed files and
does not clean anything automatically. Run on an explicitly authorized host
directory when comparing the Pi or server. Compiler/source/binary hashes and
raw logs are in [results](SCAN_IO_RESULTS.json). The native scripts and server
Job manifest are checked in beside the probe.

Artifacts:

- Mac: `<scratch>`
- Pi: `<scratch>`
- server: `<scratch>`

Failed original/new fixtures and C failure files have a verified archive,
with a second copy on server. Successful redundant databases are removed only
after logs and file hashes are recorded; see [cleanup](SCAN_IO_CLEANUP.json).
One Mac instrumented control, one uninstrumented control, one Pi control and
the deliberately interrupted Pi fixture remain available.
Cleanup reclaimed **4,865,855,488 allocated bytes (4.87 GB)** across the three
hosts. The server backup independently verifies all 25 archived files,
representing 1,469,709,036 logical bytes of failure evidence.

No engine code, disk format, query interface, multimodel index or law changes
in this commit. Do not waive native correctness gates because the Mac stack
is faulty. Re-evaluate the significant frozen V3 gains with full native
Store/collection and pager gates, keeping baseline E4 and SQLite explicit.
