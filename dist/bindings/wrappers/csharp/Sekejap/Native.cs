using System;
using System.Runtime.InteropServices;

namespace Sekejap
{
    // P/Invoke declarations for the sekejap C ABI (see wrappers/c/include/sekejap.h).
    // The library name "sekejap" resolves to libsekejap.{so,dylib} / sekejap.dll.
    //
    // NOTE: the C ABI returns C `long` (execute/put_many/counts). That is 64-bit on
    // macOS/Linux (LP64) — matched here by C# `long`. On Windows C `long` is 32-bit;
    // add a Windows-specific marshaling pass (or harden the C ABI to int64_t) before
    // shipping Windows binaries.
    internal static class Native
    {
        private const string Lib = "sekejap";

        [DllImport(Lib)] internal static extern IntPtr sekejap_open([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
        [DllImport(Lib)] internal static extern IntPtr sekejap_open_paged([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
        [DllImport(Lib)] internal static extern IntPtr sekejap_open_read_only([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
        [DllImport(Lib)] internal static extern void sekejap_close(IntPtr db);

        [DllImport(Lib)] internal static extern long sekejap_execute(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string sql);
        [DllImport(Lib)] internal static extern long sekejap_execute_params(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string sql, [MarshalAs(UnmanagedType.LPUTF8Str)] string paramsJson);
        [DllImport(Lib)] internal static extern IntPtr sekejap_query(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string sql);
        [DllImport(Lib)] internal static extern IntPtr sekejap_query_params(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string sql, [MarshalAs(UnmanagedType.LPUTF8Str)] string paramsJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_prepare(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string sql);
        [DllImport(Lib)] internal static extern IntPtr sekejap_query_prepared(IntPtr db, IntPtr stmt, [MarshalAs(UnmanagedType.LPUTF8Str)] string paramsJson);
        [DllImport(Lib)] internal static extern void sekejap_stmt_free(IntPtr stmt);

        [DllImport(Lib)] internal static extern int sekejap_put(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string slug, [MarshalAs(UnmanagedType.LPUTF8Str)] string payloadJson);
        [DllImport(Lib)] internal static extern long sekejap_put_many(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string rowsJson);
        [DllImport(Lib)] internal static extern IntPtr sekejap_get(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string slug);
        [DllImport(Lib)] internal static extern int sekejap_remove(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string slug);
        [DllImport(Lib)] internal static extern int sekejap_link(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string from, [MarshalAs(UnmanagedType.LPUTF8Str)] string to, [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType);
        [DllImport(Lib)] internal static extern int sekejap_contains(IntPtr db, [MarshalAs(UnmanagedType.LPUTF8Str)] string slug);

        [DllImport(Lib)] internal static extern long sekejap_node_count(IntPtr db);
        [DllImport(Lib)] internal static extern long sekejap_edge_count(IntPtr db);
        [DllImport(Lib)] internal static extern IntPtr sekejap_collection_names(IntPtr db);
        [DllImport(Lib)] internal static extern int sekejap_compact(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_last_error(IntPtr db);
        [DllImport(Lib)] internal static extern void sekejap_string_free(IntPtr s);
        [DllImport(Lib)] internal static extern IntPtr sekejap_version();
    }
}
