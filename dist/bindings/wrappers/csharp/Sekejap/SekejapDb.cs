using System;

namespace Sekejap
{
    /// <summary>
    /// An open sekejap 0.17.0 database: collections addressed by name plus
    /// key, JSON documents, SQL with <c>$n</c> parameters, rows as arrays of
    /// objects keyed by column, scans, prepared statements, transactions,
    /// links, the catalog and, in service mode, parallel readers and a
    /// change feed. Every result that crosses the C ABI is JSON text --
    /// decode it with <c>System.Text.Json</c>. Dispose closes the handle.
    /// </summary>
    public sealed class SekejapDb : IDisposable
    {
        private IntPtr _db;

        private SekejapDb(IntPtr db) => _db = db;

        // ── open / identity ──────────────────────────────────────────────────

        /// <summary><c>sekejap_open</c>: open (or create) the database directory at <paramref name="path"/>.</summary>
        public static SekejapDb Open(string path) => FromHandle(Native.sekejap_open(path), "open");

        /// <summary><c>sekejap_open_with_config</c>: the same, under a JSON
        /// store configuration -- <c>{"budget_bytes","io":"buffered"|"direct","sync":"full"|"normal"|"off"}</c>,
        /// every member optional. <paramref name="configJson"/> may be
        /// <c>null</c> for sekejap's defaults (<c>SyncMode.Full</c>).</summary>
        public static SekejapDb OpenWithConfig(string path, string? configJson) =>
            FromHandle(Native.sekejap_open_with_config(path, configJson), "open_with_config");

        /// <summary><c>sekejap_open_service</c>: SERVICE mode -- one writer,
        /// parallel readers on a published snapshot, the change feed, the
        /// statement timeout and the cancel (docs/dist/OPS_CONTRACT.md
        /// §1-§5). Only a handle opened this way answers
        /// <see cref="StatementTimeoutMs"/>, <see cref="Cancel"/>,
        /// <see cref="ClearInterrupt"/>, <see cref="Subscribe"/>,
        /// <see cref="NextChange"/> and <see cref="Unsubscribe"/> --
        /// every other handle gets <see cref="SekejapStatus.Refused"/>
        /// from them, by name.</summary>
        public static SekejapDb OpenService(string path) =>
            FromHandle(Native.sekejap_open_service(path), "open_service");

        /// <summary><c>sekejap_open_memory</c>: REFUSED. sekejap is
        /// disk-first and has no in-memory database -- a temporary directory
        /// would be a fake of an ephemeral store, so this does not make one.
        /// Always throws <see cref="SekejapException"/> with
        /// <see cref="SekejapStatus.Refused"/>; give <see cref="Open"/> a
        /// directory instead. Kept 1:1 so a caller porting from a
        /// memory-backed engine gets a named reason rather than a silent
        /// workaround.</summary>
        public static SekejapDb OpenMemory() => FromHandle(Native.sekejap_open_memory(), "open_memory");

        private static SekejapDb FromHandle(IntPtr db, string what)
        {
            if (db == IntPtr.Zero) throw Interop.Fail(what);
            return new SekejapDb(db);
        }

        /// <summary><c>sekejap_version</c>: <c>MAJOR.MINOR.PATCH</c>, e.g. "0.17.0".</summary>
        public static string Version() => Interop.TakeStaticString(Native.sekejap_version());

        /// <summary><c>sekejap_format_version</c>: the disk format this build
        /// reads and writes (2). A file stamped with anything else is
        /// refused by name, nothing changed.</summary>
        public static int FormatVersion() => Native.sekejap_format_version();

        // ── documents ─────────────────────────────────────────────────────────

        /// <summary><c>sekejap_put</c>: write one document, committed before
        /// this call returns. A <c>_key</c> member in <paramref name="documentJson"/>
        /// must equal <paramref name="key"/>. Fails on a collection that is
        /// not in the catalog -- declare it first with
        /// <see cref="CreateCollection"/> or <c>CREATE TABLE</c>.</summary>
        public void Put(string collection, string key, string documentJson)
        {
            if (Native.sekejap_put(_db, collection, key, documentJson) < 0) throw Interop.Fail("put");
        }

        /// <summary><c>sekejap_put_many</c>: many documents into one
        /// collection under ONE commit. <paramref name="rowsJson"/> is a
        /// JSON array of <c>{"key","doc"}</c>. A failure stores NONE of the
        /// batch. Returns the rows written.</summary>
        public long PutMany(string collection, string rowsJson)
        {
            long n = Native.sekejap_put_many(_db, collection, rowsJson);
            if (n < 0) throw Interop.Fail("put_many");
            return n;
        }

        /// <summary><c>sekejap_get</c>: the document with <c>_key</c> set,
        /// or <c>null</c> for a clean miss -- not an exception.</summary>
        public string? Get(string collection, string key)
        {
            IntPtr r = Native.sekejap_get(_db, collection, key);
            if (r != IntPtr.Zero) return Interop.TakeString(r);
            if (Interop.LastErrorCode() != SekejapStatus.Ok) throw Interop.Fail("get");
            return null;
        }

        /// <summary><c>sekejap_exists</c>: whether the row is there.</summary>
        public bool Exists(string collection, string key)
        {
            int r = Native.sekejap_exists(_db, collection, key);
            if (r < 0) throw Interop.Fail("exists");
            return r != 0;
        }

        /// <summary><c>sekejap_delete</c>: delete one row and every edge
        /// that touches it, committed. Returns whether it was there.</summary>
        public bool Delete(string collection, string key)
        {
            int r = Native.sekejap_delete(_db, collection, key);
            if (r < 0) throw Interop.Fail("delete");
            return r != 0;
        }

        /// <summary><c>sekejap_scan_open</c>: a walk of one collection in
        /// stable id order, holding at most <paramref name="pageRows"/> rows
        /// at a time (0 means sekejap's default of 256). Dispose the
        /// returned <see cref="SekejapScan"/> before closing this database.</summary>
        public SekejapScan ScanOpen(string collection, ulong pageRows = 0)
        {
            IntPtr h = Native.sekejap_scan_open(_db, collection, (UIntPtr)pageRows);
            if (h == IntPtr.Zero) throw Interop.Fail("scan_open");
            return new SekejapScan(h, isQuery: false);
        }

        // ── SQL ───────────────────────────────────────────────────────────────

        /// <summary><c>sekejap_execute</c>: one writing statement, committed.
        /// A statement that only raises a notice returns 0. Returns the
        /// rows it moved.</summary>
        public long Execute(string sql, string? paramsJson = null)
        {
            long n = Native.sekejap_execute(_db, sql, paramsJson);
            if (n < 0) throw Interop.Fail("execute");
            return n;
        }

        /// <summary><c>sekejap_query</c>: one row-returning statement. The
        /// answer is a JSON array of objects keyed by column name; a column
        /// MISSING in a row is omitted, because missing is not null.</summary>
        public string Query(string sql, string? paramsJson = null)
        {
            IntPtr r = Native.sekejap_query(_db, sql, paramsJson);
            if (r == IntPtr.Zero) throw Interop.Fail("query");
            return Interop.TakeString(r);
        }

        /// <summary><c>sekejap_explain</c>: the plan the engine would build for one statement.</summary>
        public string Explain(string sql, string? paramsJson = null)
        {
            IntPtr r = Native.sekejap_explain(_db, sql, paramsJson);
            if (r == IntPtr.Zero) throw Interop.Fail("explain");
            return Interop.TakeString(r);
        }

        /// <summary><c>sekejap_prepare</c>: PARSE one statement now -- a
        /// syntax error is reported here -- and compile it on its first
        /// bind. Dispose the returned <see cref="SekejapStatement"/> before
        /// closing this database.</summary>
        public SekejapStatement Prepare(string sql)
        {
            IntPtr h = Native.sekejap_prepare(_db, sql);
            if (h == IntPtr.Zero) throw Interop.Fail("prepare");
            return new SekejapStatement(h);
        }

        /// <summary><c>sekejap_query_open</c>: run a row-returning statement
        /// and open a PAGED DELIVERY of its answer (0 means 4,096 rows per
        /// page). The ENGINE pages the execution at <paramref name="pageRows"/>
        /// rows; the answer is assembled at open time because a compiled
        /// SELECT cannot be suspended between two calls -- this bounds the
        /// string per call and lets a caller stop reading, but does not
        /// bound the answer (docs/dist/C_ABI.md §4.4). Dispose the returned
        /// <see cref="SekejapScan"/> before closing this database.</summary>
        public SekejapScan QueryOpen(string sql, string? paramsJson = null, ulong pageRows = 0)
        {
            IntPtr h = Native.sekejap_query_open(_db, sql, paramsJson, (UIntPtr)pageRows);
            if (h == IntPtr.Zero) throw Interop.Fail("query_open");
            return new SekejapScan(h, isQuery: true);
        }

        // ── edges ─────────────────────────────────────────────────────────────

        /// <summary><c>sekejap_link</c>: link two rows with a typed edge in
        /// the base graph context, committed. BOTH endpoints must already
        /// exist -- a missing one is <see cref="SekejapStatus.UnknownRow"/>,
        /// never a dangling identity.</summary>
        public void Link(string fromCollection, string fromKey, string edgeType, string toCollection, string toKey)
        {
            if (Native.sekejap_link(_db, fromCollection, fromKey, edgeType, toCollection, toKey) < 0)
                throw Interop.Fail("link");
        }

        /// <summary><c>sekejap_link_with</c>: the same, carrying a JSON properties object.</summary>
        public void LinkWith(string fromCollection, string fromKey, string edgeType, string toCollection, string toKey, string propertiesJson)
        {
            if (Native.sekejap_link_with(_db, fromCollection, fromKey, edgeType, toCollection, toKey, propertiesJson) < 0)
                throw Interop.Fail("link_with");
        }

        /// <summary><c>sekejap_unlink</c>: remove one edge, committed.
        /// Returns whether it was there.</summary>
        public bool Unlink(string fromCollection, string fromKey, string edgeType, string toCollection, string toKey)
        {
            int r = Native.sekejap_unlink(_db, fromCollection, fromKey, edgeType, toCollection, toKey);
            if (r < 0) throw Interop.Fail("unlink");
            return r != 0;
        }

        /// <summary><c>sekejap_neighbours</c>: the rows one hop away, in one
        /// direction, under a complete-or-error bound of at most 256 edges
        /// -- a wider walk is <c>GRAPH_TABLE</c> in SQL and is refused here
        /// by name. <paramref name="edgeType"/> may be <c>null</c> for every
        /// type. The answer is a JSON array of
        /// <c>{"collection","key","document"}</c> -- the collection is
        /// named because a neighbour can be in another one.</summary>
        public string Neighbours(string collection, string key, string? edgeType, SekejapDirection direction, ulong limit)
        {
            IntPtr r = Native.sekejap_neighbours(_db, collection, key, edgeType, direction, (UIntPtr)limit);
            if (r == IntPtr.Zero) throw Interop.Fail("neighbours");
            return Interop.TakeString(r);
        }

        // ── the catalog ───────────────────────────────────────────────────────

        /// <summary><c>sekejap_create_collection</c>: declare a collection.
        /// <paramref name="fieldsJson"/> is a JSON array of
        /// <c>{"name","kind":"text"|"int"|"real"|"bool"|"json"|"geo"|"point"|"vector","dimension"?}</c>
        /// (<c>dimension</c> required for <c>vector</c>, rejected otherwise).
        /// The declaration is a floor, not a fence: a document may carry a
        /// field it does not name, stored in the row's extras. Returns
        /// whether it was created (<c>false</c> means it was already there).</summary>
        public bool CreateCollection(string name, string fieldsJson)
        {
            int r = Native.sekejap_create_collection(_db, name, fieldsJson);
            if (r < 0) throw Interop.Fail("create_collection");
            return r != 0;
        }

        /// <summary><c>sekejap_drop_collection</c>: remove a collection, its
        /// rows, its indexes and its descriptor. Returns whether it was there.</summary>
        public bool DropCollection(string name)
        {
            int r = Native.sekejap_drop_collection(_db, name);
            if (r < 0) throw Interop.Fail("drop_collection");
            return r != 0;
        }

        /// <summary><c>sekejap_collections</c>: every collection name, in
        /// key order, as a JSON array of strings.</summary>
        public string Collections()
        {
            IntPtr r = Native.sekejap_collections(_db);
            if (r == IntPtr.Zero) throw Interop.Fail("collections");
            return Interop.TakeString(r);
        }

        /// <summary><c>sekejap_describe</c>: the declared shape of one
        /// collection as JSON
        /// (<c>{"name","timestamps","rows","fields":[...],"indexes":[...]}</c>),
        /// or <c>null</c> when there is no such collection -- a clean miss,
        /// not an exception. <c>"rows"</c> is the LIVE row count or
        /// <c>null</c> where this database keeps no record for it.</summary>
        public string? Describe(string collection)
        {
            IntPtr r = Native.sekejap_describe(_db, collection);
            if (r != IntPtr.Zero) return Interop.TakeString(r);
            if (Interop.LastErrorCode() != SekejapStatus.Ok) throw Interop.Fail("describe");
            return null;
        }

        /// <summary><c>sekejap_count_rows</c>: the rows, from the LIVE
        /// record when this database keeps one and from a walk when it does not.</summary>
        public long CountRows(string collection)
        {
            long n = Native.sekejap_count_rows(_db, collection);
            if (n < 0) throw Interop.Fail("count_rows");
            return n;
        }

        /// <summary><c>sekejap_scan_count_rows</c>: the rows BY WALKING
        /// them, whether or not a live record exists. The explicit walk,
        /// named as one.</summary>
        public long ScanCountRows(string collection)
        {
            long n = Native.sekejap_scan_count_rows(_db, collection);
            if (n < 0) throw Interop.Fail("scan_count_rows");
            return n;
        }

        /// <summary><c>sekejap_scan_count_edges</c>: every edge BY WALKING
        /// the primary edge keyspace -- sekejap keeps no O(1) edge counter,
        /// so this stays a scan and is named as one.</summary>
        public long ScanCountEdges()
        {
            long n = Native.sekejap_scan_count_edges(_db);
            if (n < 0) throw Interop.Fail("scan_count_edges");
            return n;
        }

        // ── transactions ──────────────────────────────────────────────────────

        /// <summary><c>sekejap_tx_begin</c>: take the writer for many writes
        /// under ONE barrier. See <see cref="SekejapTx"/> for the commit /
        /// rollback bargain.</summary>
        public SekejapTx TxBegin()
        {
            IntPtr h = Native.sekejap_tx_begin(_db);
            if (h == IntPtr.Zero) throw Interop.Fail("tx_begin");
            return new SekejapTx(h);
        }

        // ── maintenance ───────────────────────────────────────────────────────

        /// <summary><c>sekejap_checkpoint</c>: fold the committed
        /// write-ahead log into the data file. <see cref="SekejapCheckpointResult.Deferred"/>
        /// (not a failure) while a live reader holds a slot -- in service
        /// mode that is every call, because the published read view holds
        /// one for its whole life.</summary>
        public SekejapCheckpointResult Checkpoint()
        {
            int r = Native.sekejap_checkpoint(_db);
            if (r < 0) throw Interop.Fail("checkpoint");
            return (SekejapCheckpointResult)r;
        }

        /// <summary><c>sekejap_publish</c>: make the newest commit visible
        /// to readers now. In single mode there is no published view to
        /// swap and every commit is already visible to this handle, so this
        /// succeeds having done nothing.</summary>
        public void Publish()
        {
            if (Native.sekejap_publish(_db) < 0) throw Interop.Fail("publish");
        }

        /// <summary><c>sekejap_storage</c>: the bytes on disk, as JSON
        /// <c>{"data_bytes","wal_bytes","total_bytes"}</c>.</summary>
        public string Storage()
        {
            IntPtr r = Native.sekejap_storage(_db);
            if (r == IntPtr.Zero) throw Interop.Fail("storage");
            return Interop.TakeString(r);
        }

        /// <summary><c>sekejap_trim_memory</c>: REFUSED. sekejap's caches
        /// are bounded at open (the buffer pool by <c>budget_bytes</c>, the
        /// plan cache by its three ceilings), so there is nothing
        /// proportional to rows held in memory to give back. Always throws
        /// -- a no-op that returned success would be a fake of reclaim.</summary>
        public void TrimMemory()
        {
            if (Native.sekejap_trim_memory(_db) < 0) throw Interop.Fail("trim_memory");
        }

        /// <summary><c>sekejap_compact</c>: REFUSED. There is no
        /// payload-rewriting compaction; <see cref="Checkpoint"/> folds the
        /// write-ahead log without rewriting rows, and naming that
        /// "compact" would promise something else. Always throws.</summary>
        public void Compact()
        {
            if (Native.sekejap_compact(_db) < 0) throw Interop.Fail("compact");
        }

        /// <summary><c>sekejap_show</c>: REFUSED. The <c>SHOW</c> family is
        /// not in this dialect -- <see cref="Collections"/> and
        /// <see cref="Describe"/> answer the same questions as data. Always
        /// throws.</summary>
        public string Show(string? statement)
        {
            IntPtr r = Native.sekejap_show(_db, statement);
            if (r == IntPtr.Zero) throw Interop.Fail("show");
            return Interop.TakeString(r);
        }

        // ── service mode ──────────────────────────────────────────────────────
        // Every call below is REFUSED BY NAME on a handle not opened with
        // OpenService: SekejapStatus.Refused, and a message naming the call
        // and saying single mode has no writer to time out, no interrupt and
        // no change feed.

        /// <summary><c>sekejap_statement_timeout_ms</c>: refuse a statement
        /// that runs longer than <paramref name="milliseconds"/>. 0 clears
        /// the timeout.</summary>
        public void StatementTimeoutMs(ulong milliseconds)
        {
            if (Native.sekejap_statement_timeout_ms(_db, milliseconds) < 0)
                throw Interop.Fail("statement_timeout_ms");
        }

        /// <summary><c>sekejap_cancel</c>: cancel the work in flight on this
        /// service, from any thread. Sticky until <see cref="ClearInterrupt"/>.</summary>
        public void Cancel()
        {
            if (Native.sekejap_cancel(_db) < 0) throw Interop.Fail("cancel");
        }

        /// <summary><c>sekejap_clear_interrupt</c>: clear a cancel so the
        /// service accepts work again. Returns whether one was standing.</summary>
        public bool ClearInterrupt()
        {
            int r = Native.sekejap_clear_interrupt(_db);
            if (r < 0) throw Interop.Fail("clear_interrupt");
            return r != 0;
        }

        /// <summary><c>sekejap_subscribe</c>: subscribe to the commit-time
        /// change feed. Returns a subscription id valid from any thread; a
        /// subscription left open is closed by <see cref="Dispose"/>.</summary>
        public long Subscribe()
        {
            long id = Native.sekejap_subscribe(_db);
            if (id < 0) throw Interop.Fail("subscribe");
            return id;
        }

        /// <summary><c>sekejap_next_change</c>: the next change event for
        /// <paramref name="subscriptionId"/>, as JSON
        /// <c>{"sequence","collections","edge_types","keys","keys_total","keys_truncated","unnamed_writes","rows_affected"}</c>,
        /// or <c>null</c> when none arrived within <paramref name="timeoutMs"/>
        /// (0 polls and returns at once).</summary>
        public string? NextChange(long subscriptionId, ulong timeoutMs)
        {
            IntPtr r = Native.sekejap_next_change(_db, subscriptionId, timeoutMs);
            if (r != IntPtr.Zero) return Interop.TakeString(r);
            if (Interop.LastErrorCode() != SekejapStatus.Ok) throw Interop.Fail("next_change");
            return null;
        }

        /// <summary><c>sekejap_unsubscribe</c>: close one subscription.
        /// Returns whether it was open.</summary>
        public bool Unsubscribe(long subscriptionId)
        {
            int r = Native.sekejap_unsubscribe(_db, subscriptionId);
            if (r < 0) throw Interop.Fail("unsubscribe");
            return r != 0;
        }

        // ── IDisposable ───────────────────────────────────────────────────────

        /// <summary><c>sekejap_close</c>: close and free. Null-safe,
        /// idempotent. Uncommitted work is discarded -- a close is not a
        /// commit. Every <see cref="SekejapStatement"/>, <see cref="SekejapScan"/>
        /// and <see cref="SekejapTx"/> taken from this handle must be
        /// disposed FIRST.</summary>
        public void Dispose()
        {
            if (_db != IntPtr.Zero) { Native.sekejap_close(_db); _db = IntPtr.Zero; }
        }
    }
}
