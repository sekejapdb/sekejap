using System;
using System.Runtime.InteropServices;

namespace Sekejap
{
    /// <summary>
    /// C-string ownership and error-translation helpers shared by every
    /// public type in this assembly (docs/dist/C_ABI.md §1: "strings OUT
    /// are owned", "errors are THREAD-LOCAL").
    /// </summary>
    internal static class Interop
    {
        /// <summary>Copy an owned C string into a managed string and free it
        /// exactly once with <c>sekejap_string_free</c>, per the C ABI's
        /// ownership rule. <see cref="IntPtr.Zero"/> in is <c>""</c> out --
        /// callers that need to tell a miss from an empty string check the
        /// native pointer BEFORE calling this.</summary>
        internal static string TakeString(IntPtr p)
        {
            if (p == IntPtr.Zero) return string.Empty;
            string s = Marshal.PtrToStringUTF8(p) ?? string.Empty;
            Native.sekejap_string_free(p);
            return s;
        }

        /// <summary><c>sekejap_version</c> points into static program data --
        /// read it, never free it.</summary>
        internal static string TakeStaticString(IntPtr p) =>
            p == IntPtr.Zero ? string.Empty : (Marshal.PtrToStringUTF8(p) ?? string.Empty);

        /// <summary>The thread-local error slot. <c>db</c> is accepted by the
        /// C ABI and ignored -- the slot belongs to the CALLING THREAD, not
        /// the handle, which is what lets a failed <c>sekejap_open</c> (no
        /// handle yet) still report -- so every caller here passes
        /// <see cref="IntPtr.Zero"/>.</summary>
        internal static string LastErrorMessage() => TakeString(Native.sekejap_last_error(IntPtr.Zero));

        /// <summary>The thread-local error code; <see cref="SekejapStatus.Ok"/>
        /// after a success or a clean miss.</summary>
        internal static SekejapStatus LastErrorCode() => (SekejapStatus)Native.sekejap_last_error_code(IntPtr.Zero);

        /// <summary>Build a <see cref="SekejapException"/> from the
        /// thread-local error slot after a call whose sentinel (`NULL` or
        /// `-1`) said it failed. <paramref name="what"/> is the C function
        /// name (without the `sekejap_` prefix), used as a fallback message
        /// only in the theoretical case the slot is empty.</summary>
        internal static SekejapException Fail(string what)
        {
            string message = LastErrorMessage();
            SekejapStatus code = LastErrorCode();
            return new SekejapException(message.Length > 0 ? message : $"sekejap_{what} failed", code);
        }
    }
}
