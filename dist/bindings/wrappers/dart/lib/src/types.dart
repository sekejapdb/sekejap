// The JSON shapes of `docs/dist/C_ABI.md` §2, as Dart types.
//
// A document, a parameter list and a row stay `Map<String, Object?>` and
// `List<Object?>` -- Dart's own JSON types, which is the cheap rendering the
// wrapper brief asks for. The shapes that are the ABI's own vocabulary -- a
// field declaration, a collection description, a neighbour, the bytes on
// disk, a change event -- get a class, because a caller reads their members by
// name and a typo in a map key is a run-time surprise.

/// The kind of a declared field.
enum FieldKind {
  text('text'),
  integer('int'),
  real('real'),
  boolean('bool'),
  json('json'),
  geo('geo'),
  point('point'),
  vector('vector');

  const FieldKind(this.wire);

  /// The spelling the ABI uses.
  final String wire;

  /// The kind for a spelling the library returned.
  static FieldKind fromWire(String wire) => FieldKind.values
      .firstWhere((k) => k.wire == wire, orElse: () => FieldKind.json);
}

/// One field of a collection declaration, for `Db.createCollection`.
class FieldSpec {
  /// A field of [kind]. [dimension] is required for [FieldKind.vector] and
  /// rejected for every other kind.
  const FieldSpec(this.name, this.kind, {this.dimension});

  /// A `VECTOR(components)` field.
  const FieldSpec.vector(this.name, int components)
      : kind = FieldKind.vector,
        dimension = components;

  final String name;
  final FieldKind kind;

  /// The number of components of a vector field, in components.
  final int? dimension;

  /// The `{"name", "kind", "dimension"?}` object the ABI takes.
  Map<String, Object?> toJson() => {
        'name': name,
        'kind': kind.wire,
        if (dimension != null) 'dimension': dimension,
      };
}

/// A field as the catalog holds it, from `Db.describe`.
class FieldInfo {
  FieldInfo.fromJson(Map<String, Object?> json)
      : name = json['name'] as String,
        kind = FieldKind.fromWire(json['kind'] as String),
        dimension = json['dimension'] as int?,
        declared = json['declared'] as String?,
        primaryKey = json['primary_key'] as bool? ?? false;

  final String name;
  final FieldKind kind;

  /// The components of a vector field, in components; null for every other
  /// kind.
  final int? dimension;

  /// The SQL spelling the catalog recorded where the kind does not carry it
  /// (`TIMESTAMPTZ` and `DATE` are both `int`), or null.
  final String? declared;

  final bool primaryKey;

  @override
  String toString() => 'FieldInfo($name, ${kind.wire}'
      '${dimension == null ? '' : '($dimension)'}'
      '${primaryKey ? ', primary key' : ''})';
}

/// An index as the catalog holds it, from `Db.describe`.
class IndexInfo {
  IndexInfo.fromJson(Map<String, Object?> json)
      : name = json['name'] as String,
        field = json['field'] as String,
        family = json['family'] as String,
        unique = json['unique'] as bool? ?? false,
        ready = json['ready'] as bool? ?? false;

  final String name;
  final String field;

  /// One of `scalar`, `text`, `exact_vector`, `quantized_vector`,
  /// `spatial_point`, `spatial_geometry`.
  final String family;

  final bool unique;
  final bool ready;

  @override
  String toString() => 'IndexInfo($name on $field, $family'
      '${unique ? ', unique' : ''}${ready ? '' : ', not ready'})';
}

/// The declared shape of one collection, from `Db.describe`.
class CollectionDescription {
  CollectionDescription.fromJson(Map<String, Object?> json)
      : name = json['name'] as String,
        timestamps = json['timestamps'] as bool? ?? false,
        rows = json['rows'] as int?,
        fields = [
          for (final f in (json['fields'] as List<Object?>? ?? const []))
            FieldInfo.fromJson(f as Map<String, Object?>)
        ],
        indexes = [
          for (final i in (json['indexes'] as List<Object?>? ?? const []))
            IndexInfo.fromJson(i as Map<String, Object?>)
        ];

  final String name;
  final bool timestamps;

  /// The live row count in rows, or null where this database keeps no record
  /// for the collection. Null is "no record", not "no rows": the number then
  /// costs a walk, which is `Db.scanCountRows`.
  final int? rows;

  final List<FieldInfo> fields;
  final List<IndexInfo> indexes;

  @override
  String toString() => 'CollectionDescription($name, '
      '${fields.length} fields, ${indexes.length} indexes, rows=$rows)';
}

/// One row a hop away, from `Db.neighbours`. The collection is part of the
/// answer because a neighbour can be in another one.
class Neighbour {
  Neighbour.fromJson(Map<String, Object?> json)
      : collection = json['collection'] as String,
        key = json['key'] as String,
        document = (json['document'] as Map).cast<String, Object?>();

  final String collection;
  final String key;
  final Map<String, Object?> document;

  @override
  String toString() => 'Neighbour($collection/$key)';
}

/// The bytes on disk, from `Db.storage`.
class StorageBytes {
  StorageBytes.fromJson(Map<String, Object?> json)
      : dataBytes = json['data_bytes'] as int,
        walBytes = json['wal_bytes'] as int,
        totalBytes = json['total_bytes'] as int;

  /// The data file, in bytes.
  final int dataBytes;

  /// The write-ahead log, in bytes.
  final int walBytes;

  /// Both, in bytes.
  final int totalBytes;

  @override
  String toString() =>
      'StorageBytes(data $dataBytes bytes, wal $walBytes bytes, '
      'total $totalBytes bytes)';
}

/// What a write did to one key, inside a [ChangeEvent].
class ChangedKey {
  ChangedKey.fromJson(Map<String, Object?> json)
      : collectionId = json['collection'] as int,
        key = json['key'] as String,
        deleted = json['kind'] == 'delete';

  /// The collection's NUMERIC id, not its name: the feed carries ids, and the
  /// C ABI has no call that resolves one to a name (see the README, "Gaps").
  final int collectionId;

  final String key;

  /// True for a delete, false for a put.
  final bool deleted;

  @override
  String toString() =>
      'ChangedKey($collectionId/$key, ${deleted ? 'delete' : 'put'})';
}

/// One event of the commit-time change feed, from `Db.nextChange`.
class ChangeEvent {
  ChangeEvent.fromJson(Map<String, Object?> json)
      : sequence = json['sequence'] as int,
        collectionIds = [
          for (final c in (json['collections'] as List<Object?>? ?? const []))
            c as int
        ],
        edgeTypeIds = [
          for (final e in (json['edge_types'] as List<Object?>? ?? const []))
            e as int
        ],
        keys = [
          for (final k in (json['keys'] as List<Object?>? ?? const []))
            ChangedKey.fromJson(k as Map<String, Object?>)
        ],
        keysTotal = json['keys_total'] as int? ?? 0,
        keysTruncated = json['keys_truncated'] as bool? ?? false,
        unnamedWrites = json['unnamed_writes'] as int? ?? 0,
        rowsAffected = json['rows_affected'] as int? ?? 0;

  /// The commit sequence. A gap between two events is exactly how many events
  /// a slow subscriber lost.
  final int sequence;

  /// Every collection the batch touched, by NUMERIC id. Exact even when
  /// [keysTruncated]. The feed carries ids, not names.
  final List<int> collectionIds;

  /// Every edge type the batch touched, by numeric id.
  final List<int> edgeTypeIds;

  /// The keys the batch moved, in keys. Empty when [keysTruncated].
  final List<ChangedKey> keys;

  /// How many keys the batch moved, in keys.
  final int keysTotal;

  /// True when the batch moved more keys than the feed's per-event cap of
  /// 1,024 keys: the list is dropped whole rather than handed over half-true.
  final bool keysTruncated;

  /// Writes the feed could not attribute to a collection, in writes: SQL DML
  /// and DDL, and a write run through a prepared statement handle. A listener
  /// that sees this above zero re-runs its query.
  final int unnamedWrites;

  /// The rows the batch moved, in rows.
  final int rowsAffected;

  @override
  String toString() => 'ChangeEvent(sequence $sequence, '
      '${collectionIds.length} collections, $keysTotal keys, '
      '$rowsAffected rows)';
}

/// How the store writes, for `Db.openWithConfig`. Every member is optional and
/// an absent one keeps sekejap's own default, which is [SyncMode.full].
class StoreConfig {
  const StoreConfig({this.budgetBytes, this.io, this.sync});

  /// The buffer pool ceiling, in bytes.
  final int? budgetBytes;

  final IoMode? io;
  final SyncMode? sync;

  Map<String, Object?> toJson() => {
        if (budgetBytes != null) 'budget_bytes': budgetBytes,
        if (io != null) 'io': io!.wire,
        if (sync != null) 'sync': sync!.wire,
      };
}

/// How the store reaches the medium.
enum IoMode {
  buffered('buffered'),
  direct('direct');

  const IoMode(this.wire);
  final String wire;
}

/// How often the write-ahead log is flushed to the medium.
enum SyncMode {
  full('full'),
  normal('normal'),
  off('off');

  const SyncMode(this.wire);
  final String wire;
}
