import 'package:hive/hive.dart';
import '../workload.dart';
import '../bench.dart' show dirSize;

class HiveBench implements EngineBench {
  @override
  final name = 'hive';
  Box<Map>? _box;

  @override
  Future<void> open(String dir) async {
    Hive.init(dir);
    _box = await Hive.openBox<Map>('docs');
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    await _box!.putAll({
      for (final d in docs)
        d.id: {'name': d.name, 'category': d.category, 'value': d.value, 'ts': d.ts}
    });
  }

  @override
  Future<String?> getById(String id) async => _box!.get(id)?['name'] as String?;

  // Hive has no secondary indexes: category/range queries are full scans.
  @override
  Future<int> queryCategory(String cat) async =>
      _box!.values.where((m) => m['category'] == cat).length;

  @override
  Future<int> queryRange(double lo, double hi) async => _box!.values
      .where((m) => (m['value'] as double) >= lo && (m['value'] as double) <= hi)
      .length;

  @override
  Future<void> update(String id, double v) async {
    final m = Map<dynamic, dynamic>.from(_box!.get(id)!);
    m['value'] = v;
    await _box!.put(id, m);
  }

  @override
  Future<void> deleteById(String id) => _box!.delete(id);

  @override
  Future<void> close() async { await _box?.close(); await Hive.close(); _box = null; }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);
}
