using System;
using System.Runtime.InteropServices;

namespace Sekejap
{
    // Raw P/Invoke declarations for the sekejap 0.17.0 C ABI: one entry per
    // `extern "C"` function in dist/ffi/include/sekejap.h (contract:
    // docs/dist/C_ABI.md). 59 functions, none of them hand-written beyond
    // this file -- everything idiomatic (exceptions, IDisposable, optional
    // parameters) lives in the wrapper types that call through this class.
    //
    // "sekejap" resolves to libsekejap.{so,dylib} / sekejap.dll: the crate
    // is `sekejap-capi`, but `[lib] name = "sekejap"` in dist/ffi/Cargo.toml
    // makes the file on disk `libsekejap`, which is what DllImport needs.
    //
    // NOTE (Windows `long`): the C ABI spells several return/parameter types
    // as C `long` -- sekejap_execute, sekejap_put_many, sekejap_stmt_execute,
    // sekejap_count_rows, sekejap_scan_count_rows, sekejap_scan_count_edges,
    // sekejap_subscribe, and the `subscription` argument of
    // sekejap_next_change/sekejap_unsubscribe. C `long` is 64-bit on
    // macOS/Linux (LP64) -- matched here by C# `long` -- but 32-bit on
    // Windows (LLP64). A Windows-specific marshaling pass (or hardening the
    // C ABI to `int64_t`) is needed before shipping Windows binaries; see
    // README "Caveats". `uintptr_t` parameters (page sizes, the neighbours
    // limit) are `UIntPtr`, which is pointer-width on every platform and
    // therefore has no such mismatch.
    internal static class Native
    {
        private const string Lib = "sekejap";

        // ---- 4.1 Opening and identity --------------------------------------------------

        [DllImport(Lib)] internal static extern IntPtr sekejap_open(
            [MarshalAs(UnmanagedType.LPUTF8Str)] string path);

        [DllImport(Lib)] internal static extern IntPtr sekejap_open_with_config(
            [MarshalAs(UnmanagedType.LPUTF8Str)] string path,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? configJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_open_service(
            [MarshalAs(UnmanagedType.LPUTF8Str)] string path);

        [DllImport(Lib)] internal static extern void sekejap_close(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_version();

        [DllImport(Lib)] internal static extern int sekejap_format_version();

        // ---- 4.2 Errors and memory ------------------------------------------------------

        [DllImport(Lib)] internal static extern IntPtr sekejap_last_error(IntPtr db);

        [DllImport(Lib)] internal static extern int sekejap_last_error_code(IntPtr db);

        [DllImport(Lib)] internal static extern void sekejap_string_free(IntPtr s);

        // ---- 4.3 Documents ---------------------------------------------------------------

        [DllImport(Lib)] internal static extern int sekejap_put(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string documentJson);

        [DllImport(Lib)] internal static extern long sekejap_put_many(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string rowsJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_get(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key);

        [DllImport(Lib)] internal static extern int sekejap_exists(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key);

        [DllImport(Lib)] internal static extern int sekejap_delete(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key);

        [DllImport(Lib)] internal static extern IntPtr sekejap_scan_open(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            UIntPtr pageRows);

        [DllImport(Lib)] internal static extern IntPtr sekejap_scan_next(IntPtr scan);

        [DllImport(Lib)] internal static extern void sekejap_scan_close(IntPtr scan);

        // ---- 4.4 SQL ----------------------------------------------------------------------

        [DllImport(Lib)] internal static extern long sekejap_execute(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_query(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_explain(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern IntPtr sekejap_prepare(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql);

        [DllImport(Lib)] internal static extern IntPtr sekejap_stmt_query(
            IntPtr stmt,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern long sekejap_stmt_execute(
            IntPtr stmt,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern int sekejap_stmt_rebindable(IntPtr stmt);

        [DllImport(Lib)] internal static extern void sekejap_stmt_free(IntPtr stmt);

        [DllImport(Lib)] internal static extern IntPtr sekejap_query_open(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson,
            UIntPtr pageRows);

        [DllImport(Lib)] internal static extern IntPtr sekejap_query_next(IntPtr scan);

        [DllImport(Lib)] internal static extern void sekejap_query_close(IntPtr scan);

        // ---- 4.5 Edges --------------------------------------------------------------------

        [DllImport(Lib)] internal static extern int sekejap_link(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromKey,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toKey);

        [DllImport(Lib)] internal static extern int sekejap_link_with(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromKey,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toKey,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string propertiesJson);

        [DllImport(Lib)] internal static extern int sekejap_unlink(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromKey,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toKey);

        [DllImport(Lib)] internal static extern IntPtr sekejap_neighbours(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? edgeType,
            SekejapDirection direction,
            UIntPtr limit);

        // ---- 4.6 The catalog ---------------------------------------------------------------

        [DllImport(Lib)] internal static extern int sekejap_create_collection(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string name,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fieldsJson);

        [DllImport(Lib)] internal static extern int sekejap_drop_collection(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string name);

        [DllImport(Lib)] internal static extern IntPtr sekejap_collections(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_describe(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection);

        [DllImport(Lib)] internal static extern long sekejap_count_rows(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection);

        [DllImport(Lib)] internal static extern long sekejap_scan_count_rows(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection);

        [DllImport(Lib)] internal static extern long sekejap_scan_count_edges(IntPtr db);

        // ---- 4.7 Transactions ---------------------------------------------------------------

        [DllImport(Lib)] internal static extern IntPtr sekejap_tx_begin(IntPtr db);

        [DllImport(Lib)] internal static extern int sekejap_tx_put(
            IntPtr tx,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string documentJson);

        [DllImport(Lib)] internal static extern int sekejap_tx_delete(
            IntPtr tx,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string collection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string key);

        [DllImport(Lib)] internal static extern int sekejap_tx_link(
            IntPtr tx,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string fromKey,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string edgeType,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toCollection,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string toKey);

        [DllImport(Lib)] internal static extern long sekejap_tx_execute(
            IntPtr tx,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? paramsJson);

        [DllImport(Lib)] internal static extern int sekejap_tx_commit(IntPtr tx);

        [DllImport(Lib)] internal static extern int sekejap_tx_rollback(IntPtr tx);

        // ---- 4.8 Maintenance ----------------------------------------------------------------

        [DllImport(Lib)] internal static extern int sekejap_checkpoint(IntPtr db);

        [DllImport(Lib)] internal static extern int sekejap_publish(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_storage(IntPtr db);

        // ---- 4.9 Service mode ---------------------------------------------------------------

        [DllImport(Lib)] internal static extern int sekejap_statement_timeout_ms(
            IntPtr db, ulong milliseconds);

        [DllImport(Lib)] internal static extern int sekejap_cancel(IntPtr db);

        [DllImport(Lib)] internal static extern int sekejap_clear_interrupt(IntPtr db);

        [DllImport(Lib)] internal static extern long sekejap_subscribe(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_next_change(
            IntPtr db, long subscription, ulong timeoutMs);

        [DllImport(Lib)] internal static extern int sekejap_unsubscribe(
            IntPtr db, long subscription);

        // ---- REFUSED by name (kept 1:1 so the wrapper never fakes a memory
        //      store, a memory trim, a payload-rewriting compact or SHOW) --------------

        [DllImport(Lib)] internal static extern IntPtr sekejap_open_memory();

        [DllImport(Lib)] internal static extern int sekejap_trim_memory(IntPtr db);

        [DllImport(Lib)] internal static extern int sekejap_compact(IntPtr db);

        [DllImport(Lib)] internal static extern IntPtr sekejap_show(
            IntPtr db,
            [MarshalAs(UnmanagedType.LPUTF8Str)] string? statement);
    }
}
