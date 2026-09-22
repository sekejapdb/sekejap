using System;

namespace Sekejap
{
    /// <summary>
    /// Thrown on a sekejap C ABI failure. Carries the engine's last error
    /// message (<c>sekejap_last_error</c>) and the closed
    /// <see cref="SekejapStatus"/> code (<c>sekejap_last_error_code</c>,
    /// docs/dist/C_ABI.md §1.1) so a caller can branch on the reason without
    /// parsing <see cref="Exception.Message"/>.
    /// </summary>
    public sealed class SekejapException : Exception
    {
        /// <summary>The closed failure code -- never <see cref="SekejapStatus.Ok"/>,
        /// since an <c>Ok</c> answer never raises this exception.</summary>
        public SekejapStatus Status { get; }

        public SekejapException(string message, SekejapStatus status) : base(message)
        {
            Status = status;
        }
    }
}
