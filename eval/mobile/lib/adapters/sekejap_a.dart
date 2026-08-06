import 'dart:convert';
import 'package:sekejap/sekejap.dart' as sk;
import '../workload.dart';
import '../bench.dart' show dirSize;

class SekejapBench implements EngineBench {
  @override
  final name = 'sekejap';
  sk.SekejapDb? _db;

  @override
  Future<void> open(String dir) async {
    _db = await sk.dbOpen(path: dir);
    // Mobile profile: Normal WAL sync + Manual compaction (no inline compact
    // during write bursts). Reclaim happens at compact-on-close.
    await sk.dbMobileProfile(db: _db!);
    // Schema + indexes once; CREATE TABLE is idempotent-guarded by catch.
    try {
      await sk.dbExecute(db: _db!, sql: '''
        CREATE TABLE docs (_key TEXT PRIMARY KEY, name TEXT,
          category TEXT, value REAL, ts INTEGER)''');
      await sk.dbExecute(db: _db!, sql: 'CREATE INDEX ON docs USING hash (category)');
      await sk.dbExecute(db: _db!, sql: 'CREATE INDEX ON docs USING btree (value)');
    } catch (_) {/* exists on reopen */}
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    // One FFI crossing, one batch, one durability barrier.
    final pairs = [
      for (final d in docs)
        (
          'docs/${d.id}',
          jsonEncode({
            '_collection': 'docs', '_key': d.id, 'name': d.name,
            'category': d.category, 'value': d.value, 'ts': d.ts,
          })
        )
    ];
    await sk.dbPutMany(db: _db!, pairs: pairs);
  }

  @override
  Future<String?> getById(String id) async {
    final s = await sk.dbGet(db: _db!, slug: 'docs/$id');
    return s == null ? null : (jsonDecode(s)['name'] as String?);
  }

  @override
  Future<int> queryCategory(String cat) async {
    final rows = jsonDecode(await sk.dbQueryParams(
        db: _db!,
        sql: 'SELECT COUNT(*) AS n FROM docs WHERE category = \$1',
        paramsJson: jsonEncode([cat]))) as List;
    return _asInt((rows.first as Map)['payload']['n']);
  }

  @override
  Future<int> queryRange(double lo, double hi) async {
    final rows = jsonDecode(await sk.dbQueryParams(
        db: _db!,
        sql: 'SELECT COUNT(*) AS n FROM docs WHERE value >= \$1 AND value <= \$2',
        paramsJson: jsonEncode([lo, hi]))) as List;
    return _asInt((rows.first as Map)['payload']['n']);
  }

  @override
  Future<void> update(String id, double v) => sk.dbExecuteParams(
      db: _db!,
      sql: 'UPDATE docs SET value = \$1 WHERE _key = \$2',
      paramsJson: jsonEncode([v, id]));

  @override
  Future<void> deleteById(String id) => sk.dbExecuteParams(
      db: _db!,
      sql: 'DELETE FROM docs WHERE _key = \$1',
      paramsJson: jsonEncode([id]));

  @override
  Future<void> close() async {
    if (_db != null) { await sk.dbCompact(db: _db!); }
    _db = null;
  }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);

  int _asInt(dynamic v) {
    if (v is int) return v;
    if (v is num) return v.toInt();
    if (v is String) return int.tryParse(v) ?? 0;
    return 0;
  }
}
