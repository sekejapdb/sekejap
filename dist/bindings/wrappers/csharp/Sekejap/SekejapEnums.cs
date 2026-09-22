namespace Sekejap
{
    /// <summary>
    /// Mirrors C <c>SekejapStatus</c> (docs/dist/C_ABI.md §1.1): a closed
    /// enumeration a wrapper maps without parsing the message text. `Ok` is
    /// also what a clean MISS leaves behind.
    /// </summary>
    public enum SekejapStatus
    {
        /// <summary>The last call succeeded, or answered a clean miss.</summary>
        Ok = 0,
        /// <summary>A construct sekejap has no atomic for, named with its
        /// reason: a Tier-2/Tier-3 statement, an in-memory open, a
        /// payload-rewriting compact, a service call on a single-mode
        /// handle, a read-only store.</summary>
        Refused = 1,
        /// <summary>A page or a log failed verification. Nothing was changed.</summary>
        Corrupt = 2,
        /// <summary>A format, policy or configuration this build does not implement.</summary>
        Unsupported = 3,
        /// <summary>The directory, the file or the medium refused.</summary>
        Io = 4,
        /// <summary>The caller's arguments are wrong: a null pointer, text
        /// that is not UTF-8, JSON that does not parse, a collection that is
        /// not in the catalog, a parameter of the wrong type, a syntax error.</summary>
        Invalid = 5,
        /// <summary>A bound refused rather than waiting: a work budget, a
        /// statement deadline, a cancel, a second writer, a reader slot.</summary>
        Busy = 6,
        /// <summary>The named row is not in the collection, on a call that
        /// needs it to exist -- an edge endpoint.</summary>
        UnknownRow = 7,
        /// <summary>Nothing above classified it, including a panic caught at
        /// the boundary. The message is still in the last-error slot.</summary>
        Unknown = 8,
    }

    /// <summary>Mirrors C <c>SekejapDirection</c>, for <see cref="SekejapDb.Neighbours"/>.</summary>
    public enum SekejapDirection
    {
        /// <summary>Edges that leave the row.</summary>
        Outgoing = 0,
        /// <summary>Edges that arrive at the row.</summary>
        Incoming = 1,
        /// <summary>Both, with each neighbour reported once.</summary>
        Both = 2,
    }

    /// <summary>
    /// The three answers of <c>sekejap_stmt_rebindable</c>
    /// (<see cref="SekejapStatement.Rebindable"/>). `-1` is a failure and
    /// throws instead of landing here; `SEKEJAP_REBIND_UNBOUND` in the
    /// header is `Unbound` = 2.
    /// </summary>
    public enum SekejapRebind
    {
        /// <summary>A further bind would recompile the statement.</summary>
        No = 0,
        /// <summary>A further bind compiles nothing.</summary>
        Yes = 1,
        /// <summary>Not bound yet -- compiled by the first
        /// <see cref="SekejapStatement.Query"/> or
        /// <see cref="SekejapStatement.Execute"/>. Not a failure.</summary>
        Unbound = 2,
    }

    /// <summary>
    /// The two answers of <c>sekejap_checkpoint</c> (<see cref="SekejapDb.Checkpoint"/>).
    /// `-1` is a failure and throws instead of landing here.
    /// </summary>
    public enum SekejapCheckpointResult
    {
        /// <summary>A live reader holds a slot, so the fold was deferred.
        /// In service mode this is every call, because the published read
        /// view holds one for its whole life. Not a failure.</summary>
        Deferred = 0,
        /// <summary>The committed write-ahead log was folded into the data file.</summary>
        Folded = 1,
    }
}
