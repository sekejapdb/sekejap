import 'package:sqflite/sqflite.dart';
import '../workload.dart';
import '../bench.dart' show dirSize;

class SqfliteBench implements EngineBench {
  @override
  final name = 'sqflite';
  Database? _db;

  @override
  Future<void> open(String dir) async {
    _db = await openDatabase('$dir/bench.db', version: 1,
        onCreate: (db, _) async {
      await db.execute('''CREATE TABLE docs (key TEXT PRIMARY KEY, name TEXT,
          category TEXT, value REAL, ts INTEGER)''');
      await db.execute('CREATE INDEX idx_cat ON docs (category)');
      await db.execute('CREATE INDEX idx_val ON docs (value)');
    });
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    final batch = _db!.batch();
    for (final d in docs) {
      batch.insert('docs', {
        'key': d.id, 'name': d.name, 'category': d.category,
        'value': d.value, 'ts': d.ts,
      });
    }
    await batch.commit(noResult: true);
  }

  @override
  Future<String?> getById(String id) async {
    final r = await _db!.query('docs', where: 'key = ?', whereArgs: [id], limit: 1);
    return r.isEmpty ? null : r.first['name'] as String?;
  }

  @override
  Future<int> queryCategory(String cat) async => Sqflite.firstIntValue(
      await _db!.rawQuery('SELECT COUNT(*) FROM docs WHERE category = ?', [cat]))!;

  @override
  Future<int> queryRange(double lo, double hi) async => Sqflite.firstIntValue(
      await _db!.rawQuery('SELECT COUNT(*) FROM docs WHERE value >= ? AND value <= ?', [lo, hi]))!;

  @override
  Future<void> update(String id, double v) =>
      _db!.update('docs', {'value': v}, where: 'key = ?', whereArgs: [id]);

  @override
  Future<void> deleteById(String id) =>
      _db!.delete('docs', where: 'key = ?', whereArgs: [id]);

  @override
  Future<void> close() async { await _db?.close(); _db = null; }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);
}
