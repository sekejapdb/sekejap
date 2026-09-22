using System;

namespace Sekejap
{
    /// <summary>
    /// The writer, held across many writes under ONE barrier
    /// (<see cref="SekejapDb.TxBegin"/>). Every plain <see cref="SekejapDb"/>
    /// call commits per call; a transaction is the other bargain. While one
    /// is open it HOLDS the writer: a call on the same <see cref="SekejapDb"/>
    /// that needs the writer -- from this thread or another -- waits for it.
    /// </summary>
    /// <remarks>
    /// <c>sekejap_tx_commit</c> and <c>sekejap_tx_rollback</c> both FREE the
    /// native handle whether or not they succeed -- the pointer is dangling
    /// after either call, in both cases -- so <see cref="Commit"/> and
    /// <see cref="Rollback"/> do the same here. <see cref="Dispose"/> is the
    /// idiomatic backstop for a <c>using</c> block that exits without an
    /// explicit <see cref="Commit"/>: it rolls back, mirroring the header's
    /// "a handle dropped any other way ROLLS BACK" (docs/dist/C_ABI.md §3),
    /// and is a no-op after either <see cref="Commit"/> or
    /// <see cref="Rollback"/> already ran.
    /// </remarks>
    public sealed class SekejapTx : IDisposable
    {
        private IntPtr _handle;

        internal SekejapTx(IntPtr handle) => _handle = handle;

        /// <summary><c>sekejap_tx_put</c>: write one document, NOT committed.</summary>
        public void Put(string collection, string key, string documentJson)
        {
            if (Native.sekejap_tx_put(_handle, collection, key, documentJson) < 0)
                throw Interop.Fail("tx_put");
        }

        /// <summary><c>sekejap_tx_delete</c>: delete one row, NOT committed.
        /// Returns whether it was there.</summary>
        public bool Delete(string collection, string key)
        {
            int r = Native.sekejap_tx_delete(_handle, collection, key);
            if (r < 0) throw Interop.Fail("tx_delete");
            return r != 0;
        }

        /// <summary><c>sekejap_tx_link</c>: link two rows, NOT committed.</summary>
        public void Link(string fromCollection, string fromKey, string edgeType, string toCollection, string toKey)
        {
            if (Native.sekejap_tx_link(_handle, fromCollection, fromKey, edgeType, toCollection, toKey) < 0)
                throw Interop.Fail("tx_link");
        }

        /// <summary><c>sekejap_tx_execute</c>: one writing statement, NOT
        /// committed. Returns the rows it moved.</summary>
        public long Execute(string sql, string? paramsJson = null)
        {
            long n = Native.sekejap_tx_execute(_handle, sql, paramsJson);
            if (n < 0) throw Interop.Fail("tx_execute");
            return n;
        }

        /// <summary><c>sekejap_tx_commit</c>: commit and free the handle.
        /// The pointer is dangling after this call whether it succeeds or
        /// not.</summary>
        public void Commit()
        {
            IntPtr h = TakeHandle();
            if (Native.sekejap_tx_commit(h) < 0) throw Interop.Fail("tx_commit");
        }

        /// <summary><c>sekejap_tx_rollback</c>: roll back and free the
        /// handle. The pointer is dangling after this call whether it
        /// succeeds or not.</summary>
        public void Rollback()
        {
            IntPtr h = TakeHandle();
            if (Native.sekejap_tx_rollback(h) < 0) throw Interop.Fail("tx_rollback");
        }

        private IntPtr TakeHandle()
        {
            IntPtr h = _handle;
            _handle = IntPtr.Zero;
            if (h == IntPtr.Zero)
                throw new SekejapException("transaction already committed or rolled back", SekejapStatus.Invalid);
            return h;
        }

        /// <summary>Idiomatic backstop: rolls back if neither
        /// <see cref="Commit"/> nor <see cref="Rollback"/> was called yet.
        /// A no-op after either. Dispose must not throw, so a rollback
        /// failure here is silent -- call <see cref="Rollback"/> directly
        /// to observe it.</summary>
        public void Dispose()
        {
            if (_handle == IntPtr.Zero) return;
            IntPtr h = _handle;
            _handle = IntPtr.Zero;
            Native.sekejap_tx_rollback(h);
        }
    }
}
