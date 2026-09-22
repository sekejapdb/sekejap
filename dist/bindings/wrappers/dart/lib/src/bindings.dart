// The 59 `extern "C"` entry points of `libsekejap`, looked up one to one.
//
// This file is the only place that names a C symbol. It is written against
// `dist/ffi/include/sekejap.h` (contract: `docs/dist/C_ABI.md`) and adds
// nothing: no allocation policy, no JSON, no error mapping. The classes in
// `db.dart`, `statement.dart`, `scan.dart` and `tx.dart` put the Dart idioms
// on top.
//
// Integer widths follow the header: `int32_t` is [Int32], `uint64_t` is
// [Uint64], `uintptr_t` is [UintPtr], and C `long` is [Long] -- which is 64
// bits on macOS, Linux and Android and 32 bits on Windows, so the ABI-specific
// type is what keeps one Dart `int` correct on all of them.

import 'dart:ffi';

import 'package:ffi/ffi.dart';

/// An open database. Opaque: Dart never looks inside one.
final class NativeDb extends Opaque {}

/// A prepared statement, borrowing its [NativeDb].
final class NativeStmt extends Opaque {}

/// The writer, held across many writes.
final class NativeTx extends Opaque {}

/// A paged walk: of a collection, or of one statement's answer.
final class NativeScan extends Opaque {}

/// Every C function of `libsekejap`, bound to one Dart closure each.
class SekejapBindings {
  SekejapBindings(this.library);

  /// The loaded `libsekejap.{dylib,so,dll}`.
  final DynamicLibrary library;

  // ── 4.1 Opening and identity ──────────────────────────────────────────────

  late final open = library.lookupFunction<
      Pointer<NativeDb> Function(Pointer<Utf8>),
      Pointer<NativeDb> Function(Pointer<Utf8>)>('sekejap_open');

  late final openWithConfig = library.lookupFunction<
      Pointer<NativeDb> Function(Pointer<Utf8>, Pointer<Utf8>),
      Pointer<NativeDb> Function(
          Pointer<Utf8>, Pointer<Utf8>)>('sekejap_open_with_config');

  late final openService = library.lookupFunction<
      Pointer<NativeDb> Function(Pointer<Utf8>),
      Pointer<NativeDb> Function(Pointer<Utf8>)>('sekejap_open_service');

  late final close = library.lookupFunction<Void Function(Pointer<NativeDb>),
      void Function(Pointer<NativeDb>)>('sekejap_close');

  late final version = library.lookupFunction<Pointer<Utf8> Function(),
      Pointer<Utf8> Function()>('sekejap_version');

  late final formatVersion =
      library.lookupFunction<Int32 Function(), int Function()>(
          'sekejap_format_version');

  // ── 4.2 Errors and memory ─────────────────────────────────────────────────

  late final lastError = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>),
      Pointer<Utf8> Function(Pointer<NativeDb>)>('sekejap_last_error');

  late final lastErrorCode = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_last_error_code');

  late final stringFree = library.lookupFunction<Void Function(Pointer<Utf8>),
      void Function(Pointer<Utf8>)>('sekejap_string_free');

  // ── 4.3 Documents ─────────────────────────────────────────────────────────

  late final put = library.lookupFunction<
      Int32 Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_put');

  late final putMany = library.lookupFunction<
      Long Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_put_many');

  late final get = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      Pointer<Utf8> Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_get');

  late final exists = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_exists');

  late final delete = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_delete');

  late final scanOpen = library.lookupFunction<
      Pointer<NativeScan> Function(Pointer<NativeDb>, Pointer<Utf8>, UintPtr),
      Pointer<NativeScan> Function(
          Pointer<NativeDb>, Pointer<Utf8>, int)>('sekejap_scan_open');

  late final scanNext = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeScan>),
      Pointer<Utf8> Function(Pointer<NativeScan>)>('sekejap_scan_next');

  late final scanClose = library.lookupFunction<
      Void Function(Pointer<NativeScan>),
      void Function(Pointer<NativeScan>)>('sekejap_scan_close');

  // ── 4.4 SQL ───────────────────────────────────────────────────────────────

  late final execute = library.lookupFunction<
      Long Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_execute');

  late final query = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      Pointer<Utf8> Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_query');

  late final explain = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      Pointer<Utf8> Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_explain');

  late final prepare = library.lookupFunction<
      Pointer<NativeStmt> Function(Pointer<NativeDb>, Pointer<Utf8>),
      Pointer<NativeStmt> Function(
          Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_prepare');

  late final stmtQuery = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeStmt>, Pointer<Utf8>),
      Pointer<Utf8> Function(
          Pointer<NativeStmt>, Pointer<Utf8>)>('sekejap_stmt_query');

  late final stmtExecute = library.lookupFunction<
      Long Function(Pointer<NativeStmt>, Pointer<Utf8>),
      int Function(Pointer<NativeStmt>, Pointer<Utf8>)>('sekejap_stmt_execute');

  late final stmtRebindable = library.lookupFunction<
      Int32 Function(Pointer<NativeStmt>),
      int Function(Pointer<NativeStmt>)>('sekejap_stmt_rebindable');

  late final stmtFree = library.lookupFunction<
      Void Function(Pointer<NativeStmt>),
      void Function(Pointer<NativeStmt>)>('sekejap_stmt_free');

  late final queryOpen = library.lookupFunction<
      Pointer<NativeScan> Function(
          Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>, UintPtr),
      Pointer<NativeScan> Function(Pointer<NativeDb>, Pointer<Utf8>,
          Pointer<Utf8>, int)>('sekejap_query_open');

  late final queryNext = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeScan>),
      Pointer<Utf8> Function(Pointer<NativeScan>)>('sekejap_query_next');

  late final queryClose = library.lookupFunction<
      Void Function(Pointer<NativeScan>),
      void Function(Pointer<NativeScan>)>('sekejap_query_close');

  // ── 4.5 Edges ─────────────────────────────────────────────────────────────

  late final link = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_link');

  late final linkWith = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>,
          Pointer<Utf8>,
          Pointer<Utf8>,
          Pointer<Utf8>,
          Pointer<Utf8>,
          Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_link_with');

  late final unlink = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_unlink');

  late final neighbours = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Int32, UintPtr),
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, int, int)>('sekejap_neighbours');

  // ── 4.6 The catalog ───────────────────────────────────────────────────────

  late final createCollection = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeDb>, Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_create_collection');

  late final dropCollection = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_drop_collection');

  late final collections = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>),
      Pointer<Utf8> Function(Pointer<NativeDb>)>('sekejap_collections');

  late final describe = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>),
      Pointer<Utf8> Function(
          Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_describe');

  late final countRows = library.lookupFunction<
      Long Function(Pointer<NativeDb>, Pointer<Utf8>),
      int Function(Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_count_rows');

  late final scanCountRows = library.lookupFunction<
      Long Function(Pointer<NativeDb>, Pointer<Utf8>),
      int Function(
          Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_scan_count_rows');

  late final scanCountEdges = library.lookupFunction<
      Long Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_scan_count_edges');

  // ── 4.7 Transactions ──────────────────────────────────────────────────────

  late final txBegin = library.lookupFunction<
      Pointer<NativeTx> Function(Pointer<NativeDb>),
      Pointer<NativeTx> Function(Pointer<NativeDb>)>('sekejap_tx_begin');

  late final txPut = library.lookupFunction<
      Int32 Function(
          Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_tx_put');

  late final txDelete = library.lookupFunction<
      Int32 Function(Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeTx>, Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_tx_delete');

  late final txLink = library.lookupFunction<
      Int32 Function(Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>,
          Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>)>('sekejap_tx_link');

  late final txExecute = library.lookupFunction<
      Long Function(Pointer<NativeTx>, Pointer<Utf8>, Pointer<Utf8>),
      int Function(Pointer<NativeTx>, Pointer<Utf8>,
          Pointer<Utf8>)>('sekejap_tx_execute');

  late final txCommit = library.lookupFunction<
      Int32 Function(Pointer<NativeTx>),
      int Function(Pointer<NativeTx>)>('sekejap_tx_commit');

  late final txRollback = library.lookupFunction<
      Int32 Function(Pointer<NativeTx>),
      int Function(Pointer<NativeTx>)>('sekejap_tx_rollback');

  // ── 4.8 Maintenance ───────────────────────────────────────────────────────

  late final checkpoint = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_checkpoint');

  late final publish = library.lookupFunction<Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_publish');

  late final storage = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>),
      Pointer<Utf8> Function(Pointer<NativeDb>)>('sekejap_storage');

  // ── 4.9 Service mode ──────────────────────────────────────────────────────

  late final statementTimeoutMs = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Uint64),
      int Function(Pointer<NativeDb>, int)>('sekejap_statement_timeout_ms');

  late final cancel = library.lookupFunction<Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_cancel');

  late final clearInterrupt = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_clear_interrupt');

  late final subscribe = library.lookupFunction<
      Long Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_subscribe');

  late final nextChange = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Long, Uint64),
      Pointer<Utf8> Function(
          Pointer<NativeDb>, int, int)>('sekejap_next_change');

  late final unsubscribe = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>, Long),
      int Function(Pointer<NativeDb>, int)>('sekejap_unsubscribe');

  // ── 4.10 Refused by name ──────────────────────────────────────────────────
  //
  // These four keep their symbol so the refusal arrives with a name and a
  // reason rather than as a missing-symbol error. All four always fail.

  late final openMemory = library.lookupFunction<Pointer<NativeDb> Function(),
      Pointer<NativeDb> Function()>('sekejap_open_memory');

  late final trimMemory = library.lookupFunction<
      Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_trim_memory');

  late final compact = library.lookupFunction<Int32 Function(Pointer<NativeDb>),
      int Function(Pointer<NativeDb>)>('sekejap_compact');

  late final show = library.lookupFunction<
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>),
      Pointer<Utf8> Function(Pointer<NativeDb>, Pointer<Utf8>)>('sekejap_show');
}
