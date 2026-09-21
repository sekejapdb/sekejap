/// The typed collection base.
///
/// A generated `<Entity>Collection` extends this, supplying the collection name,
/// columns, and (de)serialisers. It offers typed writes and multi-model query
/// starters (`where`, `near`, `matchText`, `rankByText`, `rankByVector`), each
/// returning a chainable [Query].
library;

import 'database.dart';
import 'filter.dart';
import 'query.dart';

/// Typed accessor for one collection. `T` is the entity, `C` its columns type.
abstract class Collection<T, C> {
  /// The owning database.
  Sekejap get store;

  /// The collection (table) name.
  String get collectionName;

  /// The generated columns object (the `d` in `(d) => d.field`).
  C get columns;

  /// Decode a stored payload into an entity.
  T Function(Map<String, dynamic> payload) get fromJson;

  /// Encode an entity to a stored payload.
  Map<String, dynamic> toJson(T entity);

  /// The primary-key value of an entity.
  String keyOf(T entity);

  /// A fresh query over this collection.
  Query<T, C> query() =>
      Query<T, C>(store.raw, collectionName, columns, fromJson);

  // ── query starters ──────────────────────────────────────────────────────────

  Query<T, C> where(Filter Function(C c) build) => query()..where(build);
  Query<T, C> sortBy(Col Function(C c) select, {bool desc = false}) =>
      query()..sortBy(select, desc: desc);
  Query<T, C> near(Col Function(C c) select, GeoPoint point,
          {required double metres}) =>
      query()..near(select, point, metres: metres);
  Query<T, C> matchText(Col Function(C c) select, String terms) =>
      query()..matchText(select, terms);
  Query<T, C> rankByText(Col Function(C c) select, String terms,
          {double weight = 1.0, bool normalized = true}) =>
      query()..rankByText(select, terms, weight: weight, normalized: normalized);
  Query<T, C> rankByVector(Col Function(C c) select, List<double> vector,
          {VectorMetric metric = VectorMetric.cosine, double weight = 1.0}) =>
      query()..rankByVector(select, vector, metric: metric, weight: weight);
  Query<T, C> limitTo(int n) => query()..limitTo(n);
  Query<T, C> get all => query();

  Future<List<T>> find() => query().find();
  Future<int> count() => query().count();

  // ── writes ──────────────────────────────────────────────────────────────────

  Future<void> put(T entity) =>
      store.putJson(collectionName, keyOf(entity), toJson(entity));

  Future<void> putAll(Iterable<T> entities) => store.putManyJson([
        for (final e in entities) ('$collectionName/${keyOf(e)}', toJson(e)),
      ]);

  Future<void> delete(String key) => store.deleteByKey(collectionName, key);

  Future<T?> get(String key) async {
    final json = await store.getJson(collectionName, key);
    return json == null ? null : fromJson(json);
  }
}
