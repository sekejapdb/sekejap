using System;

namespace Sekejap
{
    /// <summary>
    /// One statement, PARSED at <see cref="SekejapDb.Prepare"/> -- a syntax
    /// error is reported there -- and compiled on its first bind here.
    /// Dispose (or <c>using</c>) before closing the owning
    /// <see cref="SekejapDb"/>.
    /// </summary>
    public sealed class SekejapStatement : IDisposable
    {
        private IntPtr _handle;

        internal SekejapStatement(IntPtr handle) => _handle = handle;

        /// <summary><c>sekejap_stmt_query</c>: run it as a row-returning
        /// statement. Same JSON shape as <see cref="SekejapDb.Query"/>.</summary>
        public string Query(string? paramsJson = null)
        {
            IntPtr r = Native.sekejap_stmt_query(_handle, paramsJson);
            if (r == IntPtr.Zero) throw Interop.Fail("stmt_query");
            return Interop.TakeString(r);
        }

        /// <summary><c>sekejap_stmt_execute</c>: run it as a writing
        /// statement and commit. Returns the rows it moved.</summary>
        public long Execute(string? paramsJson = null)
        {
            long n = Native.sekejap_stmt_execute(_handle, paramsJson);
            if (n < 0) throw Interop.Fail("stmt_execute");
            return n;
        }

        /// <summary><c>sekejap_stmt_rebindable</c>: whether a further bind
        /// compiles nothing. Always <see cref="SekejapRebind.No"/> for a
        /// WRITING statement, because a write folds its document at
        /// compile -- that saves the parse and nothing else.</summary>
        public SekejapRebind Rebindable()
        {
            int r = Native.sekejap_stmt_rebindable(_handle);
            if (r < 0) throw Interop.Fail("stmt_rebindable");
            return (SekejapRebind)r;
        }

        /// <summary><c>sekejap_stmt_free</c>. Null-safe, idempotent.</summary>
        public void Dispose()
        {
            if (_handle != IntPtr.Zero) { Native.sekejap_stmt_free(_handle); _handle = IntPtr.Zero; }
        }
    }
}
