/// The typed, multi-model query builder base.
///
/// Parameterised by the entity type `T` and its generated columns type `C`, so
/// every builder method — scalar `where`/`sortBy`, spatial `near`, text
/// `matchText`, and vector/text `rankBy…` — is typed against the entity's
/// columns. A query lowers to a single SGQL statement (scalar predicates as
/// `$1`, `$2` bindings; spatial/vector/text as inline literals) and maps result
/// payloads back to `T`. `.find()` runs it once; `.watch()` re-runs it whenever
/// a committed change touches the collection.
library;

import 'dart:async';
import 'dart:convert';

import '../rust/api/simple.dart';
import '../watch.dart';
import 'filter.dart';

/// A WGS84 longitude/latitude point for spatial predicates.
class GeoPoint {
  final double lon;
  final double lat;
  const GeoPoint(this.lon, this.lat);
  String toSql() => 'POINT($lon $lat)';
}

/// Vector distance/similarity used for ranking. `cosine` and `dot` are
/// similarity-like (higher is nearer, ranked DESC).
enum VectorMetric {
  cosine('VECTOR_COSINE'),
  dot('VECTOR_DOT');

  final String fn;
  const VectorMetric(this.fn);
}

/// A composable, typed, multi-model query over one collection producing
/// `List<T>`. `C` is the generated columns type (the `d` in `(d) => d.field`).
class Query<T, C> {
  final SekejapDb db;
  final String collection;
  final C columns;
  final T Function(Map<String, dynamic> payload) fromJson;

  Filter? _where;
  final List<String> _extraWhere = []; // spatial/text predicates (inline literals)
  String? _orderBy; // scalar ORDER BY field
  bool _desc = false;
  final List<String> _rankTerms = []; // weighted score terms → ORDER BY sum DESC
  int? _limit;
  int? _offset;

  Query(this.db, this.collection, this.columns, this.fromJson);

  // ── scalar ────────────────────────────────────────────────────────────────

  Query<T, C> where(Filter Function(C c) build) {
    final f = build(columns);
    _where = _where == null ? f : _where! & f;
    return this;
  }

  Query<T, C> sortBy(Col Function(C c) select, {bool desc = false}) {
    _orderBy = select(columns).name;
    _desc = desc;
    return this;
  }

  Query<T, C> limitTo(int n) {
    _limit = n;
    return this;
  }

  Query<T, C> offsetBy(int n) {
    _offset = n;
    return this;
  }

  // ── spatial ─────────────────────────────────────────────────────────────────

  /// Keep rows whose [select] geometry lies within [metres] of [point].
  Query<T, C> near(Col Function(C c) select, GeoPoint point,
      {required double metres}) {
    final col = select(columns).name;
    _extraWhere.add('ST_DWithin($col, ${point.toSql()}, $metres)');
    return this;
  }

  // ── text ────────────────────────────────────────────────────────────────────

  /// Keep rows whose [select] text field matches [terms] (BM25 score > 0).
  Query<T, C> matchText(Col Function(C c) select, String terms) {
    final col = select(columns).name;
    _extraWhere.add("BM25($col, ${_str(terms)}) > 0.0");
    return this;
  }

  /// Add a BM25 relevance term to the ranking (normalised 0–1 by default).
  Query<T, C> rankByText(Col Function(C c) select, String terms,
      {double weight = 1.0, bool normalized = true}) {
    final col = select(columns).name;
    final fn = normalized ? 'BM25_NORM' : 'BM25';
    _rankTerms.add('$fn($col, ${_str(terms)}) * $weight');
    return this;
  }

  // ── vector ──────────────────────────────────────────────────────────────────

  /// Add a vector-similarity term to the ranking.
  Query<T, C> rankByVector(Col Function(C c) select, List<double> query,
      {VectorMetric metric = VectorMetric.cosine, double weight = 1.0}) {
    final col = select(columns).name;
    _rankTerms.add('${metric.fn}($col, ${_vec(query)}) * $weight');
    return this;
  }

  // ── lowering ────────────────────────────────────────────────────────────────

  /// Top-level AND clauses, flattened so a range like `value >= x AND value <= y`
  /// stays a flat conjunction (no wrapping parentheses) — the form the engine's
  /// btree range-scan detection recognises. Nested ORs keep their parentheses.
  List<String> _whereClauses(SqlContext ctx) {
    final clauses = <String>[];
    void flatten(Filter f) {
      if (f is AndFilter) {
        flatten(f.a);
        flatten(f.b);
      } else {
        clauses.add(f.render(ctx));
      }
    }

    if (_where != null) flatten(_where!);
    clauses.addAll(_extraWhere);
    return clauses;
  }

  /// The SGQL statement and its JSON-encoded scalar parameter list.
  (String sql, String paramsJson) build({String projection = '*'}) {
    final ctx = SqlContext();
    final sb = StringBuffer('SELECT $projection FROM $collection');

    final clauses = _whereClauses(ctx);
    if (clauses.isNotEmpty) sb.write(' WHERE ${clauses.join(' AND ')}');

    if (_rankTerms.isNotEmpty) {
      sb.write(' ORDER BY ${_rankTerms.join(' + ')} DESC');
    } else if (_orderBy != null) {
      sb.write(' ORDER BY $_orderBy ${_desc ? 'DESC' : 'ASC'}');
    }
    if (_limit != null) sb.write(' LIMIT $_limit');
    if (_offset != null) sb.write(' OFFSET $_offset');
    return (sb.toString(), jsonEncode(ctx.params));
  }

  /// Apply [assignments] (`column: value`) to every matching row. Lowers to
  /// `UPDATE … SET … WHERE …`. Returns the number of rows changed.
  Future<int> update(Map<String, Object?> assignments) async {
    final ctx = SqlContext();
    final sets = [
      for (final e in assignments.entries)
        '${e.key} = ${ctx.placeholder(e.value)}'
    ].join(', ');
    final sb = StringBuffer('UPDATE $collection SET $sets');
    final clauses = _whereClauses(ctx);
    if (clauses.isNotEmpty) sb.write(' WHERE ${clauses.join(' AND ')}');
    final n = await dbExecuteParams(
        db: db, sql: sb.toString(), paramsJson: jsonEncode(ctx.params));
    return n.toInt();
  }

  /// Delete every matching row. Lowers to `DELETE FROM … WHERE …`. Returns the
  /// number of rows deleted.
  Future<int> deleteAll() async {
    final ctx = SqlContext();
    final sb = StringBuffer('DELETE FROM $collection');
    final clauses = _whereClauses(ctx);
    if (clauses.isNotEmpty) sb.write(' WHERE ${clauses.join(' AND ')}');
    final n = await dbExecuteParams(
        db: db, sql: sb.toString(), paramsJson: jsonEncode(ctx.params));
    return n.toInt();
  }

  /// Run the query once.
  Future<List<T>> find() async {
    final (sql, params) = build();
    final raw = await dbQueryParams(db: db, sql: sql, paramsJson: params);
    final rows = jsonDecode(raw) as List;
    return [
      for (final row in rows)
        fromJson((row['payload'] as Map).cast<String, dynamic>())
    ];
  }

  /// Run the query and return the first row, or null.
  Future<T?> findFirst() async {
    final saved = _limit;
    _limit = 1;
    final rows = await find();
    _limit = saved;
    return rows.isEmpty ? null : rows.first;
  }

  /// Count matching rows (scalar + spatial/text predicates).
  Future<int> count() async {
    final (sql, params) = build(projection: 'COUNT(*) AS n');
    final raw = await dbQueryParams(db: db, sql: sql, paramsJson: params);
    final rows = jsonDecode(raw) as List;
    if (rows.isEmpty) return 0;
    return ((rows.first['payload'] as Map)['n'] as num).toInt();
  }

  /// Reactive results: the current list now, then a fresh list each time a
  /// committed change touches this collection. Cancelling releases the native
  /// listener.
  Stream<List<T>> watch() {
    StreamSubscription<ChangeEvent>? sub;
    late StreamController<List<T>> controller;
    controller = StreamController<List<T>>(
      onListen: () async {
        controller.add(await find());
        sub = watchChanges(db).listen((event) async {
          if (event.collections.contains(collection) && !controller.isClosed) {
            controller.add(await find());
          }
        });
      },
      onCancel: () async => sub?.cancel(),
    );
    return controller.stream;
  }

  // ── literal encoders (inline, for the multi-model functions) ────────────────

  static String _str(String s) => "'${s.replaceAll("'", "''")}'";
  static String _vec(List<double> v) => '[${v.join(', ')}]';
}
