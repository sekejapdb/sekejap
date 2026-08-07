/// Annotations for sekejap's typed model layer.
///
/// Mark a class with [SekejapEntity] and run `dart run build_runner build` to
/// generate a typed collection, column references, and (de)serializers. The
/// generated code lowers to the same SGQL the engine already runs — it is a
/// typed front-end, not a second engine.
library;

/// Marks a class as a stored entity (a collection). Codegen produces a typed
/// `db.<plural>` accessor, a columns object for `where`/`sortBy`, JSON
/// (de)serializers, and a schema descriptor.
class SekejapEntity {
  /// Collection name. Defaults to the lower-cased class name (`Dish` → `dish`).
  final String? collection;
  const SekejapEntity({this.collection});
}

/// Marks the primary-key field. Exactly one per entity. Maps to `_key`.
class Key {
  const Key();
}

/// Secondary index kind for a scalar field. `btree` supports equality and
/// range; `hash` is equality-only but faster for it.
enum IndexKind { btree, hash }

/// Marks a field for a scalar index, so filters on it use the index instead of
/// a scan. Defaults to a btree (equality + range); pass [IndexKind.hash] for an
/// equality-only column.
class Index {
  final IndexKind kind;
  const Index([this.kind = IndexKind.btree]);
}

/// Marks a `List<double>` field as a vector of the given dimension (for ANN
/// search). Reserved for the multi-model query surface.
class Vector {
  final int dim;
  const Vector(this.dim);
}

/// Marks a `GeoPoint`/geometry field as spatial. Reserved for the multi-model
/// query surface.
class Geo {
  const Geo();
}

/// Marks a text field for BM25 full-text ranking. Reserved for the multi-model
/// query surface.
class Bm25 {
  const Bm25();
}

/// Convenience singletons.
const key = Key();
const index = Index();
const geo = Geo();
const bm25 = Bm25();
