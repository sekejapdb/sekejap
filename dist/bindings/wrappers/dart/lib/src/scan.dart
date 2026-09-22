// A paged walk: of one collection (`sekejap_scan_open`) or of one statement's
// answer (`sekejap_query_open`). One page per call, so no single string holds
// the whole answer.

import 'dart:ffi';

import 'bindings.dart';
import 'db.dart';
import 'internal.dart';
import 'library.dart';
import 'status.dart';

/// A walk that hands back one page per call.
///
/// A scan borrows its [Db]: close it before the database. [Db.close] closes
/// any that are still open rather than leaving a dangling pointer behind.
///
/// Used from ONE isolate at a time, which is the ABI's rule for a derived
/// handle.
class Scan {
  Scan.internal(this.database, this._handle, this._paging);

  /// The database this walk borrows.
  final Db database;

  Pointer<NativeScan> _handle;

  /// What opened it: a collection walk, or a statement's paged answer. The
  /// two are the same operation under two names, and the wrapper calls the
  /// name that matches the open.
  final ScanPaging _paging;

  bool _closed = false;

  /// Whether [close] has already run.
  bool get isClosed => _closed;

  /// The next page, or null at the END of the walk.
  ///
  /// A null with a failure status is a failure and throws; a null with
  /// [SekejapStatus.ok] is the end, because a miss is not a failure.
  List<Map<String, Object?>>? nextPage() {
    _guard();
    final page = _paging == ScanPaging.collection
        ? sekejap.scanNext(_handle)
        : sekejap.queryNext(_handle);
    if (page == nullptr) {
      if (statusOf(database.handle) != SekejapStatus.ok) {
        throwLast(database.handle, 'scan_next');
      }
      return null;
    }
    return decodeRows(takeString(page)!);
  }

  /// Every remaining page, one call per page.
  Iterable<List<Map<String, Object?>>> pages() sync* {
    while (true) {
      final page = nextPage();
      if (page == null) return;
      yield page;
    }
  }

  /// Every remaining row, flattened across the pages.
  Iterable<Map<String, Object?>> rows() sync* {
    for (final page in pages()) {
      yield* page;
    }
  }

  /// Close the walk and free it. Calling it twice is harmless.
  void close() {
    if (_closed) return;
    _closed = true;
    database.forgetScan(this);
    if (_paging == ScanPaging.collection) {
      sekejap.scanClose(_handle);
    } else {
      sekejap.queryClose(_handle);
    }
    _handle = nullptr;
  }

  void _guard() {
    if (_closed) throw StateError('this Scan is closed');
    database.guardOpen();
  }
}

/// Which open made a [Scan], so the matching close is the one that runs.
enum ScanPaging {
  /// `sekejap_scan_open` / `_next` / `_close`.
  collection,

  /// `sekejap_query_open` / `_next` / `_close`.
  answer,
}
