using System;

namespace Sekejap
{
    /// <summary>
    /// A paged walk: of one collection (<see cref="SekejapDb.ScanOpen"/> --
    /// C <c>sekejap_scan_open</c> / <c>sekejap_scan_next</c> /
    /// <c>sekejap_scan_close</c>) or of one statement's answer
    /// (<see cref="SekejapDb.QueryOpen"/> -- C <c>sekejap_query_open</c> /
    /// <c>sekejap_query_next</c> / <c>sekejap_query_close</c>). The header
    /// states these are the SAME operation under the name that matches how
    /// the walk was opened (docs/dist/C_ABI.md §4.4), which this class
    /// mirrors with an internal flag rather than two native calls into one.
    /// Dispose (or <c>using</c>) before closing the owning
    /// <see cref="SekejapDb"/>.
    /// </summary>
    public sealed class SekejapScan : IDisposable
    {
        private IntPtr _handle;
        private readonly bool _isQuery;

        internal SekejapScan(IntPtr handle, bool isQuery)
        {
            _handle = handle;
            _isQuery = isQuery;
        }

        /// <summary>
        /// The next page: a JSON array of documents (a collection scan) or
        /// of column-keyed row objects (a query stream). <c>null</c> at the
        /// end of the walk -- a clean end, not an exception.
        /// </summary>
        public string? Next()
        {
            IntPtr r = _isQuery ? Native.sekejap_query_next(_handle) : Native.sekejap_scan_next(_handle);
            if (r != IntPtr.Zero) return Interop.TakeString(r);
            if (Interop.LastErrorCode() != SekejapStatus.Ok)
                throw Interop.Fail(_isQuery ? "query_next" : "scan_next");
            return null;
        }

        /// <summary>Closes the walk. Null-safe, idempotent.</summary>
        public void Dispose()
        {
            if (_handle == IntPtr.Zero) return;
            if (_isQuery) Native.sekejap_query_close(_handle);
            else Native.sekejap_scan_close(_handle);
            _handle = IntPtr.Zero;
        }
    }
}
