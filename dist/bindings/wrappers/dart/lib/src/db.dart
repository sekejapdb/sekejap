// The database handle: one Dart class per C handle, one method per C
// function.

import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'bindings.dart';
import 'internal.dart';
import 'library.dart';
import 'scan.dart';
import 'statement.dart';
import 'status.dart';
import 'tx.dart';
import 'types.dart';

/// An open sekejap database.
///
/// Open one with [Db.open], [Db.openWithConfig] or [Db.openService], and
/// [close] it when you are done. `sekejap::Db` is `Send + Sync`, so one handle
/// MAY be used from several isolates; the handles derived from it -- [Scan],
/// [Statement], [Transaction] -- are each used from one at a time.
///
/// Every call outside a [Transaction] commits before it returns: durability
/// per call. A [Transaction] is the other bargain -- many writes, one barrier
/// -- and the two are the whole story.
class Db {
  Db._(this._handle, {required this.path, required this.service});

  Pointer<NativeDb> _handle;

  /// The directory this database was opened from, or null for a refused open.
  final String? path;

  /// Whether this handle was opened in SERVICE mode: one writer, parallel
  /// readers on a published snapshot, the change feed, the statement timeout
  /// and the cancel. The `subscribe`/`cancel`/`statementTimeout` family is
  /// REFUSED BY NAME on a handle that was not.
  final bool service;

  bool _closed = false;
  final Set<Statement> _statements = {};
  final Set<Scan> _scans = {};
  final Set<Transaction> _transactions = {};

  // ── 4.1 Opening and identity ──────────────────────────────────────────────

  /// Open the database in [path], creating it when the directory holds none.
  static Db open(String path) => using((arena) {
        final handle = sekejap.open(path.toNativeUtf8(allocator: arena));
        if (handle == nullptr) throwLast(nullptr, 'open');
        return Db._(handle, path: path, service: false);
      });

  /// [open] under a store [config].
  static Db openWithConfig(String path, StoreConfig config) => using((arena) {
        final handle = sekejap.openWithConfig(
          path.toNativeUtf8(allocator: arena),
          jsonEncode(config.toJson()).toNativeUtf8(allocator: arena),
        );
        if (handle == nullptr) throwLast(nullptr, 'open_with_config');
        return Db._(handle, path: path, service: false);
      });

  /// Open in SERVICE mode.
  static Db openService(String path) => using((arena) {
        final handle = sekejap.openService(path.toNativeUtf8(allocator: arena));
        if (handle == nullptr) throwLast(nullptr, 'open_service');
        return Db._(handle, path: path, service: true);
      });

  /// REFUSED: sekejap is disk-first and has no in-memory store. Always throws
  /// a [SekejapException] with [SekejapStatus.refused].
  ///
  /// The symbol is kept so the refusal arrives with a name and a reason
  /// rather than as a missing-symbol error. Give [open] a directory.
  static Db openMemory() {
    final handle = sekejap.openMemory();
    if (handle == nullptr) throwLast(nullptr, 'open_memory');
    return Db._(handle, path: null, service: false);
  }

  /// Close the handle. Uncommitted work is discarded: a close is not a
  /// commit.
  ///
  /// Every [Statement], [Scan] and [Transaction] still open on this database
  /// is closed FIRST, as the ABI requires; an open [Transaction] therefore
  /// rolls back. Calling [close] twice is harmless.
  void close() {
    if (_closed) return;
    for (final transaction in _transactions.toList()) {
      transaction.rollbackIfOpen();
    }
    for (final statement in _statements.toList()) {
      statement.close();
    }
    for (final scan in _scans.toList()) {
      scan.close();
    }
    _closed = true;
    sekejap.close(_handle);
    _handle = nullptr;
  }

  /// Whether [close] has already run.
  bool get isClosed => _closed;

  /// The library version, as `MAJOR.MINOR.PATCH`.
  ///
  /// Static program data on the C side: it is read, never freed.
  static String get version => sekejap.version().toDartString();

  /// The sekejap disk format this build reads and writes.
  static int get formatVersion => sekejap.formatVersion();

  // ── 4.2 Errors ────────────────────────────────────────────────────────────

  /// The message for the last failure ON THIS ISOLATE'S THREAD, or null after
  /// a success.
  ///
  /// The wrapper turns a failure into a [SekejapException] already; this is
  /// the raw slot, for a caller that wants to read it directly.
  static String? get lastError => messageOf(nullptr);

  /// The code for the last failure on this thread, [SekejapStatus.ok] after a
  /// success or a clean miss.
  static SekejapStatus get lastErrorCode => statusOf(nullptr);

  // ── 4.3 Documents ─────────────────────────────────────────────────────────

  /// Write one document, committed before this call returns.
  ///
  /// A `_key` member in [document] must equal [key]. A collection that is not
  /// in the catalog is a failure, not an implicit create: declare it with
  /// [createCollection] or `CREATE TABLE`.
  void put(String collection, String key, Map<String, Object?> document) {
    guardOpen();
    using((arena) {
      final answer = sekejap.put(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
        jsonEncode(document).toNativeUtf8(allocator: arena),
      );
      if (answer != 0) throwLast(_handle, 'put');
    });
  }

  /// Write many documents into one collection under ONE commit. Returns the
  /// rows written, in rows; a failure stores NONE of the batch.
  int putMany(String collection, Map<String, Map<String, Object?>> rows) {
    guardOpen();
    final payload = [
      for (final row in rows.entries) {'key': row.key, 'doc': row.value}
    ];
    return using((arena) {
      final written = sekejap.putMany(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        jsonEncode(payload).toNativeUtf8(allocator: arena),
      );
      if (written < 0) throwLast(_handle, 'put_many');
      return written;
    });
  }

  /// One document with `_key` set, or null for a MISS.
  ///
  /// A miss is not a failure: null here means there is no such row.
  Map<String, Object?>? get(String collection, String key) {
    guardOpen();
    return using((arena) {
      final document = sekejap.get(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
      );
      if (document == nullptr) {
        if (statusOf(_handle) != SekejapStatus.ok) throwLast(_handle, 'get');
        return null;
      }
      return decodeDocument(takeString(document)!);
    });
  }

  /// Whether the row is there.
  bool exists(String collection, String key) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.exists(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(_handle, 'exists');
      return answer == 1;
    });
  }

  /// Delete one row and every edge that touches it, committed. True if it was
  /// there.
  bool delete(String collection, String key) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.delete(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(_handle, 'delete');
      return answer == 1;
    });
  }

  /// Open a walk of one collection in stable id order, holding at most
  /// [pageRows] rows at a time (0 means sekejap's default of 256 rows).
  ///
  /// Close the [Scan] before the database.
  Scan scan(String collection, {int pageRows = 0}) {
    guardOpen();
    return using((arena) {
      final handle = sekejap.scanOpen(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        pageRows,
      );
      if (handle == nullptr) throwLast(_handle, 'scan_open');
      final walk = Scan.internal(this, handle, ScanPaging.collection);
      _scans.add(walk);
      return walk;
    });
  }

  // ── 4.4 SQL ───────────────────────────────────────────────────────────────

  /// Run one writing statement and commit. Returns the rows it moved, in
  /// rows; a statement that only raises a notice returns 0.
  ///
  /// A Tier-2/Tier-3 construct is REFUSED by name here, as a
  /// [SekejapException] with [SekejapStatus.refused] -- never as an empty
  /// answer.
  int execute(String sql, [List<Object?>? params]) {
    guardOpen();
    return using((arena) {
      final rows = sekejap.execute(
        _handle,
        sql.toNativeUtf8(allocator: arena),
        paramsToNative(params, arena),
      );
      if (rows < 0) throwLast(_handle, 'execute');
      return rows;
    });
  }

  /// Run one row-returning statement.
  ///
  /// A column MISSING in a row is omitted from that row's map, because
  /// missing is not null.
  List<Map<String, Object?>> query(String sql, [List<Object?>? params]) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.query(
        _handle,
        sql.toNativeUtf8(allocator: arena),
        paramsToNative(params, arena),
      );
      if (answer == nullptr) throwLast(_handle, 'query');
      return decodeRows(takeString(answer)!);
    });
  }

  /// The plan the engine would build for one statement.
  String explain(String sql, [List<Object?>? params]) {
    guardOpen();
    return using((arena) {
      final plan = sekejap.explain(
        _handle,
        sql.toNativeUtf8(allocator: arena),
        paramsToNative(params, arena),
      );
      if (plan == nullptr) throwLast(_handle, 'explain');
      return takeString(plan)!;
    });
  }

  /// Prepare one statement. It is PARSED here -- a syntax error is reported
  /// now -- and compiled by its first bind.
  ///
  /// Close the [Statement] before the database.
  Statement prepare(String sql) {
    guardOpen();
    return using((arena) {
      final handle =
          sekejap.prepare(_handle, sql.toNativeUtf8(allocator: arena));
      if (handle == nullptr) throwLast(_handle, 'prepare');
      final statement = Statement.internal(this, handle, sql);
      _statements.add(statement);
      return statement;
    });
  }

  /// Run a row-returning statement and open a PAGED DELIVERY of its answer:
  /// at most [pageRows] rows per call (0 means 4,096 rows).
  ///
  /// What this buys is a bounded string per call and the freedom to stop
  /// reading; it does not bound the answer, which is assembled here.
  Scan stream(String sql, [List<Object?>? params, int pageRows = 0]) {
    guardOpen();
    return using((arena) {
      final handle = sekejap.queryOpen(
        _handle,
        sql.toNativeUtf8(allocator: arena),
        paramsToNative(params, arena),
        pageRows,
      );
      if (handle == nullptr) throwLast(_handle, 'query_open');
      final walk = Scan.internal(this, handle, ScanPaging.answer);
      _scans.add(walk);
      return walk;
    });
  }

  // ── 4.5 Edges ─────────────────────────────────────────────────────────────

  /// Link two rows with a typed edge, committed. BOTH endpoints must already
  /// exist: a missing one is [SekejapStatus.unknownRow], never a dangling
  /// identity.
  ///
  /// [properties] rides along when it is given (`sekejap_link_with`).
  void link(
    String fromCollection,
    String fromKey,
    String edgeType,
    String toCollection,
    String toKey, {
    Map<String, Object?>? properties,
  }) {
    guardOpen();
    using((arena) {
      final from = fromCollection.toNativeUtf8(allocator: arena);
      final fromK = fromKey.toNativeUtf8(allocator: arena);
      final type = edgeType.toNativeUtf8(allocator: arena);
      final to = toCollection.toNativeUtf8(allocator: arena);
      final toK = toKey.toNativeUtf8(allocator: arena);
      final answer = properties == null
          ? sekejap.link(_handle, from, fromK, type, to, toK)
          : sekejap.linkWith(_handle, from, fromK, type, to, toK,
              jsonEncode(properties).toNativeUtf8(allocator: arena));
      if (answer != 0) {
        throwLast(_handle, properties == null ? 'link' : 'link_with');
      }
    });
  }

  /// Remove one edge, committed. True if it was there.
  bool unlink(
    String fromCollection,
    String fromKey,
    String edgeType,
    String toCollection,
    String toKey,
  ) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.unlink(
        _handle,
        fromCollection.toNativeUtf8(allocator: arena),
        fromKey.toNativeUtf8(allocator: arena),
        edgeType.toNativeUtf8(allocator: arena),
        toCollection.toNativeUtf8(allocator: arena),
        toKey.toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(_handle, 'unlink');
      return answer == 1;
    });
  }

  /// The rows one hop away, in one direction.
  ///
  /// [edgeType] null is every type. The answer is complete or refused under a
  /// bound of 256 edges: a [limit] above it is REFUSED by name, and a walk
  /// deeper than one hop is SQL's `GRAPH_TABLE`, not a second spelling here.
  List<Neighbour> neighbours(
    String collection,
    String key, {
    String? edgeType,
    SekejapDirection direction = SekejapDirection.outgoing,
    int limit = 256,
  }) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.neighbours(
        _handle,
        collection.toNativeUtf8(allocator: arena),
        key.toNativeUtf8(allocator: arena),
        textToNative(edgeType, arena),
        direction.code,
        limit,
      );
      if (answer == nullptr) throwLast(_handle, 'neighbours');
      return [
        for (final row in jsonDecode(takeString(answer)!) as List<Object?>)
          Neighbour.fromJson((row as Map).cast<String, Object?>())
      ];
    });
  }

  // ── 4.6 The catalog ───────────────────────────────────────────────────────

  /// Declare a collection. True if it was created, false if it was already
  /// there.
  ///
  /// The declaration is a floor, not a fence: a document may carry a field the
  /// declaration does not name, and it is stored in the row's extras.
  bool createCollection(String name, List<FieldSpec> fields) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.createCollection(
        _handle,
        name.toNativeUtf8(allocator: arena),
        jsonEncode([for (final f in fields) f.toJson()])
            .toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(_handle, 'create_collection');
      return answer == 1;
    });
  }

  /// Remove a collection, its rows, its indexes and its descriptor. True if
  /// it was there.
  bool dropCollection(String name) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.dropCollection(
        _handle,
        name.toNativeUtf8(allocator: arena),
      );
      if (answer < 0) throwLast(_handle, 'drop_collection');
      return answer == 1;
    });
  }

  /// Every collection name in the catalog, in key order.
  List<String> collections() {
    guardOpen();
    final answer = sekejap.collections(_handle);
    if (answer == nullptr) throwLast(_handle, 'collections');
    return [
      for (final name in jsonDecode(takeString(answer)!) as List<Object?>)
        name as String
    ];
  }

  /// The declared shape of one collection, or null when there is no such
  /// collection.
  CollectionDescription? describe(String collection) {
    guardOpen();
    return using((arena) {
      final answer = sekejap.describe(
        _handle,
        collection.toNativeUtf8(allocator: arena),
      );
      if (answer == nullptr) {
        if (statusOf(_handle) != SekejapStatus.ok) {
          throwLast(_handle, 'describe');
        }
        return null;
      }
      return CollectionDescription.fromJson(
          decodeDocument(takeString(answer)!));
    });
  }

  /// The rows of one collection, in rows, from the LIVE record when this
  /// database keeps one and from the walk when it does not.
  int countRows(String collection) {
    guardOpen();
    return using((arena) {
      final count = sekejap.countRows(
        _handle,
        collection.toNativeUtf8(allocator: arena),
      );
      if (count < 0) throwLast(_handle, 'count_rows');
      return count;
    });
  }

  /// The rows of one collection, in rows, BY WALKING them. The explicit walk,
  /// named as one.
  int scanCountRows(String collection) {
    guardOpen();
    return using((arena) {
      final count = sekejap.scanCountRows(
        _handle,
        collection.toNativeUtf8(allocator: arena),
      );
      if (count < 0) throwLast(_handle, 'scan_count_rows');
      return count;
    });
  }

  /// Every edge, in edges, BY WALKING the primary edge keyspace. sekejap
  /// keeps no O(1) edge counter, so this is a scan and is named as one.
  int scanCountEdges() {
    guardOpen();
    final count = sekejap.scanCountEdges(_handle);
    if (count < 0) throwLast(_handle, 'scan_count_edges');
    return count;
  }

  // ── 4.7 Transactions ──────────────────────────────────────────────────────

  /// Take the writer for many writes under ONE barrier.
  ///
  /// While the [Transaction] is open it holds the writer: a call on this
  /// database that needs the writer waits for it.
  Transaction transaction() {
    guardOpen();
    final handle = sekejap.txBegin(_handle);
    if (handle == nullptr) throwLast(_handle, 'tx_begin');
    final tx = Transaction.internal(this, handle);
    _transactions.add(tx);
    return tx;
  }

  // ── 4.8 Maintenance ───────────────────────────────────────────────────────

  /// Fold the committed write-ahead log into the data file.
  ///
  /// True when it folded, FALSE when a live reader holds a slot and the fold
  /// is DEFERRED -- which in service mode is every call. Deferred is not a
  /// failure.
  bool checkpoint() {
    guardOpen();
    final answer = sekejap.checkpoint(_handle);
    if (answer < 0) throwLast(_handle, 'checkpoint');
    return answer == 1;
  }

  /// Make the newest commit visible to readers now.
  ///
  /// In single mode there is no published view to swap and every commit is
  /// already visible to this handle, so this succeeds having done nothing.
  void publish() {
    guardOpen();
    if (sekejap.publish(_handle) != 0) throwLast(_handle, 'publish');
  }

  /// The bytes on disk.
  StorageBytes storage() {
    guardOpen();
    final answer = sekejap.storage(_handle);
    if (answer == nullptr) throwLast(_handle, 'storage');
    return StorageBytes.fromJson(decodeDocument(takeString(answer)!));
  }

  // ── 4.9 Service mode ──────────────────────────────────────────────────────

  /// Refuse a statement that runs longer than [milliseconds]. 0 milliseconds
  /// CLEARS the timeout.
  ///
  /// REFUSED BY NAME on a handle that was not opened with [Db.openService]:
  /// single mode has no writer to time out.
  void statementTimeoutMs(int milliseconds) {
    guardOpen();
    if (sekejap.statementTimeoutMs(_handle, milliseconds) != 0) {
      throwLast(_handle, 'statement_timeout_ms');
    }
  }

  /// Cancel the work in flight on this service, from any thread. STICKY until
  /// [clearInterrupt]. Service mode only.
  void cancel() {
    guardOpen();
    if (sekejap.cancel(_handle) != 0) throwLast(_handle, 'cancel');
  }

  /// Clear a cancel so the service accepts work again. True when one was
  /// standing. Service mode only.
  bool clearInterrupt() {
    guardOpen();
    final answer = sekejap.clearInterrupt(_handle);
    if (answer < 0) throwLast(_handle, 'clear_interrupt');
    return answer == 1;
  }

  /// Subscribe to the commit-time change feed, returning the subscription id.
  /// Service mode only.
  ///
  /// A subscriber's queue is bounded and the service never waits for a slow
  /// one: a subscription that falls behind loses events, and
  /// [ChangeEvent.sequence] is what tells a listener how many. A subscription
  /// left open is closed by [close].
  int subscribe() {
    guardOpen();
    final id = sekejap.subscribe(_handle);
    if (id < 0) throwLast(_handle, 'subscribe');
    return id;
  }

  /// The next change event for one [subscription], or null when none arrived
  /// within [timeoutMs] milliseconds. 0 milliseconds polls and returns at
  /// once. Service mode only.
  ChangeEvent? nextChange(int subscription, {int timeoutMs = 0}) {
    guardOpen();
    final answer = sekejap.nextChange(_handle, subscription, timeoutMs);
    if (answer == nullptr) {
      if (statusOf(_handle) != SekejapStatus.ok) {
        throwLast(_handle, 'next_change');
      }
      return null;
    }
    return ChangeEvent.fromJson(decodeDocument(takeString(answer)!));
  }

  /// Close one subscription. True when it was open. Service mode only.
  bool unsubscribe(int subscription) {
    guardOpen();
    final answer = sekejap.unsubscribe(_handle, subscription);
    if (answer < 0) throwLast(_handle, 'unsubscribe');
    return answer == 1;
  }

  // ── 4.10 Refused by name ──────────────────────────────────────────────────

  /// REFUSED: there is nothing proportional to rows held in memory to trim.
  /// Always throws [SekejapStatus.refused].
  void trimMemory() {
    guardOpen();
    if (sekejap.trimMemory(_handle) != 0) throwLast(_handle, 'trim_memory');
  }

  /// REFUSED: there is no payload-rewriting compaction. Always throws
  /// [SekejapStatus.refused]. [checkpoint] folds the write-ahead log into the
  /// data file; it does not rewrite rows.
  void compact() {
    guardOpen();
    if (sekejap.compact(_handle) != 0) throwLast(_handle, 'compact');
  }

  /// REFUSED: the `SHOW` family has no Tier-1 spelling. Always throws
  /// [SekejapStatus.refused]. [collections] and [describe] answer the same
  /// questions as DATA.
  String show(String statement) {
    guardOpen();
    return using((arena) {
      final answer =
          sekejap.show(_handle, statement.toNativeUtf8(allocator: arena));
      if (answer == nullptr) throwLast(_handle, 'show');
      return takeString(answer)!;
    });
  }

  // ── Internals the derived handles use ─────────────────────────────────────

  /// The raw handle, for the error slot. Not part of the public contract.
  Pointer<NativeDb> get handle => _handle;

  /// Throw if the database has been closed.
  void guardOpen() {
    if (_closed) throw StateError('this Db is closed');
  }

  /// Forget a [Statement] that closed itself.
  void forgetStatement(Statement statement) => _statements.remove(statement);

  /// Forget a [Scan] that closed itself.
  void forgetScan(Scan scan) => _scans.remove(scan);

  /// Forget a [Transaction] that committed or rolled back.
  void forgetTransaction(Transaction tx) => _transactions.remove(tx);

  @override
  String toString() => 'Db(${path ?? "<refused>"}${service ? ", service" : ""}'
      '${_closed ? ", closed" : ""})';
}
