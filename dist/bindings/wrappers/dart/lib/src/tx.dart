// The writer, held across many writes under ONE barrier.

import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'bindings.dart';
import 'db.dart';
import 'internal.dart';
import 'library.dart';

/// A transaction.
///
/// While one is open it HOLDS the writer: a call on the same [Db] that needs
/// the writer waits for it. Commit or roll back before using the database for
/// anything else.
///
/// [commit] and [rollback] both free the handle, whether or not they
/// succeeded. A transaction freed any other way -- including by [Db.close] --
/// ROLLS BACK, because committing on a stray free would make an abandoned
/// batch durable.
class Transaction {
  Transaction.internal(this.database, this._handle);

  /// The database this transaction borrows.
  final Db database;

  Pointer<NativeTx> _handle;
  bool _finished = false;

  /// Whether this transaction has been committed or rolled back.
  bool get isFinished => _finished;

  /// Write one document. NOT committed.
  ///
  /// A `_key` member in [document] must equal [key].
  void put(String collection, String key, Map<String, Object?> document) {
    _guard();
    using((arena) {
      final answer = sekejap.txPut(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
        jsonEncode(document).toNativeUtf8(allocator: arena),
      );
      if (answer != 0) throwLast(database.handle, 'tx_put');
    });
  }

  /// Delete one row. NOT committed. True if it was there.
  bool delete(String collection, String key) {
    _guard();
    return using((arena) {
      final answer = sekejap.txDelete(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(database.handle, 'tx_delete');
      return answer == 1;
    });
  }

  /// Link two rows with a typed edge. NOT committed. Both endpoints must
  /// already exist.
  void link(
    String fromCollection,
    String fromKey,
    String edgeType,
    String toCollection,
    String toKey,
  ) {
    _guard();
    using((arena) {
      final answer = sekejap.txLink(
        _handle,
        fromCollection.toNativeUtf8(allocator: arena),
        fromKey.toNativeUtf8(allocator: arena),
        edgeType.toNativeUtf8(allocator: arena),
        toCollection.toNativeUtf8(allocator: arena),
        toKey.toNativeUtf8(allocator: arena),
      );
      if (answer != 0) throwLast(database.handle, 'tx_link');
    });
  }

  /// Run one writing statement. NOT committed. Returns the rows it moved, in
  /// rows.
  int execute(String sql, [List<Object?>? params]) {
    _guard();
    return using((arena) {
      final rows = sekejap.txExecute(
        _handle,
        sql.toNativeUtf8(allocator: arena),
        paramsToNative(params, arena),
      );
      if (rows < 0) throwLast(database.handle, 'tx_execute');
      return rows;
    });
  }

  /// Commit, and free the handle whether or not the commit succeeded.
  void commit() {
    _guard();
    _finish();
    final answer = sekejap.txCommit(_handle);
    _handle = nullptr;
    if (answer != 0) throwLast(database.handle, 'tx_commit');
  }

  /// Roll back, and free the handle whether or not the roll back succeeded.
  void rollback() {
    _guard();
    _finish();
    final answer = sekejap.txRollback(_handle);
    _handle = nullptr;
    if (answer != 0) throwLast(database.handle, 'tx_rollback');
  }

  /// Roll back if this transaction is still open. What [Db.close] runs, and
  /// what a `try`/`finally` around a batch wants.
  void rollbackIfOpen() {
    if (_finished) return;
    rollback();
  }

  void _finish() {
    _finished = true;
    database.forgetTransaction(this);
  }

  void _guard() {
    if (_finished) {
      throw StateError(
          'this Transaction has been committed or rolled back; its handle is '
          'gone');
    }
    database.guardOpen();
  }
}
