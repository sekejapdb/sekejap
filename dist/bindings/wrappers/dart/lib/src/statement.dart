// One statement, parsed once at `sekejap_prepare` and compiled by its first
// bind.

import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'bindings.dart';
import 'db.dart';
import 'internal.dart';
import 'library.dart';
import 'status.dart';

/// A prepared statement.
///
/// A statement borrows its [Db]: close it before the database. [Db.close]
/// closes any that are still open.
///
/// Used from ONE isolate at a time.
class Statement {
  Statement.internal(this.database, this._handle, this.sql);

  /// The database this statement borrows.
  final Db database;

  /// The text it was prepared from.
  final String sql;

  Pointer<NativeStmt> _handle;
  bool _closed = false;

  /// Whether [close] has already run.
  bool get isClosed => _closed;

  /// Run it as a row-returning statement.
  ///
  /// A column MISSING in a row is omitted from that row's map, because
  /// missing is not null.
  List<Map<String, Object?>> query([List<Object?>? params]) {
    _guard();
    return using((arena) {
      final answer = sekejap.stmtQuery(_handle, paramsToNative(params, arena));
      if (answer == nullptr) throwLast(database.handle, 'stmt_query');
      return decodeRows(takeString(answer)!);
    });
  }

  /// Run it as a writing statement and commit. Returns the rows it moved, in
  /// rows.
  int execute([List<Object?>? params]) {
    _guard();
    return using((arena) {
      final rows = sekejap.stmtExecute(_handle, paramsToNative(params, arena));
      if (rows < 0) throwLast(database.handle, 'stmt_execute');
      return rows;
    });
  }

  /// Whether a further bind compiles nothing.
  ///
  /// [Rebindable.no] for every WRITING statement, because a write folds its
  /// document at compile; [Rebindable.unbound] before the first bind.
  Rebindable get rebindable {
    _guard();
    final answer = sekejap.stmtRebindable(_handle);
    switch (answer) {
      case 1:
        return Rebindable.yes;
      case 0:
        return Rebindable.no;
      case 2: // SEKEJAP_REBIND_UNBOUND
        return Rebindable.unbound;
      default:
        throwLast(database.handle, 'stmt_rebindable');
    }
  }

  /// Free the statement. Calling it twice is harmless.
  void close() {
    if (_closed) return;
    _closed = true;
    database.forgetStatement(this);
    sekejap.stmtFree(_handle);
    _handle = nullptr;
  }

  void _guard() {
    if (_closed) throw StateError('this Statement is closed');
    database.guardOpen();
  }
}
