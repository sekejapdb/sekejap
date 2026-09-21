# Operations contract — sekejap-e4 Phase 3 (draft for owner edit)

Drafted 2026-09-20. Companion to `docs/lang/QL_CONTRACT.md` (what the language is)
and `docs/core/GRAPH_CONTRACT.md` (what the graph is). This document fixes the
third surface, the one neither of those states: what the database IS while it
is running — who writes, what a reader sees and how stale it may be, how a
statement is bounded in wall clock, how a caller stops one, how a listener
learns that something committed, and what a tool may ask about size and cost.

The reason it exists: `docs/lang/E3_PARITY.md` counts 33 e3 capabilities e4's
contracts do not mention at any tier, and the largest single cluster is this
one. e3 ships a long-running service form with publish semantics, a statement
timeout, a cancellation handle and a commit-time change feed. e4 has every
ingredient — `open_snapshot`, `Error::Cancelled`, the `WorkMeter` cancellation
closure, `io_counters`, `pool_counters`, `checkpoint` — and no contract that
says what they add up to. That is a missing document, not a missing tier row.

Tiers are `docs/lang/QL_CONTRACT.md`'s: **T1** compiles to an atomic that exists at
HEAD `bdbef43`; **T2** is a Phase-3 item with the atomic named; **T3** is
refused with the reason in the error text and never emulated.

Laws are `CONTRACT.md` / `docs/core/FOUNDATION_TEST_STANDARD.md`: L1 disk-first
(bounded RAM), L2 work proportional to change, L3 nothing fallible may delete,
L4 name costs, L5 recoverability, L6 readers, L7 target usability, L8 release
compatibility.

## 0. The table, in one place

| # | Surface | e3 (file:line) | e4 ingredient | Tier | Law |
|---|---|---|---|---|---|
| 1 | Service mode: one writer, snapshot readers | `src/service.rs:1-70`, `:174-186` | `Database::open_snapshot` (`src/collections/mod.rs:716`) | **DONE** — `dist/src/service/mod.rs:142` (`ServiceDatabase`), `:173` (`open`), `:209` (`writer`), `:220` (`try_writer`), `:249` (`reader`), `:259` (`open_reader`), `:504` (`close`), `dist/src/service/snapshot.rs:43` | L6 |
| 2 | `publish()` and the staleness window | `src/service.rs:118-151`, `:153-156` | `commit` publishes; `open_snapshot` reads the newest published transaction | **DONE** — `dist/src/service/mod.rs:275` (`publish_now`), `:282`/`:289` (`publish_interval`), `:666` (`mark_dirty_and_maybe_publish`), `dist/src/service/snapshot.rs:36` (`PUBLISH_INTERVAL_DEFAULT`) | L6 |
| 3 | Statement timeout (wall clock) | `src/db.rs:5004-5012`, `:5030-5046` | `WorkMeter::charge` / `check_cancelled` (`src/query/mod.rs:528`, `:581`) | **DONE** — `core/engine/src/query/mod.rs:528` (`QueryBudget::deadline`), `:558` (`with_deadline`), `:647` (`WorkResource::Deadline`), `:717` (`DEADLINE_POLL_CHARGES`), `:778` (`check_deadline`); `dist/src/service/mod.rs:341` (`set_statement_timeout`) | L1, L4 |
| 4 | Interrupt handle (public cancel) | `src/db.rs:325-337`, `:5014-5023` | `Error::Cancelled`, `QueryError::Cancelled` (`src/query/mod.rs:471`), `traverse_bfs_with_cancel` (`src/index/graph/mod.rs:1635`) | **DONE** — `dist/src/service/interrupt.rs:26` (`InterruptHandle`), `dist/src/service/mod.rs:377` (`interrupt_handle`), `:383` (`cancel`), `:388` (`clear_interrupt`), `:428` (`scan`, which threads it into every `next_page`) | L6 |
| 5 | Change notifications, one event per committed batch | `src/db.rs:373-386`, `:4980-5001`, `:5049-5080` | `commit` (`src/collections/mod.rs:1605`) as the single emission point | **DONE** — `dist/src/service/changes.rs:70` (`ChangeEvent`), `:46` (`CHANGE_QUEUE_BOUND` = 256), `:51` (`CHANGE_KEY_CAP` = 1,024), `:205` (`deliver`); `dist/src/service/mod.rs:629` (`WriterGuard::commit`, the emission point) | L6, **L1** |
| 6 | Introspection: `SHOW STATUS`, `SHOW STORAGE`, `stats`, `memory_report`, `trim_memory` | `src/sql.rs:8132-8135`, `src/exec.rs:463-540`, `src/db.rs:10177`, `:11726`, `:11763` | `storage_bytes` (`src/collections/mod.rs:794`), `io_counters` (`:947`), `pool_counters` (`:892`), `tracked_pages` (`:798`) | T2 | L4 |
| 7 | Bulk load: `begin_bulk` / `put_value_bulk` / `link_many` | `src/db.rs:11913-11997` | `commit` as the durability point; `put`, `put_edge` | T2 | L2, L3 |
| 8 | `write_trace`: per-phase commit timing | `src/write_trace.rs:1-22` | `io_counters` (`:947`), `pool_counters` (`:892`) | T2 | L4 |

§1-§5 are BUILT, in `dist/src/service/` over one additive change in core;
§6-§8 are still T2. The three named T3 exceptions inside §1, §3 and §5 stand,
and none of them is emulated.

**What §1-§5 cost to build, in one place.** One field
(`QueryBudget::deadline`), one enum variant (`WorkResource::Deadline`), one
constant (`DEADLINE_POLL_CHARGES` = 1,024 charges) and one private method
(`WorkMeter::check_deadline`) in `core/engine/src/query/mod.rs`; everything
else is composition in `dist/src/service/`, which is where the layering says
an operator surface lives. The tests are `dist/tests/service.rs`.

**The deviations from this document's wording, both stated where they are
made.** §1 says the published view is `RwLock<Arc<Database>>`; it is
`RwLock<Arc<Snapshot>>` with `Snapshot` owning the handle behind its own
mutex, because `Database` carries per-handle caches (`RefCell`, `Cell`) and is
therefore `Send` but not `Sync`, so `Arc<Database>` cannot cross a thread
(`dist/src/service/snapshot.rs:1-28`). §2 says the writer pays the mint so
readers never do; with no background thread in this build, a reader that finds
a view dirty and older than the interval pays it instead, and the writer still
pays it whenever another commit follows within the interval
(`dist/src/service/mod.rs:239-252`).

## 1. Service mode — one writer, snapshot readers

**What e3 does.** `CoreDB::open_as_service(dir)` (`src/service.rs:174-186`)
returns a `ServiceDb`: one process, a `Mutex<Db>` writer, and an
`RwLock<Arc<Db>>` published read view. Writes serialise on the writer lock;
reads clone the `Arc` under a momentary read lock and then run entirely
lock-free on a pinned read-only handle, so a slow write, a bulk load or a
compaction never stalls a query (`src/service.rs:8-14`). The type is
`Send + Sync` by construction and is meant to be shared behind an `Arc`
rather than wrapped in the caller's own lock (`src/service.rs:36-39`,
`:191-194`). `open()` stays the embedded form that starts and stops with an
app; this is the form a server, an agent or a gateway keeps alive.

**e4's ingredient.** `Database::open_snapshot(path, config)`
(`src/collections/mod.rs:716`) is exactly the read handle: a read-only view of
the newest published transaction, beside a writer in this or another process,
byte-stable for its life, holding one reader slot which *defers* rather than
blocks the writer's checkpoint. `docs/core/ARCHITECTURE.md` §1.3 already states the
single-writer rule. What is missing is the object that owns both halves.

**The atomic to build.** A `ServiceDatabase` that owns one `Database` writer
behind a mutex and one `Arc<Database>` snapshot behind an `RwLock`, with
`snapshot()` an `Arc` clone, `query` / `get` on the snapshot, and `execute` /
`put` on the writer. It lives beside `Database` (`src/collections/mod.rs`) or
in its own `src/service.rs`; `docs/core/SOURCE_LAYOUT.md` gains the row.

**The Law it must satisfy.** **L6.** A reader opened before a commit does not
see it; a reader opened after does. No read takes the writer's lock, and no
reader blocks the writer — the reader slot defers a checkpoint and nothing
else. e4's slot count is persisted and bounded (`readers` in `ResourceLimits`,
`docs/core/ARCHITECTURE.md` §2), so the service must refuse to mint a snapshot past
that bound rather than block: **a service that blocks a reader to honour a
reader bound has broken L6 to satisfy L1**, and the refusal is the correct
shape.

**BUILT.** `dist/src/service/mod.rs:142` (`ServiceDatabase`), `:173`
(`open`), `:209` (`writer`), `:220` (`try_writer`, the named in-process
refusal), `:249` (`reader`), `:259` (`open_reader`), `:504` (`close`), and
`dist/src/service/snapshot.rs:43` (`Snapshot`). Tests:
`dist/tests/service.rs` --
`a_reader_keeps_its_old_snapshot_while_a_writer_commits_and_a_later_reader_sees_the_commit`,
`a_second_writer_is_refused_by_name_while_the_first_holds_it_and_served_once_it_is_dropped`,
`a_second_service_on_one_directory_is_refused_by_the_stores_own_writer_lock`
(the T3, refused by the page WAL's own lock as `Kernel(WriterLocked)`),
`close_discards_uncommitted_work_and_releases_the_reader_slot`.

**The deviation, and why.** The published view is `RwLock<Arc<Snapshot>>`, not
`RwLock<Arc<Database>>`: `Database` owns four interior-mutability caches and
the page-WAL store two more, so it is `Send` but not `Sync` and `Arc<Database>`
cannot cross a thread. `Snapshot` owns the handle behind its own mutex. The
publication mechanism and the Law are unchanged -- no read takes the writer's
lock, and no reader blocks the writer -- and the cost this buys is stated at
`dist/src/service/snapshot.rs:19-28`: two readers sharing the published handle
take turns, and a caller that wants two walks at once calls `open_reader` and
spends one more persisted `readers` slot.

**The T3 inside it.** More than one writer process on one directory is T3, and
the reason is not the service: the page WAL is single-writer, and the file
lock is what says so. The contract states this here so a service caller reads
it in the same place as the rest.

## 2. `publish()` and the staleness window

**What e3 does.** After a commit the writer republishes at most once per
`publish_interval` (default 100 ms, `src/service.rs:186`), paying the mint
cost itself so readers never do, and only when a commit actually happened —
`dirty` is set by a commit and cleared by a publish (`src/service.rs:136-151`).
A read is therefore stale by at most one interval plus one mint. For
read-your-own-writes, `ServiceDb::publish()` mints synchronously
(`src/service.rs:118-133`). A failed mint leaves readers on the view they
already had, which is Law 3's shape at the API: the old photo is dropped only
once the new one is in hand (`src/service.rs:143-146`). e3's own note records
the predecessor engine shipping this default-on and undocumented, with 53% of
same-thread write-then-read cycles missing the write at a 5 ms interval — the
contract here is the same trade, written down.

**e4's ingredient, and where it is cheaper.** e3's `publish` must
`publish_generation()` — commit **and checkpoint** — before minting, because
an e3 snapshot opens the newest published *generation* and commits alone still
live in the WAL (`src/service.rs:120-122`). e4 does not have that constraint:
`commit` publishes, and `open_snapshot` reads the newest published transaction
through the committed-WAL overlay (`docs/core/ARCHITECTURE.md` §1.3,
`src/collections/mod.rs:710-716`). **e4's publish barrier is therefore one
checkpoint cheaper than e3's**: mint a fresh snapshot handle, swap it in, done.
Checkpointing stays what it is — a separate, deferrable fold — and is not on
the read-your-own-writes path.

**The atomic to build.** `ServiceDatabase::publish()`: open a fresh snapshot,
swap the `Arc`, stamp the publish instant, clear the dirty flag. Plus the
rate-limited writer-side `maybe_publish` after each successful commit. A
failed open leaves the previous `Arc` in place and is reported, never silently
retried into a gap.

**The window this contract states.** A read is stale by at most
`publish_interval` plus one snapshot open. The default interval is **100 ms**,
matching e3 so a migrating caller's timing does not change under it. No
measurement is claimed here: e3's mint costs (~12 ms at 1M rows, ~56 ms at
48M) are e3's numbers on e3's path, and e4's snapshot open has not been
measured for this document. The first item of work is to measure it and put
the metric in this section.

**The Law.** **L6**, in its visible form: the window is the contract's answer
to "when does a new reader see the latest acknowledged commit". L3 governs the
failure mode — a failed mint never destroys the served view.

**BUILT.** `dist/src/service/mod.rs:275` (`publish_now`), `:282` / `:289`
(`publish_interval`), `:666` (`mark_dirty_and_maybe_publish`, the writer-side
rate limit), `dist/src/service/snapshot.rs:36`
(`PUBLISH_INTERVAL_DEFAULT` = 100 ms).

**The metric this section asked for, measured.** On a 4,000-row corpus,
Apple M-series laptop, release build, `IoMode::Buffered` / `SyncMode::Full`,
four runs: one snapshot open is **0.9 ms to 6.7 ms**, and one `publish_now`
swap is **0.9 ms to 2.0 ms** -- that open plus an `Arc` store under a write
lock, and nothing else. A commit made just after a publication became visible
to a new reader **100.3 ms to 108.5 ms** later, measured from the commit,
against a stated window of 100 ms + one open. e3's mint costs on e3's path
were ~12 ms at 1M rows and ~56 ms at 48M; e4's is one checkpoint cheaper by
construction, and these are the first e4 numbers for it. The open cost grows
with the corpus and these are 4,000-row numbers: what is fixed is the SHAPE of
the window, not its second term.
Test: `a_commit_becomes_visible_within_the_publish_interval_plus_one_snapshot_open`,
`publish_now_swaps_the_view_in_one_snapshot_open_and_leaves_the_old_one_intact`.

**The second deviation.** e3's writer pays the mint so a reader never does.
This build starts no background thread, so a reader that finds the view dirty
and older than the interval pays it; the writer still pays it whenever another
commit follows within the interval. Without that, a database whose last commit
is its last write would serve a view that never refreshes.

## 3. Statement timeout

**What e3 does.** `Db::set_statement_timeout(Option<Duration>)`
(`src/db.rs:5004-5012`) bounds one statement, not the handle's lifetime.
A `StatementGuard` (`src/db.rs:339-368`) installs a deadline on the current
thread at statement entry and removes it on drop, including on an early return
or a panic, so a timeout never leaks into the next statement on that thread.
The deadline is tested inside `should_abort` (`src/db.rs:5030-5046`), the same
per-row check that reads the cancel flag: the flag is one relaxed atomic load
every call, the **clock is polled only every 1024 rows**, so the common
no-timeout path costs a comparison. The abort reason is recorded as
`"statement timeout exceeded"` and read once at the boundary.
`ServiceDb::set_statement_timeout` (`src/service.rs:158-166`) applies it to
every snapshot the service mints and republishes immediately so it takes
effect.

**e4's ingredient.** `WorkMeter::charge` (`src/query/mod.rs:581`) already calls
`check_cancelled` (`:528`) before charging any resource, and every walk charges
— candidates, primary reads, postings per family, graph edges and visited
nodes, key postings, output bytes. **The check points already exist and are
already at the right density**: one per unit of work, on every driver.

**The atomic to build.** A `deadline: Option<Instant>` beside the
cancellation closure in `WorkMeter`, tested in `check_cancelled` on a counted
interval of charges (e3's 1024 is the precedent; the interval is a stated
constant, not a tuning knob), returning the existing `QueryError::Cancelled`
with a reason that distinguishes a timeout from a cancel. Its setter is on the
handle and on `ServiceDatabase`; `SET [LOCAL] statement_timeout = '5s'` rides
the `SET LOCAL` path that already exists (`lang/src/compile.rs:814`).

**The Law.** **L1 and L4.** The timeout is a wall-clock bound *in addition to*
`QueryBudget`, never instead of it. The contract's bound on work stays the
work bound — that is what makes cost predictable and reproducible; a clock
bound is not reproducible and cannot be the guarantee. The timeout is the
service's safety valve for a query whose budget is generous and whose machine
is loaded. L4: the cost of having it is one comparison per charge on the
no-timeout path, and that number belongs in this paragraph once measured.

**The T3 inside it.** A timeout that interrupts a *write* mid-commit is T3.
Cancellation points are on the read path; a commit is the L3 barrier and is
not interruptible by a clock. A long write is bounded by batching it (§7), not
by a timer.

**BUILT.** The one change in core: `core/engine/src/query/mod.rs:528`
(`QueryBudget::deadline: Option<Instant>`), `:558`
(`QueryBudget::with_deadline`, so no caller restates the work bounds it
already chose), `:647` (`WorkResource::Deadline`), `:717`
(`DEADLINE_POLL_CHARGES` = **1,024 charges**, e3's precedent), `:762` /
`:778` (`check_cancelled` -> `check_deadline`). The setter is
`dist/src/service/mod.rs:341`, and `:361` / `:367` are how it reaches a
statement.

**The stated interval, and what it means.** The clock is read once per meter
-- so once per page, which is what lets a paged scan detect a deadline that
passed between two small pages -- and then once per 1,024 charges. A deadline
is therefore detected within 1,024 units of work of passing, not at the
instant it passes.

**The L4 number this paragraph owed.** On the no-deadline path the cost is one
`Option` discriminant test per charge and **no clock read at all**:
`QueryBudget::unlimited()` carries `deadline: None`, `WorkMeter::new` does not
call `Instant::now` when the field is `None`, and `check_deadline` returns on
the first `let ... else`. What is measured rather than argued is that the
bound changes no work: a 40,000-row scan charges 40,008 candidates and 40,009
primary reads, and charges exactly the same after a timeout has been set,
fired and cleared. No before/after binary comparison of the per-charge test
was made, and this paragraph does not claim one: a single predictable branch
per charge is below the noise of a 27-34 ms scan on this machine.

**The refusal's two numbers.** `limit` is the microseconds the PAGE was
allowed and `attempted` the microseconds it had spent when the clock was read;
both are measured from the page's own start, because the budget is supplied
per page while the deadline is one absolute instant for the statement. A
timeout is `QueryError::BudgetExceeded { resource: Deadline, .. }` and a
cancel is `QueryError::Cancelled`: different errors on purpose, so a retry
loop can tell them apart. Test:
`a_statement_past_its_deadline_is_refused_naming_deadline_the_elapsed_micros_and_the_work_so_far`
(across four runs the refusal reported limit 78-4,665 us against elapsed
106-11,001 us, with the work counters holding the completed pages' charges --
the spread is the point: `limit` is what the PAGE that noticed was allowed,
and a page that began close to the deadline was allowed very little),
`a_work_bound_still_refuses_by_its_own_resource_while_a_deadline_is_also_set`.

## 4. Interrupt handle — public cancel

**What e3 does.** `InterruptHandle` (`src/db.rs:325-337`) is a cloneable
`Arc<AtomicBool>`; `Db::interrupt_handle()` (`src/db.rs:5014-5018`) hands one
out, `cancel()` sets it, `is_cancelled()` reads it, and
`Db::clear_interrupt()` (`src/db.rs:5021-5023`) clears it. The shape is
`sqlite3_interrupt`'s. The semantics e3 states and e4 should keep: cancellation
is **per handle and therefore per snapshot** — cancelling stops every query in
flight on that view, which for a shared read snapshot is all of its readers; a
timeout is per statement and needs no clearing, while an explicit cancel is
**sticky** until cleared. The running query stops at its next check point and
returns an error naming the cancellation.

**e4's ingredient.** More of this exists than anywhere else in this document.
`QueryError::Cancelled` (`src/query/mod.rs:471`) and `Error::Cancelled` are
already the error; `WorkMeter` already carries a `cancelled: &mut C` closure
threaded through every charge (`src/query/mod.rs:516-531`); every index family
already returns `Cancelled` from its walk (`src/index/text/mod.rs:1717`,
`src/index/spatial/point.rs:655`, `src/index/vector/exact.rs:294`,
`src/index/vector/quantized.rs:322`); and `traverse_bfs_with_cancel`
(`src/index/graph/mod.rs:1635`) is the graph half, with `traverse_bfs` its
never-cancelled wrapper. What is missing is only the public object and the
wiring: today the closure is supplied per call by the caller, so there is
nothing a second thread can hold.

**The atomic to build.** A public `InterruptHandle(Arc<AtomicBool>)` on
`Database` and on `ServiceDatabase`, whose load becomes the default
cancellation closure every prepared query is given. One relaxed atomic load
per charge, the same cost e3 pays.

**The Law.** **L6.** Cancelling a reader must not touch the writer, must not
damage the snapshot, and must leave the handle answering normally after
`clear_interrupt`. A cancelled query returns an error; it never returns a
partial answer labelled as complete — that is the same rule §6 of
`docs/lang/QL_CONTRACT.md` makes for a budget refusal.

**BUILT.** `dist/src/service/interrupt.rs:26` (`InterruptHandle`), `:35`
(`cancel`), `:41` (`is_cancelled`), `:47` (`clear`);
`dist/src/service/mod.rs:377` / `:383` / `:388`, and `:428` (`scan`), which
hands the handle's load to `prepare_sql_with` and to every `next_page` it
issues. Test:
`a_cancel_from_another_thread_stops_a_long_scan_and_clearing_it_restores_the_reader`
-- which also proves the sticky half (the next statement is refused too) and
that a cleared handle answers in full -- and
`a_standing_cancel_does_not_stop_the_writer_from_committing` for L6's other
half.

## 5. Change notifications — one event per committed batch

**What e3 does.** `Db::subscribe_changes(callback) -> u64` and
`unsubscribe_changes(id) -> bool` (`src/db.rs:4980-5001`). A `ChangeEvent`
(`src/db.rs:373-386`) carries three lists: the **collections** whose members
moved, the **node keys** (`collection/key`) that were put, updated or removed,
and the **edge type** labels that were linked or unlinked. The delivery rule
is exact and is the whole value of the primitive: **one event per committed
mutation batch** — a single autocommit statement fires once, a transaction
fires once at COMMIT with the union of everything it touched, and a rolled-back
transaction fires **not at all** (`src/db.rs:5066-5080`: emission is suppressed
while `commit_depth > 0` or a transaction is open). Listeners run on the thread
that commits, so they must be quick and do real work by waking another task.
Recording is free when nobody is listening — every `record_*` returns
immediately on an empty listener list (`src/db.rs:5049-5062`). e3 names this
what it is: the primitive a reactive `.watch()` is built on. A listener learns
*which* collections, keys and edge types moved, then re-runs whatever query it
cares about.

**e4's ingredient.** `Database::commit` (`src/collections/mod.rs:1605`) is the
single point every mutation passes through to become durable, and it is
already the only place a batch ends. Collections and edge types are already
named objects in the catalog and the graph header.

**The atomic to build.** A pending-event accumulator on `Database`, appended
by `put` / `delete` / `put_edge` / `delete_edge` behind an empty-listener
guard, drained and delivered inside `commit` after the barrier succeeds and
never before it, and dropped whole by `rollback`.

**The Law — and the correction e4 must make.** **L6** for the feed itself: an
event means a new published generation exists, which is precisely the signal a
snapshot reader needs in order to decide to re-open.

**L1 for its bound, and this is where e4 must not copy e3.** e3's
`ChangeEvent.keys` is a `Vec<String>` that accumulates every changed key in
the batch, deduplicated only against the immediately preceding entry
(`push_unique`, `src/db.rs:396-400`). A bulk load of ten million rows under one
commit therefore holds ten million key strings in RAM before a listener sees
any of them — memory proportional to the batch, inside the engine, which is a
Law 1 violation that e3's own bulk-load path (§7) makes reachable in one call.
e4's contract bounds it: **the collections and edge-type lists are carried in
full** (both are bounded by the catalog, which is bounded by the schema), and
**the key list is carried up to a stated cap, after which the event reports
`keys_truncated` with the total count and the key list is dropped**. A
listener past the cap does what a listener is supposed to do anyway — re-run
its query against the collections it was told about. The cap is a constant
fixed at open, in the same family as the plan cache's three ceilings.

**The T3 inside it.** A durable, replayable change log — a listener that
subscribes after the fact and catches up — is T3. That is a second write path
with its own retention, its own truncation and its own recovery story, and it
has no atomic here. The feed this contract states is in-process, live, and
lossless only for a listener that was already subscribed.

**BUILT.** `dist/src/service/changes.rs:70` (`ChangeEvent`), `:46`
(`CHANGE_QUEUE_BOUND` = **256 events per subscriber**), `:51`
(`CHANGE_KEY_CAP` = **1,024 keys per event**), `:205` (`deliver`, which
`try_send`s and counts a drop rather than waiting);
`dist/src/service/mod.rs:629` (`WriterGuard::commit`) is the single emission
point, after `Database::commit` returned `Ok` and while the writer lock is
still held.

**The ordering, proved without a crash.** By the time a subscriber holds the
event, a snapshot opened right then already holds the batch: the signal cannot
run ahead of the durable state. Test:
`exactly_one_event_per_committed_batch_and_the_event_never_precedes_durability`.
The rest: `a_rolled_back_batch_and_a_dropped_guard_fire_no_event_and_leave_no_rows`
(a guard dropped without committing rolls back and emits nothing),
`a_subscriber_that_stops_draining_drops_events_past_the_stated_bound_and_counts_them`
(264 commits against a 256-event queue: 8 dropped, 8 counted in `lagged`, and
the queue held the OLDEST 256 -- a full queue drops the new event, not the
held one), `the_key_list_stops_at_the_stated_cap_and_the_event_reports_the_total`,
`unsubscribing_stops_the_feed_and_an_unlistened_commit_records_nothing`.

**What this feed cannot name.** A SQL write through `WriterGuard::sql` is
counted in `ChangeEvent::unnamed_writes` rather than attributed to a
collection: `lang`'s compiled `WritePlan`, the only thing that knows a
statement's target, is `pub(crate)` to `sekejap-lang`, and widening it is a
lang change this surface does not make. A listener that sees it above zero
re-runs its query, exactly as one past the key cap does. `BEGIN`, `COMMIT` and
`ROLLBACK` are refused through the guard
(`dist/src/service/mod.rs:122`, `:603`): lang compiles `COMMIT` straight to
`Database::commit`, which would cross the barrier behind the feed's back, and
this section's whole claim is that the event and the barrier are one point.

## 6. Introspection

### 6.1 `SHOW STATUS`

**What e3 does.** `src/exec.rs:519-540` returns one `(name, value)` row per
fact: format version, generation, mode (`snapshot` or `embedded`), sync mode,
collection count, node count, edge count, data bytes, WAL bytes, and a query
counter.

**e4's ingredient.** Format version and the create-feature bits live in
`src/store/pagewal/format.rs`; generation and sync mode are page-WAL state;
mode is whether the handle came from `open` or `open_snapshot`; bytes come
from `storage_bytes` (`src/collections/mod.rs:794`), which is exact and O(1)
and already documents that the 96-byte publication hint is excluded;
`tracked_pages` (`:798`) is the distinct-page debt a `tracked_pages` policy
bounds; `io_counters` (`:947`) supplies frames, fsyncs and bytes.

**The atomic to build.** One catalog view (`db_status`) over those reads, and
the `SHOW STATUS` sugar over it (`docs/lang/QL_CONTRACT.md` §2).

**Stated cost.** Everything above is O(1). The **node and edge counts are
not**: a count without a filter is a scan, which `docs/lang/QL_CONTRACT.md` §6
already labels as such. They are therefore optional columns, absent unless
asked for, and `EXPLAIN` says scan when they are.

**Law: L4.** This is where a caller reads what the database is costing.

**Tier: T2.**

### 6.2 `SHOW STORAGE`

**What e3 does.** `src/exec.rs:463-518`: one full store scan attributing live
key + value bytes and row counts to each keyspace tag — catalog, node
payloads, membership, edges, reverse edges, vectors, external keys, properties,
vector codes, text postings, text metadata, spatial grid, geometry, vector nav
graph, SQL metadata, field indexes, search slots — then the `data` and `wal`
file sizes. e3's own comment prices it honestly: *an administrative command,
priced like one*.

**e4's ingredient.** The same shape: e4's keyspaces are tag-prefixed
(`docs/core/ARCHITECTURE.md` §1.2), so one ordered walk attributes every byte, and
the file sizes are `storage_bytes` at O(1).

**The atomic to build.** A tag-attributing walk behind `db_storage`, and the
`SHOW STORAGE` sugar.

**Stated cost.** It is a scan by definition and reports itself as one. It is
the one statement in either contract whose cost is proportional to the database
on purpose, and the contract says so rather than hiding it behind a cached
number that drifts.

**Law: L4.** **Tier: T2.**

### 6.3 `stats()`, `memory_report()`, `trim_memory()`

**What e3 does.** `stats()` (`src/db.rs:10177-10198`) is the struct
`SHOW STATUS` reads, with counts computed, not cached — e3's doc comment says
to call it for a report, not in a loop. `memory_report()`
(`src/db.rs:11763-11806`) returns per-structure resident bytes and is written
with unusual care: it names only structures that exist, refusing to report
absent structures as `0` because *"reporting them as 0 would name the wrong
structure with the authority of a measurement"*. The kernel buffer pool
appears as the arena it reserves at open, labelled a ceiling and not a
residency. `trim_memory()` (`src/db.rs:11726-11747`) shrinks the registries
and returns free arenas to the OS, and is explicitly safe: it drops no data
and no index, so every query answers the same after it as before.

**e4's ingredient.** `pool_counters` (`src/collections/mod.rs:892`) gives hits,
misses, evictions and clock-sweep steps, with the doc comment already making
the distinction a scan budget needs — a miss is a pread and a checksum, a hit
is a hash lookup. `tag_hint_stats` (`:940`) and `io_counters` (`:947`) are the
rest.

**The atomic to build.** `Database::memory_report()` over e4's actually
resident structures, and `trim_memory()` over the caches that can shrink
(`index_cache`, the catalog and layout caches).

**What makes e4's report short, and why that is the point.** e4 holds nothing
proportional to rows: rows are in the tree and are served through a
fixed-size buffer pool reserved at open. So the honest e4 report is the pool
arena (a ceiling, labelled), the index and layout caches (proportional to
collections and declared indexes), and the reader-slot state. **That short
list is Law 1 being true, stated as a measurement.** e3's refusal to report
absent structures as zero is adopted verbatim as a rule of this contract.

**Law: L4, evidencing L1.** **Tier: T2.**

## 7. Bulk load

**What e3 does.** `begin_bulk()` / `end_bulk()` (`src/db.rs:11913-11938`) open
a scope that defers the durability point to the matching close, so a stream of
writes costs one commit instead of one per write; scopes nest and only the
outermost close commits, and unbalanced calls are absorbed rather than
panicking because the call arrives from FFI. `put_value_bulk(rows)`
(`src/db.rs:11951-11977`) is the node form and `link_many(edges)`
(`src/db.rs:11987-11997`) the edge form, both nesting-safe so neither cuts an
outer batch short. e3 is precise about what is won: not a skipped fsync — e3
has no per-write fsync — but the batching of the durability point.

**e4's ingredient.** The same fact holds and is easier to state: e4's
durability point is `commit` (`src/collections/mod.rs:1605`), and `put`,
`delete`, `put_edge` and `delete_edge` already write into the working tree
without one. A bulk load in e4 is already "many puts, then one commit"; what
is missing is the named scope that makes it an API and a statement.

**The atomic to build.** A nesting-counted write scope on `Database`
(`begin_batch` / `end_batch`) whose outermost close calls `commit`, plus the
two batched entry points. `COPY ... FROM STDIN` on the wire (p3-wire) is the
same scope with a parser in front.

**The durability rule this contract makes.** The batch commits with **the same
durability as any other commit** — the same FULL barrier, the same publication.
A bulk-load mode that weakens the barrier is T3 and is not offered: L3 does not
have a fast path.

**Where e4 is stronger than e3, stated as a deviation.** e3's
`put_value_bulk` aborts at a malformed row "leaving the rows before it
stored". e4 has `rollback` (`src/collections/mod.rs:1628`), which discards the
uncommitted working tree in place exactly as a reopen would, so **a failed
batch leaves nothing committed** — the batch is all or nothing. Callers
migrating from e3 must expect the stronger guarantee, not the weaker one.

**Laws: L2** (work is the rows written, and one commit is one barrier, not N)
**and L3** (the failure path deletes nothing and publishes nothing). **L1** is
what §5 bounds: a large batch must not accumulate an unbounded change event.

**Tier: T1 — DONE.** `Database::begin_bulk` / `Database::end_bulk`
(`core/engine/src/collections/write_set.rs`), nesting-counted on the handle,
with the outermost `end_bulk` calling `Database::commit` and returning whether
it did. The SQL spelling is `BEGIN BULK` / `END BULK` (`docs/lang/QL_CONTRACT.md`
§2), chosen over `BEGIN`/`COMMIT` because the scope is not a transaction: the
single writer is already inside one, and what the scope moves is the
durability point.

Three deviations from e3, each stated rather than inherited:

* **No batched entry points.** e3 needs `put_value_bulk(rows)` and
  `link_many(edges)` because its scope is nesting-*unsafe* without them. e4's
  `put`, `delete`, `put_edge` and `delete_edge` already write into the working
  tree with no durability point of their own, so a loop of `put` inside a
  scope IS the batch; a second entry point would be the same calls behind a
  second name. `COPY ... FROM STDIN` on the wire (p3-wire) is this scope with
  a parser in front, and needs nothing new here.
* **An unbalanced `end_bulk` is an error.** e3 absorbs it because the call
  arrives across an FFI boundary where a panic is worse than a silence. This
  is not that boundary, and an `end_bulk` outside a scope would otherwise
  commit somebody else's uncommitted rows.
* **`rollback` clears the counter.** The scope's rows are gone and no caller
  is left to close it, so the depth goes back to zero with them. That is the
  §7 "a failed batch leaves nothing committed" guarantee, extended to the
  scope itself.

Tests: `core/engine/tests/write_where.rs`
`a_bulk_scope_nests_and_only_the_outermost_close_commits`,
`an_unbalanced_close_is_refused_and_a_rollback_forgets_the_scope`,
`a_bulk_scope_commits_with_the_same_durability_as_any_other_commit`;
`lang/tests/sql_dml.rs`
`begin_bulk_and_end_bulk_nest_and_only_the_outermost_close_commits`.

## 8. `write_trace` — per-phase commit timing

**What e3 does.** `src/write_trace.rs:1-22` is feature-gated end-to-end timing
for one transaction payload operation, broken into the phases that actually
cost: identity parse, prepare, reads of the old row, the primary put (itself a
nested kernel `PutTrace`), bookkeeping, the membership probe, the label write,
membership metadata, index maintenance, the collection catalog, page CRC (with
its count), and the number of caller-side value copies made before the kernel
sees the bytes. State is thread-local and the module is feature-gated, so the
timers cost nothing when off.

**e4's ingredient.** `io_counters` (`src/collections/mod.rs:947`) already
counts frames, fsyncs and bytes; `pool_counters` (`:892`) already separates a
pread-and-checksum miss from a hash-lookup hit; `tag_hint_stats` (`:940`)
already reports the per-keyspace append hints. e4 counts the I/O; it does not
yet attribute the time.

**The atomic to build.** A `cfg(feature = "write-trace")` thread-local phase
timer on the write path, with e4's own phase list: key mapping, row encode,
primary put, per-family index maintenance (one line per family, because that
is where a sacrifice shows), descriptor and header writes, and the commit
barrier itself.

**The Law: L4.** This is the instrument that makes "name your sacrifice"
falsifiable rather than asserted. `docs/core/ARCHITECTURE.md` §6 names the
sacrifices — the reverse mirror doubles edge storage, geometry writes up to
eight cell postings, a quantized index costs codes plus an f32 rerank. Those
are claims about *space*. `write_trace` is how the matching claims about
*time on the write path* get a number instead of an adjective.

**Tier: T2**, and last in the order of work: it is diagnostic, it is
feature-gated, and nothing else waits on it.

## 9. Wire implications (p3-wire)

The Postgres wire protocol has a first-class surface for three of the
capabilities above, and using it means a stock client gets them with no
sekejap-specific code. Each one is a reason the corresponding section must be
built the way it is stated.

### 9.1 `statement_timeout` as a GUC

`SET statement_timeout = '5s'`, `SET LOCAL statement_timeout = '5s'`, and the
same key in a connection's startup parameters. e4 already parses
`SET [LOCAL] <name> = <value>` and dispatches it by name
(`lang/src/compile.rs:814`, where `ef_search` and
`diskann.query_search_list_size` live), so this is one more name on a path
that exists. `SHOW statement_timeout` returns it. The value is Postgres's
interval-or-milliseconds spelling; `0` means no limit, which is §3's `None`.

### 9.2 `CancelRequest` as the interrupt handle

The protocol's cancellation is out of band: the server hands the client a
backend process id and a secret key at startup, and a cancel arrives on a
**second connection** as a `CancelRequest` carrying that pair. This maps onto
§4 with nothing left over — the pair identifies which `InterruptHandle` to
fire, and firing it is `cancel()`. It is why §4's handle must be reachable
from outside the thread running the statement, and why per-handle (not
per-statement) is the right granularity: the protocol has no statement id.

Both §3 and §4 must surface the **same** error code: Postgres `57014
query_canceled`. That is what `psql`'s Ctrl-C, JDBC `Statement.cancel()` and
every pooler already expect, and a different code turns a cancel into an
unexpected fault in client code nobody here wrote. The two are distinguished
by the message text, not the code.

### 9.3 `LISTEN` / `NOTIFY` as the change feed's surface

The change feed of §5 is `LISTEN` / `NOTIFY` seen from the other side, and the
match is unusually close:

- Postgres delivers notifications **at the end of the transaction**, not when
  `NOTIFY` runs, and a rolled-back transaction delivers nothing. That is §5's
  delivery rule word for word.
- Duplicate notifications with the same channel and payload in one transaction
  are collapsed into one — §5's `push_unique` by another name.
- A payload is capped at **8000 bytes**. This is an independent, external
  reason the key list must be bounded, arriving at the same answer as L1 in
  §5: collections and edge types (few, bounded by the catalog) fit; a key list
  from a bulk load does not, and must degrade to a truncation flag and a count.

So the surface is: `LISTEN <channel>` subscribes the session, each committed
batch emits one notification per listening channel with a payload naming the
collections and edge types that moved plus the key count, and `UNLISTEN` ends
it. A client-side `.watch()` is then `LISTEN` plus the caller's own re-query,
which is exactly the contract §5 makes.

### 9.4 What the wire does not get

Service mode (§1) and publish (§2) have no wire spelling and need none: a
connection is served by the process that opened the service, and the staleness
window is a property of that process, not of the protocol. `SHOW STATUS`,
`SHOW STORAGE` and the bulk-load scope arrive as ordinary statements
(`COPY ... FROM STDIN` for the last). `write_trace` (§8) is a build-time
feature and never appears on the wire.

## 10. Laws

| Law | What this contract owes it | Where |
|---|---|---|
| L1 Disk-first | The change feed's key list is capped and degrades to a count; the plan cache's three ceilings are fixed at open; `memory_report` is short because nothing here holds state proportional to rows. | §5, §6.3 |
| L2 Work ∝ change | A bulk batch is one barrier for N rows, not N barriers; the predicate-driven `UPDATE`/`DELETE` of `docs/lang/QL_CONTRACT.md` §2 walks matches, not collections. | §7 |
| L3 Nothing fallible may delete | A failed snapshot mint leaves the served view in place; a failed batch commits nothing; a statement timeout never interrupts a commit. | §2, §3, §7 |
| L4 Name costs | `SHOW STATUS`, `SHOW STORAGE` and `write_trace` are the instruments; each states what is O(1) and what is a scan. | §6, §8 |
| L5 Recoverability | Not this document's surface. Introspection reports state; it never repairs it, and no statement here is a recovery path. | — |
| L6 Readers | Service mode is L6 made visible at the API; the staleness window is its stated bound; the change feed is the signal that a new generation exists; a cancel touches no writer. | §1, §2, §4, §5 |
| L7 Target usability | The service form is the shape a Pi gateway runs; the bulk-load scope is the import path L7 gates on. | §1, §7 |
| L8 Release compatibility | Nothing here changes the disk format. The one persisted addition in the companion contract — per-field defaults, generated expressions and `NOT NULL` in the descriptor — is an additive feature bit; a file without it opens unchanged. | — |

## 11. Order of work

1. ~~§1 service mode and §2 publish~~ — **DONE**, and the snapshot-open cost
   is measured and in §2.
2. ~~§4 the interrupt handle~~ — **DONE**.
3. ~~§3 the statement timeout~~ — **DONE**, with the poll interval stated as
   1,024 charges and the no-deadline cost measured in §3.
4. ~~§5 the change feed~~ — **DONE**, with both of its L1 bounds decided and
   stated before a line was written (256 events per subscriber, 1,024 keys per
   event).
5. §6.1 `SHOW STATUS` and §6.3 `stats` / `memory_report` / `trim_memory`:
   O(1) reads over counters that exist.
6. §7 the bulk-load scope.
7. §6.2 `SHOW STORAGE`: a scan, so it waits for the ones that are not.
8. §8 `write_trace`, feature-gated, last.

p3-wire consumes §3, §4 and §5 in that order (§9).
