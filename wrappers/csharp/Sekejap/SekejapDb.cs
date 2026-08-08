using System;
using System.Runtime.InteropServices;
using System.Text;

namespace Sekejap
{
    /// <summary>Thrown on a sekejap C ABI failure; carries the engine's last error.</summary>
    public sealed class SekejapException : Exception
    {
        public SekejapException(string message) : base(message) { }
    }

    /// <summary>A prepared (compiled) query. Dispose frees it (or use <c>using</c>).</summary>
    public sealed class SekejapStatement : IDisposable
    {
        internal IntPtr Handle;
        internal SekejapStatement(IntPtr handle) => Handle = handle;

        public void Dispose()
        {
            if (Handle != IntPtr.Zero) { Native.sekejap_stmt_free(Handle); Handle = IntPtr.Zero; }
        }
    }

    /// <summary>
    /// An open sekejap database — SQL + graph + spatial + vector + full-text over
    /// one embedded engine. Results come back as JSON strings. Dispose closes it.
    /// </summary>
    public sealed class SekejapDb : IDisposable
    {
        private IntPtr _db;

        private SekejapDb(IntPtr db) => _db = db;

        // ── open ──────────────────────────────────────────────────────────────
        public static SekejapDb Open(string path) => Opened(Native.sekejap_open(path), nameof(Open));
        public static SekejapDb OpenPaged(string path) => Opened(Native.sekejap_open_paged(path), nameof(OpenPaged));
        public static SekejapDb OpenReadOnly(string path) => Opened(Native.sekejap_open_read_only(path), nameof(OpenReadOnly));

        private static SekejapDb Opened(IntPtr db, string what)
        {
            if (db == IntPtr.Zero) throw new SekejapException($"sekejap_{what} failed");
            return new SekejapDb(db);
        }

        // ── statements & queries ────────────────────────────────────────────────
        public long Execute(string sql)
        {
            long n = Native.sekejap_execute(_db, sql);
            if (n < 0) throw Fail("execute");
            return n;
        }

        public long ExecuteParams(string sql, string paramsJson)
        {
            long n = Native.sekejap_execute_params(_db, sql, paramsJson);
            if (n < 0) throw Fail("execute_params");
            return n;
        }

        public string Query(string sql)
        {
            IntPtr r = Native.sekejap_query(_db, sql);
            if (r == IntPtr.Zero) throw Fail("query");
            return TakeString(r);
        }

        public string QueryParams(string sql, string paramsJson)
        {
            IntPtr r = Native.sekejap_query_params(_db, sql, paramsJson);
            if (r == IntPtr.Zero) throw Fail("query_params");
            return TakeString(r);
        }

        public SekejapStatement Prepare(string sql)
        {
            IntPtr s = Native.sekejap_prepare(_db, sql);
            if (s == IntPtr.Zero) throw Fail("prepare");
            return new SekejapStatement(s);
        }

        public string QueryPrepared(SekejapStatement stmt, string paramsJson)
        {
            IntPtr r = Native.sekejap_query_prepared(_db, stmt.Handle, paramsJson);
            if (r == IntPtr.Zero) throw Fail("query_prepared");
            return TakeString(r);
        }

        // ── records & graph ─────────────────────────────────────────────────────
        public void Put(string slug, string payloadJson)
        {
            if (Native.sekejap_put(_db, slug, payloadJson) < 0) throw Fail("put");
        }

        public long PutMany(string rowsJson)
        {
            long n = Native.sekejap_put_many(_db, rowsJson);
            if (n < 0) throw Fail("put_many");
            return n;
        }

        /// <summary>The node's JSON payload, or <c>null</c> if it does not exist.</summary>
        public string? Get(string slug)
        {
            IntPtr r = Native.sekejap_get(_db, slug);
            if (r != IntPtr.Zero) return TakeString(r);
            string e = LastError();               // null: clean miss clears last_error
            if (e.Length > 0) throw new SekejapException(e);
            return null;
        }

        public void Remove(string slug)
        {
            if (Native.sekejap_remove(_db, slug) < 0) throw Fail("remove");
        }

        public void Link(string from, string to, string edgeType)
        {
            if (Native.sekejap_link(_db, from, to, edgeType) < 0) throw Fail("link");
        }

        public bool Contains(string slug)
        {
            int r = Native.sekejap_contains(_db, slug);
            if (r < 0) throw Fail("contains");
            return r != 0;
        }

        // ── introspection & maintenance ─────────────────────────────────────────
        public long NodeCount() => Native.sekejap_node_count(_db);
        public long EdgeCount() => Native.sekejap_edge_count(_db);
        public string CollectionNames() => TakeString(Native.sekejap_collection_names(_db));

        public void Compact()
        {
            if (Native.sekejap_compact(_db) < 0) throw Fail("compact");
        }

        /// <summary>Library version, e.g. "0.16.2".</summary>
        public static string Version() => TakeStaticString(Native.sekejap_version());

        // ── IDisposable ─────────────────────────────────────────────────────────
        public void Dispose()
        {
            if (_db != IntPtr.Zero) { Native.sekejap_close(_db); _db = IntPtr.Zero; }
        }

        // ── helpers ─────────────────────────────────────────────────────────────
        private SekejapException Fail(string what)
        {
            string e = LastError();
            return new SekejapException(e.Length > 0 ? e : $"sekejap {what} failed");
        }

        private string LastError() => TakeString(Native.sekejap_last_error(_db));

        // Copy an owned C string into a managed string and free it (C ABI contract).
        private static string TakeString(IntPtr p)
        {
            if (p == IntPtr.Zero) return string.Empty;
            string s = Marshal.PtrToStringUTF8(p) ?? string.Empty;
            Native.sekejap_string_free(p);
            return s;
        }

        // sekejap_version returns a static string — read it but do NOT free.
        private static string TakeStaticString(IntPtr p) =>
            p == IntPtr.Zero ? string.Empty : (Marshal.PtrToStringUTF8(p) ?? string.Empty);
    }
}
