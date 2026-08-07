/// The high-level typed database handle.
///
/// [Sekejap.open] initialises the native library, opens the store, and applies
/// each entity's schema. Generated code adds typed `db.<collection>` accessors
/// as extensions on [Sekejap].
library;

import 'dart:convert';

import '../../sekejap.dart' show initSekejap;
import '../rust/api/simple.dart';

/// A generated entity's schema descriptor: the collection name, its
/// `CREATE TABLE` statement, and any index statements (btree/spatial/hnsw/bm25),
/// all applied on open.
class EntitySchema {
  final String collection;
  final String createTableSql;
  final List<String> indexSql;
  const EntitySchema(this.collection, this.createTableSql,
      {this.indexSql = const []});
}

/// A typed sekejap database. Hold one per store; obtain typed collections via
/// the generated extensions (e.g. `db.dishes`).
class Sekejap {
  /// The underlying native handle. Generated collections use this directly.
  final SekejapDb raw;

  Sekejap._(this.raw);

  /// Open a persistent database at [path] and apply [schema]. In a Flutter app
  /// omit [libraryPath] (the bundled library is found automatically); in
  /// standalone Dart/tests pass a built `libsekejap_ffi.{dylib,so,dll}`.
  static Future<Sekejap> open(
    String path, {
    List<EntitySchema> schema = const [],
    String? libraryPath,
  }) async {
    await initSekejap(libraryPath: libraryPath);
    final db = await dbOpen(path: path);
    return _applySchema(Sekejap._(db), schema);
  }

  /// Open an in-memory (non-persistent) database and apply [schema].
  static Future<Sekejap> openInMemory({
    List<EntitySchema> schema = const [],
    String? libraryPath,
  }) async {
    await initSekejap(libraryPath: libraryPath);
    return _applySchema(Sekejap._(await dbNew()), schema);
  }

  static Future<Sekejap> _applySchema(
      Sekejap s, List<EntitySchema> schema) async {
    for (final entity in schema) {
      // Schema application is idempotent from the caller's view: on reopen the
      // collection already exists, so a failed CREATE TABLE is expected and
      // ignored. (The engine has no `CREATE TABLE IF NOT EXISTS`.)
      try {
        await dbExecute(db: s.raw, sql: entity.createTableSql);
      } catch (_) {
        // already created on a previous open
      }
      for (final idx in entity.indexSql) {
        try {
          await dbExecute(db: s.raw, sql: idx);
        } catch (_) {
          // index already present
        }
      }
    }
    return s;
  }

  /// Store a typed entity's JSON payload under `collection/key`.
  Future<void> putJson(String collection, String key, Map<String, dynamic> json) =>
      dbPut(db: raw, slug: '$collection/$key', json: jsonEncode(json))
          .then((_) {});

  /// Fetch one entity's decoded JSON payload by collection and key, or null.
  /// Keeps `dart:convert` out of generated part files.
  Future<Map<String, dynamic>?> getJson(String collection, String key) async {
    final raw0 = await dbGet(db: raw, slug: '$collection/$key');
    if (raw0 == null) return null;
    return (jsonDecode(raw0) as Map).cast<String, dynamic>();
  }

  /// Store many entities in one native batch (one durability barrier).
  Future<void> putManyJson(List<(String slug, Map<String, dynamic> json)> items) =>
      dbPutMany(
        db: raw,
        pairs: [for (final (slug, json) in items) (slug, jsonEncode(json))],
      ).then((_) {});

  /// Delete an entity by collection and key.
  Future<void> deleteByKey(String collection, String key) async =>
      dbRemove(db: raw, slug: '$collection/$key');

  /// Flush the WAL and write a snapshot (call at an idle moment on mobile).
  Future<void> compact() => dbCompact(db: raw);
}
